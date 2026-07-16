//! Session index over Claude Code transcript stores.
//!
//! Builds a newest-first, workspace-grouped inventory of CC sessions across the
//! global `~/.claude/projects/` store plus every live isolated runtime's own
//! store. The cost ceiling is deliberate: a session's first and last user
//! message come from a bounded HEAD read and a seek-from-end TAIL read of each
//! JSONL, never a full-transcript parse — the token subsystem already shows a
//! full parse is too heavy to run per index build (see `docs/sessions-design.md`).
//!
//! This is the A1 foundation: the index core plus preview redaction. Later
//! passes fill the remaining [`SessionInfo`] fields — A2 the per-session
//! `tokens`/`cost` annotation, A3 the `last_ran_profile` stamp — so those fields
//! are defined now but left `None` here.

// Staged foundation: nothing in the non-test build calls this module yet. The
// A2 annotation pass and the TUI/CLI sessions surface consume it; drop this
// allow once the first of those wires `build_index` in.
#![allow(
    dead_code,
    reason = "A1 session-index foundation, wired by a later phase"
)]

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::logline::logline;
use crate::pricing::PriceTable;
use crate::profile::{atomic_write_600, claude_dir, clauth_dir};

/// Bytes read from a file's head to recover its workspace and first user
/// message. The session id comes from the filename stem, not the head, so this
/// window only has to reach the first user turn, which sits at or near the top.
const HEAD_MAX_BYTES: u64 = 256 * 1024;
/// Initial tail-read window scanned backward for the last user message.
const TAIL_CHUNK: u64 = 64 * 1024;
/// Ceiling the tail window grows to when a chunk holds no user line, bounding
/// the read on a transcript whose tail is all tool traffic.
const TAIL_MAX: u64 = 1024 * 1024;
/// Preview length cap, in characters (not bytes — truncation lands on a char
/// boundary so non-ASCII never panics).
const PREVIEW_MAX_CHARS: usize = 200;
/// Recursion cap for the `*.jsonl` walk. Subagent/workflow transcripts nest a
/// few levels under `projects/<slug>/<session>/`, so a shallow walk would miss
/// them; the cap bounds the descent and (with symlink dirs treated as files)
/// avoids cycles.
const WALK_MAX_DEPTH: usize = 8;

/// The fixed mask a secret-shaped substring is replaced with.
const MASK: &str = "[REDACTED]";

/// Which store a session's transcript lives in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionSource {
    /// The shared global store `~/.claude/projects/`.
    Global,
    /// A live isolated runtime's own throwaway store, tagged with its profile.
    Isolated { profile: String },
}

/// One indexed session.
///
/// `tokens`, `cost`, and `last_ran_profile` are populated by later passes (A2
/// token/cost annotation, A3 profile tracking) and are left `None` here. They
/// stay `Option` on purpose: a session missing from the token stats or the
/// last-ran map renders blank, never `0`/empty-string.
#[derive(Debug, Clone)]
pub(crate) struct SessionInfo {
    /// The session id — the transcript filename stem (`<sessionId>.jsonl`), the
    /// id `claude --resume <id>` resolves by. NOT the in-file `sessionId`, which
    /// a resume copy carries forward from its parent. Deliberately not redacted
    /// (a UUID); only the message previews below are.
    pub(crate) id: String,
    /// The workspace, taken from the transcript line's own `cwd` value — the
    /// authoritative source. The dashed dir-slug under `projects/` is lossy and
    /// deliberately not decoded back into a path. Deliberately not redacted: it
    /// is the grouping key and a user-chosen filesystem path; masking it would
    /// break grouping and gut the path display. Only the previews below are.
    pub(crate) workspace: String,
    /// Source file path — the tie-breaker when the same session id shows up in
    /// two stores at an equal mtime. Module-private: consumers key off `id`.
    path: PathBuf,
    /// File mtime — a cheap freshness key that needs no parse.
    pub(crate) updated: SystemTime,
    /// First user message, redacted preview (`None` when the head held none).
    pub(crate) first_message: Option<String>,
    /// Last user message, redacted preview (`None` when the tail held none).
    pub(crate) last_message: Option<String>,
    /// Which store the transcript came from.
    pub(crate) source: SessionSource,
    /// Per-session token total — A2 fills this; `None` = absent from stats.
    pub(crate) tokens: Option<u64>,
    /// API-equivalent cost in USD — A2 fills this; `None` = unpriced/absent.
    pub(crate) cost: Option<f64>,
    /// Profile the session last ran under — A3 fills this; `None` = unknown.
    pub(crate) last_ran_profile: Option<String>,
}

