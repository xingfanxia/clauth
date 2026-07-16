//! Session-index core + redaction tests. Fixture stores live under a
//! `HomeSandbox` so the global (`~/.claude/projects`) and isolated
//! (`~/.clauth/profiles/<n>/runtime-isolated/projects`) walks stay off the real
//! tree. Every transcript file is named `<sessionId>.jsonl` because the session
//! id is keyed off the filename stem, not the head line.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::time::{Duration, SystemTime};

use serde_json::json;

use crate::testutil::{HomeSandbox, set_mtime};

fn write_jsonl(path: &Path, lines: &[String]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, lines.join("\n")).unwrap();
}

fn user_line(sid: &str, cwd: &str, text: &str) -> String {
    json!({"sessionId": sid, "cwd": cwd, "message": {"role": "user", "content": text}}).to_string()
}

fn assistant_line(sid: &str, cwd: &str, text: &str) -> String {
    json!({"sessionId": sid, "cwd": cwd,
        "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}})
    .to_string()
}

/// An assistant filler line padded to exactly `len` bytes (`len` must be >= the
/// base line length). The pad is unescaped ASCII, so the byte length is exact —
/// it places a user line precisely across a tail-chunk boundary.
fn assistant_line_of_len(sid: &str, cwd: &str, len: usize) -> String {
    let base = assistant_line(sid, cwd, "");
    let pad = len.saturating_sub(base.len());
    assistant_line(sid, cwd, &"y".repeat(pad))
}

/// A `role:user` line whose only block is a `tool_result` — carries no text, so
/// it must never surface as a first/last preview.
fn tool_result_line(sid: &str) -> String {
    json!({"sessionId": sid,
        "message": {"role": "user", "content": [{"type": "tool_result", "content": "out"}]}})
    .to_string()
}

/// An assistant usage line: carries a `message.id` (the token dedup key), a
/// model, and input/output token counts. `parse_file` requires a timestamp, so
/// one is always stamped.
fn usage_line(sid: &str, cwd: &str, msg_id: &str, model: &str, input: u64, output: u64) -> String {
    json!({
        "sessionId": sid, "cwd": cwd, "timestamp": "2026-06-11T10:30:00+00:00",
        "message": {
            "id": msg_id, "role": "assistant", "model": model,
            "usage": {
                "input_tokens": input, "output_tokens": output,
                "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0
            }
        }
    })
    .to_string()
}

/// A `PriceTable` from `(model_id, input_rate, output_rate)` rows; cache rates 0.
fn price_table(rows: &[(&str, f64, f64)]) -> crate::pricing::PriceTable {
    let mut rates = HashMap::new();
    for &(id, input, output) in rows {
        rates.insert(
            id.to_owned(),
            crate::pricing::ModelRate {
                input,
                output,
                cache_read: 0.0,
                cache_write: 0.0,
            },
        );
    }
    crate::pricing::PriceTable::from_rates(rates)
}

fn find<'a>(groups: &'a [WorkspaceGroup], id: &str) -> Option<&'a SessionInfo> {
    groups
        .iter()
        .flat_map(|g| g.sessions.iter())
        .find(|s| s.id == id)
}

/// Minimal groups carrying only the ids under test — enough to drive
/// `annotate_owners`, which reads `id` and writes `last_ran_profile` and touches
/// nothing else. Decouples the owner-store tests from `build_index`/liveness.
fn groups_of(ids: &[&str]) -> Vec<WorkspaceGroup> {
    let sessions = ids
        .iter()
        .map(|id| SessionInfo {
            id: (*id).to_owned(),
            workspace: String::new(),
            path: std::path::PathBuf::new(),
            updated: SystemTime::UNIX_EPOCH,
            first_message: None,
            last_message: None,
            source: SessionSource::Global,
            tokens: None,
            cost: None,
            last_ran_profile: None,
        })
        .collect();
    vec![WorkspaceGroup {
        workspace: String::new(),
        sessions,
    }]
}

