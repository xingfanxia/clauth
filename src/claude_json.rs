//! Cross-profile `.claude.json` synchronizer.
//!
//! Claude Code keeps one large config file — `~/.claude.json` for normal use —
//! holding user-global state (`projects`, `mcpServers`, `tips`, `userID`, …)
//! alongside an account-specific `oauthAccount` block and a few billing/usage
//! caches. `clauth start <profile>` runs Claude Code against a per-profile
//! runtime tree with its OWN `.claude.json`, because a single shared file leaks
//! one account's identity into another: Claude Code trusts the cached
//! `oauthAccount` and does not re-derive it from the loaded token on a normal
//! startup (its bootstrap merge keeps the cached identity when the server
//! reports a different account).
//!
//! This module keeps every clauth-managed `.claude.json` (the global file plus
//! each profile runtime's copy) in sync EXCEPT for [`PER_PROFILE_FIELDS`], which
//! each file keeps as its own. Sync is "latest write wins" at file granularity:
//! each tick the newest parseable file is the source for the shared fields,
//! overlaid onto every other file while preserving that file's per-profile
//! fields. Atomic writes plus write-only-on-change make it convergent and safe
//! against Claude Code's concurrent in-place writes — a file caught mid-write
//! fails to parse and is simply skipped until the next tick.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::profile::{atomic_write, clauth_dir, home_dir};

/// Top-level keys that are account-specific and must never propagate between
/// profiles. Each `.claude.json` keeps its own values for these; the syncer
/// only ever touches the remaining (shared) fields.
const PER_PROFILE_FIELDS: &[&str] = &[
    "oauthAccount",
    "overageCreditGrantCache",
    "passesEligibilityCache",
    "passesLastSeenRemaining",
    "cachedExtraUsageDisabledReason",
];

/// Newest mtime propagated by the last [`sync_once`] that did work. A stat-only
/// tick whose newest file is no older than this short-circuits before any read,
/// parse, or write — honoring "poll by mtime, only reconcile on change".
static LAST_SYNCED: Mutex<Option<SystemTime>> = Mutex::new(None);

fn is_per_profile(key: &str) -> bool {
    PER_PROFILE_FIELDS.contains(&key)
}

/// Global `~/.claude.json` plus every profile runtime's `.claude.json`.
/// Non-existent paths are included; [`sync_paths`] filters them out on read.
fn known_paths() -> Result<Vec<PathBuf>> {
    let mut paths = vec![home_dir()?.join(".claude.json")];
    let profiles = clauth_dir()?.join("profiles");
    if let Ok(entries) = std::fs::read_dir(&profiles) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                paths.push(entry.path().join("runtime").join(".claude.json"));
            }
        }
    }
    Ok(paths)
}

/// Enumerate every known `.claude.json` and reconcile them once.
///
/// Cheap path: a stat-only sweep finds the newest mtime; if nothing has changed
/// since the last reconcile, it returns without reading any file. Only when a
/// file is newer does it fall through to [`sync_paths`]. `LAST_SYNCED` advances
/// only after a successful reconcile, so a transient error retries next tick.
pub(crate) fn sync_once() -> Result<()> {
    let paths = known_paths()?;
    let newest = paths
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok()?.modified().ok())
        .max();
    let Some(newest) = newest else {
        return Ok(());
    };
    {
        let last = LAST_SYNCED.lock().unwrap_or_else(|p| p.into_inner());
        if last.is_some_and(|l| newest <= l) {
            return Ok(());
        }
    }
    sync_paths(&paths)?;
    *LAST_SYNCED.lock().unwrap_or_else(|p| p.into_inner()) = Some(newest);
    Ok(())
}

struct Member {
    path: PathBuf,
    mtime: SystemTime,
    obj: Map<String, Value>,
}

/// Reconcile a fixed set of `.claude.json` paths once.
///
/// Reads + parses each existing file (skipping missing files and partial
/// mid-write JSON), picks the newest as the source of the shared fields, then
/// rewrites every other file as `winner.shared ∪ target.per_profile` —
/// atomically and only when the result differs. Idempotent once every file
/// agrees on the shared fields, so repeated ticks after convergence write
/// nothing.
fn sync_paths(paths: &[PathBuf]) -> Result<()> {
    let mut members: Vec<Member> = Vec::new();
    for path in paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        // A partial in-place write by Claude Code fails to parse — skip it this
        // tick rather than treat a truncated file as authoritative.
        let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        members.push(Member {
            path: path.clone(),
            mtime,
            obj,
        });
    }
    if members.len() < 2 {
        return Ok(());
    }

    // Newest mtime wins; ties broken by path so the choice is deterministic.
    let winner = members
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.mtime.cmp(&b.mtime).then_with(|| a.path.cmp(&b.path)))
        .map(|(i, _)| i)
        .expect("members is non-empty");

    let shared: Map<String, Value> = members[winner]
        .obj
        .iter()
        .filter(|(k, _)| !is_per_profile(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    for (i, member) in members.iter().enumerate() {
        if i == winner {
            continue;
        }
        // Start from the target so its key order and per-profile fields are
        // preserved; drop shared keys the winner no longer has, then upsert the
        // winner's shared values (preserve_order keeps existing positions).
        let mut merged = member.obj.clone();
        merged.retain(|k, _| is_per_profile(k) || shared.contains_key(k));
        for (k, v) in &shared {
            merged.insert(k.clone(), v.clone());
        }
        if merged == member.obj {
            continue;
        }
        let bytes = serde_json::to_vec_pretty(&Value::Object(merged))
            .context("failed to serialize merged .claude.json")?;
        atomic_write(&member.path, &bytes)
            .with_context(|| format!("failed to write {}", member.path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/inline/claude_json.rs"]
mod tests;