/// Sessions that share one workspace (`cwd`), newest-first within the group.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceGroup {
    pub(crate) workspace: String,
    pub(crate) sessions: Vec<SessionInfo>,
}

/// Compile a static redaction pattern. Each pattern is a compile-time constant,
/// so an `Err` is a code bug the module's own tests catch, never a runtime path.
#[allow(
    clippy::expect_used,
    reason = "static redaction pattern is a valid regex"
)]
fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid redaction regex")
}

// Layered preview redaction. Ordering is load-bearing: the precise provider
// rules (A) and the key/value rules (B) run first and drop `[REDACTED]` in
// place; the generic entropy catch-all (C) runs last over whatever survives.
// The mask holds `[` `]`, both outside Layer C's class, so C never re-touches an
// A/B mask. Err toward over-redaction: a false positive is cosmetic, a leaked
// key is not.

// --- Layer A: explicit high-confidence provider/token shapes ---
// Anthropic / OpenAI secret key.
static SK_KEY: LazyLock<Regex> = LazyLock::new(|| compile(r"\bsk-[A-Za-z0-9_-]{8,}"));
// GitHub token (`ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_`) and fine-grained PAT. A
// leading `_` is a word char, so a `\b`-anchored generic blob misses these.
static GITHUB_TOKEN: LazyLock<Regex> = LazyLock::new(|| compile(r"\bgh[pousr]_[A-Za-z0-9]{20,}"));
static GITHUB_PAT: LazyLock<Regex> = LazyLock::new(|| compile(r"\bgithub_pat_[A-Za-z0-9_]{20,}"));
// Slack token — dash-split, and `-` is not a word char.
static SLACK_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?i)\bxox[baprs]-[A-Za-z0-9-]{10,}"));
// Google API key.
static GOOGLE_API: LazyLock<Regex> = LazyLock::new(|| compile(r"\bAIza[A-Za-z0-9_-]{10,}"));
// AWS access key id.
static AWS_AKID: LazyLock<Regex> = LazyLock::new(|| compile(r"\bAKIA[0-9A-Z]{16}\b"));
// JWT — masked as one unit so a dot-split never leaves two live halves.
static JWT: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+"));
// `Bearer <token>` — keep the marker (group 1), mask the token.
static BEARER: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=-]{8,}"));
// URL credentials `scheme://user:pass@host` — keep user + host, mask the password.
static URL_CREDS: LazyLock<Regex> = LazyLock::new(|| compile(r"(://[^\s:/@]+:)([^\s/@]+)(@)"));

// --- Layer B: key/value pairs — keep the key, mask the value (group 2) ---
static KV_JSON: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?i)("[a-z0-9_.-]*(?:token|secret|password|api[_-]?key|authorization)[a-z0-9_.-]*"\s*:\s*")([^"]*)(")"#,
    )
});
static KV_ENV: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?i)\b([a-z0-9_.-]*(?:token|secret|password|api[_-]?key|authorization)[a-z0-9_.-]*\s*=\s*)([^\s"']+)"#,
    )
});

// --- Layer C: generic high-entropy catch-all, filtered in a closure ---
// `regex` has no lookahead, so the entropy test runs per match inside the
// `replace_all` closure, not as a pattern. The class omits `.` and whitespace so
// a run can't span sentence or path-dot boundaries.
static ENTROPY_BLOB: LazyLock<Regex> = LazyLock::new(|| compile(r"[A-Za-z0-9+/=_-]{24,}"));

/// A generic run "looks secret" when it clears the length floor and mixes at
/// least one digit with one letter — sparing pure-word path segments
/// (`gettingstartedguide`) and pure-number runs, while still catching random
/// tokens, url-safe base64, and git SHAs.
fn looks_secret(run: &str) -> bool {
    run.len() >= 24
        && run.bytes().any(|b| b.is_ascii_digit())
        && run.bytes().any(|b| b.is_ascii_alphabetic())
}