#[test]
fn redact_secrets_masks_secret_shapes_and_keeps_context() {
    // sk- API key — whole token masked, surrounding words survive.
    assert_eq!(
        redact_secrets("prefix sk-ant-api03-ABCDEFGHIJKLMNOPQRST suffix"),
        "prefix [REDACTED] suffix"
    );
    // Bearer token — the "Bearer " marker stays, the token is masked.
    assert_eq!(
        redact_secrets("auth: Bearer abcDEF123456ghi789 done"),
        "auth: Bearer [REDACTED] done"
    );
    // JSON key/value — the key name stays visible, only the value is masked.
    assert_eq!(
        redact_secrets(r#"cfg {"api_key": "supersecretvalue"} end"#),
        r#"cfg {"api_key": "[REDACTED]"} end"#
    );
    // Bare high-entropy hex/base64 blob (>= 24 chars, mixed digit+letter).
    assert_eq!(
        redact_secrets("hash 0123456789abcdef0123456789abcdef done"),
        "hash [REDACTED] done"
    );
}

#[test]
fn redact_secrets_masks_provider_token_bypasses() {
    // GitHub token — a leading `_` is a word char, so a `\b`-anchored generic
    // blob would miss `ghp_...` entirely.
    let s = redact_secrets("token ghp_16C7e42F292c6912E7710c838347Ae178B4a here");
    assert!(
        !s.contains("ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
        "github token leaked: {s}"
    );
    assert!(s.contains("[REDACTED]"), "{s}");

    // Fine-grained GitHub PAT.
    let s = redact_secrets("pat github_pat_11ABCDE0000aBcDeFgHiJkLmNoPqRsTuVwXyZ done");
    assert!(
        !s.contains("github_pat_11ABCDE0000aBcDeFgHiJkLmNoPqRsTuVwXyZ"),
        "github pat leaked: {s}"
    );
    assert!(s.contains("[REDACTED]"), "{s}");

    // Slack bot token — dash-split, `-` is not a word char.
    let s = redact_secrets("slack xoxb-EXAMPLE-fake-slack-token end");
    assert!(!s.contains("xoxb-EXAMPLE"), "slack token leaked: {s}");
    assert!(s.contains("[REDACTED]"), "{s}");

    // URL credentials — password masked, host + user context kept.
    let s = redact_secrets("clone https://alice:hunter2secretpw@host.example/repo.git");
    assert!(!s.contains("hunter2secretpw"), "url password leaked: {s}");
    assert!(s.contains("host.example"), "host must survive: {s}");
    assert!(s.contains("[REDACTED]"), "{s}");

    // Bare JWT — masked as one unit.
    let s = redact_secrets(
        "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w done",
    );
    assert!(!s.contains("eyJhbGciOiJIUzI1NiJ9"), "jwt leaked: {s}");
    assert!(s.contains("[REDACTED]"), "{s}");

    // AWS access key id.
    let s = redact_secrets("aws AKIAIOSFODNN7EXAMPLE key");
    assert!(!s.contains("AKIAIOSFODNN7EXAMPLE"), "aws key leaked: {s}");
    assert!(s.contains("[REDACTED]"), "{s}");
}

#[test]
fn redact_secrets_spares_pathlike_prose() {
    // A path-ish, digit-free preview must not be over-redacted: the entropy
    // catch-all only masks a run that mixes a digit with a letter.
    let s = "see docs/gettingstartedguide/readme for setup";
    assert_eq!(redact_secrets(s), s, "path-ish prose must stay unchanged");
}

#[test]
fn build_index_redacts_preview_without_touching_source() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-sec/ssec.jsonl");
    let secret = "here is my key sk-ant-api03-ABCDEFGHIJKLMNOP and more";
    write_jsonl(&path, &[user_line("ssec", "/w/sec", secret)]);
    let before = fs::read(&path).unwrap();

    let groups = build_index();

    // The source JSONL is read-only to the index — never rewritten.
    let after = fs::read(&path).unwrap();
    assert_eq!(
        before, after,
        "source file must be byte-identical after indexing"
    );

    let info = find(&groups, "ssec").expect("session indexed");
    let preview = info.first_message.as_deref().expect("first message");
    assert!(
        preview.contains("[REDACTED]"),
        "preview redacted: {preview}"
    );
    assert!(
        !preview.contains("sk-ant-api03-ABCDEFGHIJKLMNOP"),
        "secret leaked into preview: {preview}"
    );
    assert!(
        preview.contains("here is my key"),
        "non-secret text survived: {preview}"
    );
}

#[test]
fn session_id_comes_from_file_stem_not_first_line() {
    let sb = HomeSandbox::new();
    // File named by the real session id; its first line carries a DIFFERENT
    // (parent) sessionId that a resume copy carried forward — it must not key
    // the session.
    let path = sb
        .home()
        .join(".claude/projects/-w-stem/child-session.jsonl");
    write_jsonl(
        &path,
        &[
            user_line("parent-session", "/w/stem", "head msg"),
            user_line("parent-session", "/w/stem", "tail msg"),
        ],
    );

    let groups = build_index();
    assert!(
        find(&groups, "parent-session").is_none(),
        "first-line sessionId must not key the session"
    );
    let info = find(&groups, "child-session").expect("keyed by file stem");
    assert_eq!(info.workspace, "/w/stem");
    assert_eq!(info.first_message.as_deref(), Some("head msg"));
    assert_eq!(info.last_message.as_deref(), Some("tail msg"));
}

#[test]
fn last_user_message_comes_from_the_tail_not_the_head() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-tail/stail.jsonl");
    let mut lines = vec![user_line("stail", "/w/tail", "first question")];
    // Bury the final user turn deep so a head-only read would miss it.
    for i in 0..50 {
        lines.push(assistant_line(
            "stail",
            "/w/tail",
            &format!("filler answer {i}"),
        ));
    }
    lines.push(user_line("stail", "/w/tail", "final question here"));
    write_jsonl(&path, &lines);

    let groups = build_index();
    let info = find(&groups, "stail").expect("session indexed");
    assert_eq!(info.first_message.as_deref(), Some("first question"));
    assert_eq!(info.last_message.as_deref(), Some("final question here"));
}

