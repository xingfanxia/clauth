//! `clauth sessions [--json]`, `clauth resume <id|latest> [--profile <name>]`,
//! and `clauth info <id|latest>` — the CLI surface over the session index
//! ([`crate::sessions`]). The index owns the heavy work (transcript walk,
//! preview redaction, token/cost annotation, owner stamping); this module only
//! flattens it, renders it, and drives the account-aware resume spawn.
//!
//! # Exit codes (the `clauth sessions` scripting contract)
//! - `0` success.
//! - `1` a genuine error, INCLUDING "no sessions found".
//! - `2` a usage error (bad flag/args).
//!
//! `1` vs `2` is carried by [`crate::UsageError`] and mapped in
//! [`crate::exit_code`]: a `sessions`/`resume`/`info` dispatch arm returns a
//! `UsageError` for a malformed invocation, and any other `Err` (an empty index
//! included) maps to `1`.

use std::io::IsTerminal as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::profile::{AppConfig, load_config};
use crate::runtime::Isolation;
use crate::sessions::{SessionInfo, WorkspaceGroup};

/// `clauth sessions [--json]` — the full inventory, newest-first. Both a TTY and
/// a pipe print a table (the `--json` flag, not the tty, selects machine output;
/// this is deliberately NOT showagent's pipe-prints-different behavior). An empty
/// index is exit 1 ("no sessions found") on both paths, per the scripting
/// contract above.
pub(crate) fn run_sessions(json: bool) -> Result<()> {
    let mut groups = crate::sessions::build_index();
    // A cold price cache prices nothing (blank cost), never blocks the listing.
    let price = crate::pricing::load_cached();
    crate::sessions::annotate_all(&mut groups, price.as_ref());
    crate::sessions::annotate_owners(&mut groups);

    let flat = flatten_newest_first(&groups);
    if flat.is_empty() {
        anyhow::bail!("no sessions found");
    }
    if json {
        println!("{}", sessions_json(&flat));
    } else {
        emit_sessions_table(&groups);
    }
    Ok(())
}

/// `clauth resume <id|latest> [--profile <name>]` — resume a session through the
/// existing `clauth start` spawn path (runtime prep, signal forwarding, lifetime
/// guard), with `--resume <id>` injected and the session's recorded workspace as
/// the child cwd. Never a second spawn implementation. `latest` = the newest
/// session across the whole index; any other value is an exact id match.
pub(crate) fn run_resume(target: &str, profile_flag: Option<&str>) -> Result<()> {
    crate::platform::init();
    crate::runtime::gc_stale_runtimes();
    let config = load_config()?;

    let mut groups = crate::sessions::build_index();
    // Owners drive the interactive profile default; annotate before resolving.
    crate::sessions::annotate_owners(&mut groups);

    // Extract owned values so the `groups` borrow doesn't outlive the resolve.
    let (id, workspace_str, last_ran) = {
        let session = resolve_session(&groups, target)
            .ok_or_else(|| anyhow::anyhow!("no session found for '{target}'"))?;
        (
            session.id.clone(),
            session.workspace.clone(),
            session.last_ran_profile.clone(),
        )
    };

    // Resume must land in the recorded workspace, else `--resume` would run in
    // the wrong dir (or fail to find the transcript). Refuse rather than spawn.
    if workspace_str.is_empty() {
        anyhow::bail!("session '{id}' has no recorded workspace; cannot resume");
    }
    let workspace = Path::new(&workspace_str);
    if !workspace.is_dir() {
        anyhow::bail!("session '{id}' workspace '{workspace_str}' no longer exists; cannot resume");
    }

    let active = config.state.active_profile.as_deref().unwrap_or_default();
    let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let (default_profile, should_prompt) =
        resume_profile_choice(profile_flag, is_tty, last_ran.as_deref(), active);
    let chosen = if should_prompt {
        prompt_profile(&config, &default_profile)?
    } else {
        default_profile
    };

    let canonical = resolve_profile_name(&config, &chosen)?;

    let resume_args = vec!["--resume".to_string(), id];
    // Shared isolation: a resume adopts the chosen account against the shared
    // store, the same lifecycle a bare `clauth start <name>` uses.
    crate::start::run(
        &config,
        &canonical,
        &resume_args,
        Isolation::Shared,
        Some(workspace),
    )
}

/// `clauth info <id|latest>` — print the exact `clauth resume` command, the
/// workspace, and the on-disk storage path. Never launches anything.
pub(crate) fn run_info(target: &str) -> Result<()> {
    let groups = crate::sessions::build_index();
    let session = resolve_session(&groups, target)
        .ok_or_else(|| anyhow::anyhow!("no session found for '{target}'"))?;
    println!("resume:    clauth resume {}", session.id);
    println!("workspace: {}", session.workspace);
    println!("storage:   {}", session.path().display());
    Ok(())
}

/// Pick the resume profile default and whether to prompt for it, across the four
/// branches:
/// 1. explicit `--profile` → that profile, forced (never prompt).
/// 2. piped/non-TTY, no flag → the active profile, forced (can't prompt).
/// 3. TTY, no flag, known last-ran → prompt, defaulting to the last-ran profile.
/// 4. TTY, no flag, unknown last-ran → prompt, defaulting to the active profile.
///
/// Pure and returns `(default_profile, should_prompt)` so the four branches are
/// unit-testable without a terminal.
fn resume_profile_choice(
    flag: Option<&str>,
    is_tty: bool,
    last_ran: Option<&str>,
    active: &str,
) -> (String, bool) {
    if let Some(explicit) = flag {
        return (explicit.to_string(), false);
    }
    if !is_tty {
        return (active.to_string(), false);
    }
    match last_ran {
        Some(p) => (p.to_string(), true),
        None => (active.to_string(), true),
    }
}