/// Mask secret-shaped substrings in preview text. Applied to the in-memory
/// preview only; the source JSONL is never touched, so redaction is one-way at
/// the render boundary.
pub(crate) fn redact_secrets(s: &str) -> String {
    // Layer A: precise provider/token shapes (whole match, or keep a marker).
    let mut out = SK_KEY.replace_all(s, MASK).into_owned();
    out = GITHUB_TOKEN.replace_all(&out, MASK).into_owned();
    out = GITHUB_PAT.replace_all(&out, MASK).into_owned();
    out = SLACK_TOKEN.replace_all(&out, MASK).into_owned();
    out = GOOGLE_API.replace_all(&out, MASK).into_owned();
    out = AWS_AKID.replace_all(&out, MASK).into_owned();
    out = JWT.replace_all(&out, MASK).into_owned();
    out = BEARER.replace_all(&out, "${1}[REDACTED]").into_owned();
    out = URL_CREDS
        .replace_all(&out, "${1}[REDACTED]${3}")
        .into_owned();

    // Layer B: recognizable key/value pairs — key stays, value masked.
    out = KV_JSON.replace_all(&out, "${1}[REDACTED]${3}").into_owned();
    out = KV_ENV.replace_all(&out, "${1}[REDACTED]").into_owned();

    // Layer C: entropy catch-all over what survived, filtered so file paths and
    // pure-word identifiers pass through unmasked.
    out = ENTROPY_BLOB
        .replace_all(&out, |caps: &regex::Captures| {
            let run = &caps[0];
            if looks_secret(run) {
                MASK.to_string()
            } else {
                run.to_string()
            }
        })
        .into_owned();
    out
}

/// A transcript line, decoded just far enough for the index. Unknown fields are
/// ignored, so tool-use / summary / meta lines parse without error and simply
/// yield no user text.
#[derive(Deserialize)]
struct TranscriptLine {
    cwd: Option<String>,
    #[allow(dead_code)]
    timestamp: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    role: Option<String>,
    content: Option<Content>,
}

/// User `content` is either a plain string or an array of typed blocks; a
/// catch-all keeps an unexpected shape from failing the whole line.
#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
    // Only consumes an unexpected shape so the whole line still parses; its
    // value is intentionally never read.
    #[allow(dead_code)]
    Other(serde_json::Value),
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}

/// The redacted preview of a line's user text, or `None` when the line is not a
/// user turn or carries no text (e.g. a `tool_result`-only line).
fn user_text(line: &TranscriptLine) -> Option<String> {
    let msg = line.message.as_ref()?;
    if msg.role.as_deref() != Some("user") {
        return None;
    }
    let raw = match msg.content.as_ref()? {
        Content::Text(s) => s.clone(),
        Content::Blocks(blocks) => blocks
            .iter()
            .filter(|b| b.kind.as_deref() == Some("text"))
            .filter_map(|b| b.text.as_deref())
            .collect::<Vec<_>>()
            .join(""),
        Content::Other(_) => return None,
    };
    preview_of(&raw)
}

/// Redact then truncate to a bounded, char-boundary-safe preview. Redaction runs
/// on the full text first so a secret can never survive by straddling the
/// truncation point.
fn preview_of(raw: &str) -> Option<String> {
    let redacted = redact_secrets(raw.trim());
    let trimmed = redacted.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_chars(trimmed, PREVIEW_MAX_CHARS))
}

/// Truncate to at most `max` characters, appending an ellipsis when cut.
/// `char_indices().nth(max)` yields a valid UTF-8 boundary, so a multi-byte
/// character is never split.
fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => {
            let mut out = s[..idx].to_string();
            out.push('…');
            out
        }
        None => s.to_string(),
    }
}

/// Head metadata recovered from a transcript's first lines. The session id is
/// keyed off the filename, not the head, so it is absent here.
#[derive(Default)]
struct Head {
    workspace: String,
    first_message: Option<String>,
}

/// Read a bounded head window for the workspace (`cwd`) and first user message.
/// Best-effort: an unreadable file, or a head carrying neither, yields an empty
/// workspace / `None` message rather than dropping the session — its id comes
/// from the filename, so a summary-first or oversized head is still indexed.
fn read_head(path: &Path) -> Head {
    let Ok(file) = File::open(path) else {
        return Head::default();
    };
    let reader = BufReader::new(file.take(HEAD_MAX_BYTES));
    let mut cwd: Option<String> = None;
    let mut first_message: Option<String> = None;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(parsed) = serde_json::from_str::<TranscriptLine>(&line) else {
            continue;
        };
        if cwd.is_none()
            && let Some(c) = parsed.cwd.as_deref().filter(|c| !c.is_empty())
        {
            cwd = Some(c.to_string());
        }
        if first_message.is_none() {
            first_message = user_text(&parsed);
        }
        if cwd.is_some() && first_message.is_some() {
            break;
        }
    }
    Head {
        workspace: cwd.unwrap_or_default(),
        first_message,
    }
}