#[test]
fn bounded_head_and_tail_windows_recover_first_and_last_user() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-big/big-session.jsonl");

    let head = user_line("sbig", "/w/big", "the head question");
    let tail = user_line("sbig", "/w/big", "the tail question");
    let filler = assistant_line("sbig", "/w/big", &"x".repeat(900));

    let mut lines = vec![head.clone()];
    // > HEAD_MAX_BYTES of filler between head and tail: a head-only read can't
    // reach the tail, and the head cap is exercised.
    let mut mid = 0usize;
    while mid <= HEAD_MAX_BYTES as usize {
        lines.push(filler.clone());
        mid += filler.len() + 1;
    }
    lines.push(tail.clone());
    // One trailing filler line sized so `len - TAIL_CHUNK` lands INSIDE the tail
    // user line: the first 64 KiB window drops it as a partial first line,
    // forcing the tail window to grow before the tail is recovered whole.
    lines.push(assistant_line_of_len(
        "sbig",
        "/w/big",
        TAIL_CHUNK as usize - 40,
    ));
    write_jsonl(&path, &lines);

    let groups = build_index();
    let info = find(&groups, "big-session").expect("session indexed");
    assert_eq!(info.first_message.as_deref(), Some("the head question"));
    assert_eq!(info.last_message.as_deref(), Some("the tail question"));
}