/// Resolve a chosen profile name to its canonical spelling, or an error listing
/// the available names — mirrors `main::resolve_or_bail`.
fn resolve_profile_name(config: &AppConfig, chosen: &str) -> Result<String> {
    config.canonical_name(chosen).ok_or_else(|| {
        let available = config.names().join(", ");
        anyhow::anyhow!("profile '{chosen}' not found\navailable: {available}")
    })
}

/// Interactive profile prompt: list the profiles (the default marked), read a
/// line, and take the default on empty input. TTY-only — reached only when
/// [`resume_profile_choice`] returns `should_prompt`.
fn prompt_profile(config: &AppConfig, default: &str) -> Result<String> {
    use std::io::Write as _;
    println!("Resume under which profile?");
    for name in config.names() {
        let marker = if name == default { "  (default)" } else { "" };
        println!("  {name}{marker}");
    }
    print!("profile [{default}]: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let picked = line.trim();
    Ok(if picked.is_empty() {
        default.to_string()
    } else {
        picked.to_string()
    })
}

/// Flatten every group's sessions into one newest-first list. Groups are already
/// newest-first, but a flat cross-workspace order needs the same key
/// (`updated` desc, id asc) as [`crate::sessions`]'s within-group sort.
fn flatten_newest_first(groups: &[WorkspaceGroup]) -> Vec<&SessionInfo> {
    let mut all: Vec<&SessionInfo> = groups.iter().flat_map(|g| g.sessions.iter()).collect();
    all.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
    all
}

/// Resolve `latest` to the newest session, or any other value to an exact id
/// match. `None` when the index is empty or the id is unknown.
fn resolve_session<'a>(groups: &'a [WorkspaceGroup], target: &str) -> Option<&'a SessionInfo> {
    let flat = flatten_newest_first(groups);
    if target == "latest" {
        flat.into_iter().next()
    } else {
        flat.into_iter().find(|s| s.id == target)
    }
}

/// The stable `clauth sessions --json` array (newest-first). Documented fields
/// only: `id`, `last_ran_profile`, `workspace`, `updated`, `first_message`,
/// `last_message`, `tokens`, `cost`. Absent `tokens`/`cost` serialize to JSON
/// `null` (never `0`); `updated` is ISO-8601 UTC (`YYYY-MM-DDTHH:MM:SS+00:00`),
/// matching the rest of clauth's timestamps.
fn sessions_json(sessions: &[&SessionInfo]) -> serde_json::Value {
    serde_json::Value::Array(sessions.iter().map(|s| session_json_row(s)).collect())
}

fn session_json_row(s: &SessionInfo) -> serde_json::Value {
    serde_json::json!({
        "id": s.id,
        "last_ran_profile": s.last_ran_profile,
        "workspace": s.workspace,
        "updated": updated_iso(s.updated),
        "first_message": s.first_message,
        "last_message": s.last_message,
        "tokens": s.tokens,
        "cost": s.cost,
    })
}

/// Human table: a workspace header per group, then one row per session. The
/// index already redacted the previews, so nothing is masked here.
fn emit_sessions_table(groups: &[WorkspaceGroup]) {
    for group in groups {
        let ws = if group.workspace.is_empty() {
            "(unknown workspace)"
        } else {
            &group.workspace
        };
        println!("{ws}");
        for s in &group.sessions {
            println!(
                "  {id:<8}  {profile:<12}  {updated}  {tokens:>10}  {cost:>8}  {preview}",
                id = short_id(&s.id),
                profile = s.last_ran_profile.as_deref().unwrap_or("-"),
                updated = updated_iso(s.updated),
                tokens = s.tokens.map(|t| t.to_string()).unwrap_or_default(),
                cost = s.cost.map(|c| format!("${c:.2}")).unwrap_or_default(),
                preview = preview_pair(s),
            );
        }
    }
}

/// The first block of a uuid session id, enough to eyeball in the table (the
/// full id is what `clauth resume`/`info` take). A non-uuid stem shows whole.
fn short_id(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
}

/// `first | last` message preview, each bounded so a long line can't blow the
/// row width. Already-redacted text, so re-truncation is safe.
fn preview_pair(s: &SessionInfo) -> String {
    let first = crate::format::truncate(s.first_message.as_deref().unwrap_or(""), 50);
    let last = crate::format::truncate(s.last_message.as_deref().unwrap_or(""), 50);
    match (first.is_empty(), last.is_empty()) {
        (true, true) => String::new(),
        (false, true) => first,
        (true, false) => last,
        (false, false) => format!("{first} | {last}"),
    }
}

/// A file mtime as ISO-8601 UTC, reusing clauth's shared formatter so every
/// emitted timestamp reads the same. A pre-epoch time clamps to epoch 0.
fn updated_iso(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    crate::usage::epoch_secs_to_iso(secs)
}

#[cfg(test)]
#[path = "../tests/inline/sessions_cli.rs"]
mod tests;