/// The last user message, found by seeking from the end and scanning a bounded
/// tail window backward — never a full parse. The window grows up to [`TAIL_MAX`]
/// if a chunk holds only tool traffic. Fail-soft: any IO error yields `None`.
fn read_last_user_message(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    let mut window = TAIL_CHUNK;
    loop {
        let read_len = window.min(len);
        file.seek(SeekFrom::Start(len - read_len)).ok()?;
        let mut buf = Vec::with_capacity(read_len as usize);
        file.by_ref().take(read_len).read_to_end(&mut buf).ok()?;

        // Unless the window starts at byte 0, its first line is a partial cut —
        // drop up to the first newline so every scanned line is whole.
        let slice: &[u8] = if read_len < len {
            match buf.iter().position(|&b| b == b'\n') {
                Some(i) => &buf[i + 1..],
                None => &buf[..],
            }
        } else {
            &buf[..]
        };

        if let Some(msg) = last_user_in_slice(slice) {
            return Some(msg);
        }
        // Whole file already covered, or the window hit its ceiling: give up.
        if read_len >= len || window >= TAIL_MAX {
            return None;
        }
        window = (window * 2).min(TAIL_MAX);
    }
}

/// Scan `slice`'s lines back-to-front, returning the first (i.e. latest) user
/// message text.
fn last_user_in_slice(slice: &[u8]) -> Option<String> {
    for line in slice.split(|&b| b == b'\n').rev() {
        if line.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(line) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<TranscriptLine>(text) else {
            continue;
        };
        if let Some(msg) = user_text(&parsed) {
            return Some(msg);
        }
    }
    None
}

/// The session id: the transcript filename stem. CC names each transcript
/// `<sessionId>.jsonl` and `--resume <id>` resolves by that stem, so it is the
/// authoritative id — unlike the in-file `sessionId`, which a resume copy
/// carries forward from its parent. Should CC ever emit a `<id>.summary.jsonl`,
/// the stem `<id>.summary` is taken verbatim; plain `<uuid>.jsonl` is the norm.
fn session_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    if stem.is_empty() {
        return None;
    }
    Some(stem.to_string())
}

/// Index one file into a [`SessionInfo`], or `None` when it has no usable
/// filename stem or its metadata can't be read. Head metadata is best-effort.
fn scan_file(path: &Path, source: &SessionSource) -> Option<SessionInfo> {
    let id = session_id_from_path(path)?;
    let updated = std::fs::metadata(path).ok()?.modified().ok()?;
    let head = read_head(path);
    Some(SessionInfo {
        id,
        workspace: head.workspace,
        path: path.to_path_buf(),
        updated,
        first_message: head.first_message,
        last_message: read_last_user_message(path),
        source: source.clone(),
        tokens: None,
        cost: None,
        last_ran_profile: None,
    })
}