#[test]
fn duplicate_session_id_collapses_to_newest_file() {
    let sb = HomeSandbox::new();
    // Same session id (== stem) copied into two project-slug dirs.
    let older = sb.home().join(".claude/projects/-w-dup-a/sdup.jsonl");
    let newer = sb.home().join(".claude/projects/-w-dup-b/sdup.jsonl");
    write_jsonl(
        &older,
        &[
            user_line("sdup", "/w/dup", "old first"),
            user_line("sdup", "/w/dup", "old last"),
        ],
    );
    write_jsonl(
        &newer,
        &[
            user_line("sdup", "/w/dup", "new first"),
            user_line("sdup", "/w/dup", "new last"),
        ],
    );
    set_mtime(&older, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
    set_mtime(&newer, SystemTime::UNIX_EPOCH + Duration::from_secs(2_000));

    let groups = build_index();
    let dups: Vec<&SessionInfo> = groups
        .iter()
        .flat_map(|g| g.sessions.iter())
        .filter(|s| s.id == "sdup")
        .collect();
    assert_eq!(dups.len(), 1, "same id across files collapses to one");
    assert_eq!(dups[0].first_message.as_deref(), Some("new first"));
    assert_eq!(dups[0].last_message.as_deref(), Some("new last"));
}

#[test]
fn duplicate_equal_mtime_breaks_tie_by_greater_path() {
    let sb = HomeSandbox::new();
    // Same session id in two slug dirs at an identical mtime — the pick must be
    // deterministic regardless of `read_dir` order.
    let a = sb.home().join(".claude/projects/-w-tie-a/stie.jsonl");
    let b = sb.home().join(".claude/projects/-w-tie-b/stie.jsonl");
    write_jsonl(&a, &[user_line("stie", "/w/tie", "from a")]);
    write_jsonl(&b, &[user_line("stie", "/w/tie", "from b")]);
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
    set_mtime(&a, when);
    set_mtime(&b, when);

    let groups = build_index();
    let hits: Vec<&SessionInfo> = groups
        .iter()
        .flat_map(|g| g.sessions.iter())
        .filter(|s| s.id == "stie")
        .collect();
    assert_eq!(hits.len(), 1, "equal-mtime duplicate collapses to one");
    // `-w-tie-b/...` > `-w-tie-a/...` lexicographically, so b wins.
    assert_eq!(hits[0].first_message.as_deref(), Some("from b"));
}

#[test]
fn build_index_covers_global_and_isolated_and_indexes_corrupt() {
    let sb = HomeSandbox::new();

    // Global session (stem == sessionId) with a tool-result turn between the two
    // user turns.
    let g = sb.home().join(".claude/projects/-w-global/sg.jsonl");
    write_jsonl(
        &g,
        &[
            user_line("sg", "/w/global", "hi global"),
            assistant_line("sg", "/w/global", "reply"),
            tool_result_line("sg"),
            user_line("sg", "/w/global", "bye global"),
        ],
    );

    // Corrupt / non-transcript file: no readable head, so it is indexed under
    // its stem with best-effort empty metadata rather than dropped — the
    // fail-soft that also keeps summary-first and oversized-head sessions.
    let bad = sb.home().join(".claude/projects/-w-global/bad.jsonl");
    fs::create_dir_all(bad.parent().unwrap()).unwrap();
    fs::write(
        &bad,
        b"{\"sessionId\":\"broken\" this is not valid json\n\xff\xfe garbage".as_slice(),
    )
    .unwrap();

    // Resume copy: one file (stem `sr`) carrying two session ids. Keyed by the
    // stem; head + tail messages regardless of the id change mid-file.
    let r = sb.home().join(".claude/projects/-w-resume/sr.jsonl");
    write_jsonl(
        &r,
        &[
            user_line("sr", "/w/resume", "resume head"),
            assistant_line("sr", "/w/resume", "reply"),
            user_line("sr2", "/w/resume", "carried forward"),
            user_line("sr2", "/w/resume", "resume tail"),
        ],
    );

    // Live isolated session in its own throwaway store.
    let iso = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated/projects/-w-iso/si.jsonl");
    write_jsonl(
        &iso,
        &[
            user_line("si", "/w/iso", "hi iso"),
            user_line("si", "/w/iso", "bye iso"),
        ],
    );
    let sessions_dir = sb.home().join(".clauth/profiles/iso/sessions-isolated");
    fs::create_dir_all(&sessions_dir).unwrap();
    let lock_file = crate::runtime::open_pid_file(&sessions_dir.join("12345")).unwrap();
    lock_file.lock().unwrap(); // held for the walk so the runtime reads as live

    // Distinct mtimes fix the newest-first order: global > resume > iso > corrupt.
    set_mtime(&g, SystemTime::UNIX_EPOCH + Duration::from_secs(3_000));
    set_mtime(&r, SystemTime::UNIX_EPOCH + Duration::from_secs(2_000));
    set_mtime(&iso, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
    set_mtime(&bad, SystemTime::UNIX_EPOCH + Duration::from_secs(500));

    let groups = build_index();
    drop(lock_file);

    // The corrupt file has no `cwd`, so it groups under the empty workspace,
    // last by mtime.
    let workspaces: Vec<&str> = groups.iter().map(|g| g.workspace.as_str()).collect();
    assert_eq!(workspaces, vec!["/w/global", "/w/resume", "/w/iso", ""]);

    let all: Vec<&SessionInfo> = groups.iter().flat_map(|g| g.sessions.iter()).collect();
    let ids: HashSet<&str> = all.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, HashSet::from(["sg", "sr", "si", "bad"]));
    assert!(
        !ids.contains("sr2"),
        "in-file id is not used; the stem keys the session"
    );
    assert!(
        !ids.contains("broken"),
        "in-file sessionId is never the key"
    );

    let sg = find(&groups, "sg").unwrap();
    assert_eq!(sg.workspace, "/w/global");
    assert_eq!(sg.source, SessionSource::Global);
    assert_eq!(sg.first_message.as_deref(), Some("hi global"));
    assert_eq!(sg.last_message.as_deref(), Some("bye global"));
    assert!(sg.tokens.is_none());
    assert!(sg.cost.is_none());
    assert!(sg.last_ran_profile.is_none());

    let sr = find(&groups, "sr").unwrap();
    assert_eq!(sr.workspace, "/w/resume");
    assert_eq!(sr.source, SessionSource::Global);
    assert_eq!(sr.first_message.as_deref(), Some("resume head"));
    assert_eq!(sr.last_message.as_deref(), Some("resume tail"));

    let si = find(&groups, "si").unwrap();
    assert_eq!(si.workspace, "/w/iso");
    assert_eq!(
        si.source,
        SessionSource::Isolated {
            profile: "iso".to_string()
        }
    );
    assert_eq!(si.first_message.as_deref(), Some("hi iso"));
    assert_eq!(si.last_message.as_deref(), Some("bye iso"));

    // Corrupt file: indexed under its stem, empty workspace, no previews.
    let bad = find(&groups, "bad").unwrap();
    assert_eq!(bad.workspace, "");
    assert!(bad.first_message.is_none());
    assert!(bad.last_message.is_none());
}

#[test]
fn annotate_sums_tokens_and_cost_across_models() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-tok/stok.jsonl");
    write_jsonl(
        &path,
        &[
            usage_line("stok", "/w/tok", "m1", "claude-opus-4-8", 1000, 500),
            usage_line("stok", "/w/tok", "m2", "claude-sonnet-4-5", 2000, 1000),
        ],
    );
    // $1 in / $2 out per million for both models.
    let table = price_table(&[
        ("claude-opus-4-8", 1e-6, 2e-6),
        ("claude-sonnet-4-5", 1e-6, 2e-6),
    ]);

    let mut groups = build_index();
    annotate_all(&mut groups, Some(&table));

    let info = find(&groups, "stok").expect("session indexed");
    // in+out across both models: (1000+500) + (2000+1000) = 4500. Cache excluded.
    assert_eq!(info.tokens, Some(4500));
    // opus 1000*1e-6 + 500*2e-6 = 0.002; sonnet 2000*1e-6 + 1000*2e-6 = 0.004.
    let cost = info.cost.expect("priced");
    assert!((cost - 0.006).abs() < 1e-9, "got {cost}");
}

#[test]
fn annotate_leaves_tokenless_session_blank() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-none/snone.jsonl");
    write_jsonl(
        &path,
        &[
            user_line("snone", "/w/none", "just chatting"),
            assistant_line("snone", "/w/none", "no usage recorded"),
        ],
    );
    let table = price_table(&[("claude-opus-4-8", 1e-6, 2e-6)]);

    let mut groups = build_index();
    annotate_all(&mut groups, Some(&table));

    let info = find(&groups, "snone").expect("session indexed");
    // No usage line ⇒ blank, never Some(0), even with a price table present.
    assert_eq!(info.tokens, None);
    assert_eq!(info.cost, None);
}

#[test]
fn annotate_unpriced_model_has_tokens_but_no_cost() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-unp/sunp.jsonl");
    write_jsonl(
        &path,
        &[usage_line("sunp", "/w/unp", "u1", "gpt-5", 700, 300)],
    );
    // Table prices only opus — gpt-5 has no matching rate.
    let table = price_table(&[("claude-opus-4-8", 1e-6, 2e-6)]);

    let mut groups = build_index();
    annotate_all(&mut groups, Some(&table));

    let info = find(&groups, "sunp").expect("session indexed");
    assert_eq!(info.tokens, Some(1000)); // 700 + 300, tokens still counted
    assert_eq!(info.cost, None); // model unpriced ⇒ None, not Some(0.0)
}

#[test]
fn annotate_dedupes_carried_forward_line_by_tok_key() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".claude/projects/-w-dup/sdupe.jsonl");
    // Same response (message.id "m1") twice — the shape a resumed or branched
    // session produces when it copies its parent's history forward. Count ONCE.
    write_jsonl(
        &path,
        &[
            usage_line("sdupe", "/w/dup", "m1", "claude-opus-4-8", 1000, 500),
            usage_line("sdupe", "/w/dup", "m1", "claude-opus-4-8", 1000, 500),
        ],
    );

    let mut groups = build_index();
    annotate_all(&mut groups, None);

    let info = find(&groups, "sdupe").expect("session indexed");
    // Single-counted: 1000 + 500, NOT doubled to 3000.
    assert_eq!(info.tokens, Some(1500));
}