/// Recursively collect `*.jsonl` paths under `dir` (depth-capped). A symlinked
/// directory is treated as a file and never descended, bounding the walk.
fn collect_jsonl(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            collect_jsonl(&path, depth - 1, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

/// Index every `*.jsonl` under one store's `projects/` dir into `by_id`, keeping
/// the newest entry when a session id appears in more than one file.
fn index_store(projects: &Path, source: &SessionSource, by_id: &mut HashMap<String, SessionInfo>) {
    let mut paths = Vec::new();
    collect_jsonl(projects, WALK_MAX_DEPTH, &mut paths);
    for path in paths {
        if let Some(info) = scan_file(&path, source) {
            insert_newest(by_id, info);
        }
    }
}

/// Collapse a duplicate session id (the same `<id>.jsonl` copied into more than
/// one store or project-slug dir) to the newest by mtime. On an equal mtime the
/// lexicographically greater source path wins, so the pick stays stable
/// regardless of `read_dir` order.
fn insert_newest(map: &mut HashMap<String, SessionInfo>, info: SessionInfo) {
    match map.entry(info.id.clone()) {
        Entry::Occupied(mut e) => {
            let cur = e.get();
            let wins =
                info.updated > cur.updated || (info.updated == cur.updated && info.path > cur.path);
            if wins {
                e.insert(info);
            }
        }
        Entry::Vacant(e) => {
            e.insert(info);
        }
    }
}

/// Group sessions by workspace, newest-first within each group and groups
/// ordered by their newest session. The session id is the stable tie-breaker so
/// equal mtimes still order deterministically.
fn group_by_workspace(sessions: Vec<SessionInfo>) -> Vec<WorkspaceGroup> {
    let mut groups: HashMap<String, Vec<SessionInfo>> = HashMap::new();
    for s in sessions {
        groups.entry(s.workspace.clone()).or_default().push(s);
    }
    let mut out: Vec<WorkspaceGroup> = groups
        .into_iter()
        .map(|(workspace, mut sessions)| {
            sessions.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
            WorkspaceGroup {
                workspace,
                sessions,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        let an = a.sessions.first().map(|s| s.updated);
        let bn = b.sessions.first().map(|s| s.updated);
        bn.cmp(&an).then_with(|| a.workspace.cmp(&b.workspace))
    });
    out
}

/// Build the session index: the global store plus every live isolated runtime's
/// own store, deduped by session id and grouped by workspace, newest-first.
/// Fail-soft throughout — an unreadable file or store is skipped, never fatal.
pub(crate) fn build_index() -> Vec<WorkspaceGroup> {
    let mut by_id: HashMap<String, SessionInfo> = HashMap::new();

    if let Ok(projects) = claude_dir().map(|d| d.join("projects")) {
        index_store(&projects, &SessionSource::Global, &mut by_id);
    }
    for (profile, projects) in crate::runtime::live_isolated_stores() {
        index_store(&projects, &SessionSource::Isolated { profile }, &mut by_id);
    }

    group_by_workspace(by_id.into_values().collect())
}

/// Annotate one session in place with its token total and API-equivalent cost —
/// the full-transcript parse [`build_index`] deliberately skips, so a caller pays
/// it only when it wants these figures. Idempotent; safe to re-run.
///
/// `tokens` is input+output summed across models (`ModelTokens::in_out` — the
/// "tokens used" basis the Tokens tab headlines; cache is excluded so a resume's
/// carried-forward cache reads don't inflate the figure). It stays `None` — never
/// `Some(0)` — when the file yields no token-bearing row, so a session with no
/// usage renders blank rather than a misleading zero.
///
/// `cost` follows [`PriceTable::total_cost`]: `Some(usd)` when a table is present
/// and at least one of the session's models has a matching rate; `None` when no
/// table is given OR every model is unpriced. The priced/unpriced boundary is read
/// from the rate table directly, not from `usd > 0`, so a priced but genuinely
/// zero-cost session is `Some(0.0)` — distinct from an unpriced `None`.
pub(crate) fn annotate(info: &mut SessionInfo, price: Option<&PriceTable>) {
    let models = crate::tokens::file_model_tokens(&info.path);
    // >= 1 token-bearing row ⇒ a real total (possibly 0); no rows ⇒ blank.
    info.tokens = (!models.is_empty()).then(|| models.iter().map(|m| m.in_out()).sum());
    info.cost = price.and_then(|p| {
        let (usd, _unpriced) = p.total_cost(&models);
        // "At least one model priced" is read off the table, not `usd > 0`, so a
        // priced zero-cost session reads `Some(0.0)` while all-unpriced reads None.
        models.iter().any(|m| p.cost(m).is_some()).then_some(usd)
    });
}

/// Annotate every session across all groups (the CLI's eager pass; the TUI may
/// instead call [`annotate`] lazily per visible row).
pub(crate) fn annotate_all(groups: &mut [WorkspaceGroup], price: Option<&PriceTable>) {
    for group in groups.iter_mut() {
        for session in group.sessions.iter_mut() {
            annotate(session, price);
        }
    }
}

// ── A3: session → last-ran-profile store ─────────────────────────────────────
//
// A single GLOBAL file under `~/.clauth/` keyed by session id (NOT per-profile —
// a shared-store session is cross-profile, so its owner can't live under any one
// profile dir). Hand-rolled load/save against `clauth_dir()`, mirroring
// `pricing.rs` / `token_ledger.rs`, since the crate has no shared global-cache
// helper.

/// Global store filename under `~/.clauth/`.
const SESSION_PROFILES_FILE: &str = "session_profiles.json";

/// Which profile a session last ran under. A stored `Contested` is distinct from
/// absent: two different profiles have both touched the same shared-store
/// session, so the owner is genuinely unknown and must never resolve to either —
/// while an absent id is simply unobserved. Both read back as "unknown".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionOwner {
    Known(String),
    Contested,
}

/// The persisted store. A named wrapper (not a bare map) leaves room to add
/// fields later without breaking the on-disk shape, matching `token_ledger.rs`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionProfiles {
    /// session id → owner stamp.
    sessions: HashMap<String, SessionOwner>,
}

/// `~/.clauth/session_profiles.json`; `None` only when the home dir can't be
/// resolved.
fn store_path() -> Option<PathBuf> {
    clauth_dir().ok().map(|d| d.join(SESSION_PROFILES_FILE))
}

/// Load the store, or an empty one when absent/unreadable/corrupt — a missing
/// owner renders blank, never fatal.
fn load_store(path: &Path) -> SessionProfiles {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Persist the store atomically (0o600).
fn save_store(path: &Path, store: &SessionProfiles) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(store).map_err(std::io::Error::other)?;
    atomic_write_600(path, &bytes)
}

/// Fold one observed session id under `profile` into the owner map. Absent or
/// already `Known(profile)` ⇒ `Known(profile)`; a different owner or a prior
/// `Contested` ⇒ `Contested`. Two profiles touching one shared session means the
/// owner can't be attributed, so it stays unknown rather than guessing the last
/// writer.
fn fold_owner(map: &mut HashMap<String, SessionOwner>, id: &str, profile: &str) {
    match map.entry(id.to_owned()) {
        Entry::Vacant(e) => {
            e.insert(SessionOwner::Known(profile.to_owned()));
        }
        Entry::Occupied(mut e) => {
            let contest = match e.get() {
                SessionOwner::Known(p) => p != profile,
                SessionOwner::Contested => true,
            };
            if contest {
                e.insert(SessionOwner::Contested);
            }
            // else: already ours — leave Known(profile) as-is.
        }
    }
}

/// Session ids this run owns: the file stems under `projects_dir`, filtered to
/// this run's window on a shared (cross-profile) store. An isolated store is
/// exclusive to the profile, so every file counts regardless of mtime.
fn run_session_ids(projects_dir: &Path, isolated: bool, run_start: SystemTime) -> Vec<String> {
    let mut paths = Vec::new();
    collect_jsonl(projects_dir, WALK_MAX_DEPTH, &mut paths);
    paths
        .into_iter()
        .filter(|p| isolated || touched_since(p, run_start))
        .filter_map(|p| session_id_from_path(&p))
        .collect()
}

/// Whether `path`'s mtime is at or after `since`. Fail-soft: an unreadable mtime
/// counts as outside the window (not this run's), so it is left unstamped.
fn touched_since(path: &Path, since: SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|mtime| mtime >= since)
        .unwrap_or(false)
}