// ── A3: session → last-ran-profile stamp/read ────────────────────────────────

#[test]
fn stamp_isolated_owns_all_sessions_ignoring_mtime() {
    let sb = HomeSandbox::new();
    // An isolated store is exclusive to the profile: every transcript maps to it
    // regardless of mtime, so no run window applies.
    let projects = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated/projects");
    let a = projects.join("-w-a/isoA.jsonl");
    let b = projects.join("-w-b/isoB.jsonl");
    write_jsonl(&a, &[user_line("isoA", "/w/a", "hi")]);
    write_jsonl(&b, &[user_line("isoB", "/w/b", "yo")]);
    // Far in the past: proves the mtime window is not consulted for isolated.
    let ancient = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
    set_mtime(&a, ancient);
    set_mtime(&b, ancient);

    stamp_run_sessions("iso", &projects, true, SystemTime::now());

    let mut groups = groups_of(&["isoA", "isoB"]);
    annotate_owners(&mut groups);
    assert_eq!(
        find(&groups, "isoA").unwrap().last_ran_profile.as_deref(),
        Some("iso")
    );
    assert_eq!(
        find(&groups, "isoB").unwrap().last_ran_profile.as_deref(),
        Some("iso")
    );
}

#[test]
fn stamp_shared_respects_run_window() {
    let sb = HomeSandbox::new();
    let projects = sb.home().join(".claude/projects");
    let fresh = projects.join("-w-new/freshS.jsonl");
    let stale = projects.join("-w-old/staleS.jsonl");
    write_jsonl(&fresh, &[user_line("freshS", "/w/new", "new")]);
    write_jsonl(&stale, &[user_line("staleS", "/w/old", "old")]);

    let run_start = SystemTime::now();
    // `fresh` touched during the run (>= run_start); `stale` predates it and
    // belongs to some earlier session, not this one.
    set_mtime(&fresh, run_start + Duration::from_secs(1));
    set_mtime(&stale, run_start - Duration::from_secs(60));

    stamp_run_sessions("shared", &projects, false, run_start);

    let mut groups = groups_of(&["freshS", "staleS"]);
    annotate_owners(&mut groups);
    assert_eq!(
        find(&groups, "freshS").unwrap().last_ran_profile.as_deref(),
        Some("shared")
    );
    assert_eq!(
        find(&groups, "staleS").unwrap().last_ran_profile,
        None,
        "a pre-window shared session is not this run's"
    );
}

#[test]
fn contested_shared_session_reads_back_unknown() {
    let sb = HomeSandbox::new();
    let projects = sb.home().join(".claude/projects");
    let s = projects.join("-w-c/contested.jsonl");
    write_jsonl(&s, &[user_line("contested", "/w/c", "shared work")]);

    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
    set_mtime(&s, t0);

    // Two different profiles touch the SAME shared session within their windows.
    stamp_run_sessions("A", &projects, false, t0);
    stamp_run_sessions("B", &projects, false, t0);

    let mut groups = groups_of(&["contested"]);
    annotate_owners(&mut groups);
    // Genuinely unknown: never resolves to A, never to B.
    assert_eq!(
        find(&groups, "contested").unwrap().last_ran_profile,
        None,
        "two owners must collapse to unknown, not the last writer"
    );
}

#[test]
fn annotate_owners_sets_only_known_entries() {
    let _sb = HomeSandbox::new();
    // Build the store directly: one Known, one Contested; "absent" is never
    // inserted. `atomic_write_600` creates the 0o700 `.clauth` dir as needed.
    let path = store_path().unwrap();
    let mut store = SessionProfiles::default();
    store
        .sessions
        .insert("known".into(), SessionOwner::Known("P".into()));
    store
        .sessions
        .insert("contest".into(), SessionOwner::Contested);
    save_store(&path, &store).unwrap();

    let mut groups = groups_of(&["known", "contest", "absent"]);
    annotate_owners(&mut groups);
    assert_eq!(
        find(&groups, "known").unwrap().last_ran_profile.as_deref(),
        Some("P")
    );
    assert_eq!(find(&groups, "contest").unwrap().last_ran_profile, None);
    assert_eq!(find(&groups, "absent").unwrap().last_ran_profile, None);
}