/// Record which sessions a `clauth start` run owned into the global store.
///
/// `projects_dir` is where the run's transcripts landed: an isolated runtime's
/// exclusive `runtime-isolated/projects/` (`isolated = true` — every file maps to
/// `profile`), or the shared global `~/.claude/projects/` (`isolated = false` —
/// only files touched at or after `run_start` are attributed, catching new and
/// resumed-during-this-run sessions without claiming another profile's untouched
/// ones).
///
/// The read-modify-write runs under the state flock so two concurrent
/// `clauth start` runs fold their stamps in serially instead of clobbering each
/// other. Best-effort throughout: the session already ran, so any IO error is
/// logged and swallowed — never propagated to fail `start`.
pub(crate) fn stamp_run_sessions(
    profile: &str,
    projects_dir: &Path,
    isolated: bool,
    run_start: SystemTime,
) {
    let ids = run_session_ids(projects_dir, isolated, run_start);
    if ids.is_empty() {
        return;
    }
    let result = crate::lock::with_state_lock(|| {
        let Some(path) = store_path() else {
            return Ok(());
        };
        let mut store = load_store(&path);
        for id in &ids {
            fold_owner(&mut store.sessions, id, profile);
        }
        save_store(&path, &store)?;
        Ok(())
    });
    if let Err(e) = result {
        logline!("clauth: failed to stamp session owners: {e}");
    }
}

/// Annotate each session's `last_ran_profile` from the global owner store.
/// Loads the store once, so a caller can attach owners without paying the
/// per-session full-transcript parse [`annotate`] costs. Leaves `None` for a
/// session that is absent or `Contested` (both mean "unknown").
pub(crate) fn annotate_owners(groups: &mut [WorkspaceGroup]) {
    let Some(path) = store_path() else {
        return;
    };
    let store = load_store(&path);
    for group in groups.iter_mut() {
        for session in group.sessions.iter_mut() {
            session.last_ran_profile = match store.sessions.get(&session.id) {
                Some(SessionOwner::Known(p)) => Some(p.clone()),
                Some(SessionOwner::Contested) | None => None,
            };
        }
    }
}

#[cfg(test)]
#[path = "../tests/inline/sessions.rs"]
mod tests;
