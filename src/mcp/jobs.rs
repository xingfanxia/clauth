//! Disk-backed job store for background `delegate` calls.
//!
//! A background delegate returns a `job_id` at once and finishes on a detached
//! blocking task. The result must outlive the originating tool call AND be
//! readable by a separate process (the `mcp-await-job` PostToolUse hook), so it
//! lands on disk at `~/.clauth/jobs/<job_id>.json` rather than an in-memory
//! registry. Writes are atomic (tmp + rename) so a concurrent reader never sees a
//! torn file. No lock is taken: the path is keyed by a unique `job_id` and the
//! finalizing task is the sole writer for its own file — a leaf with no ordering
//! against the runtime/state locks.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::profile::clauth_dir;

/// Retain a `done` file this long before GC reaps it; long enough that a slow
/// poller can still collect a result the auto-delivery hook already delivered.
const DONE_TTL_MS: u64 = 60 * 60 * 1000; // 1h
/// A `running` file older than this is orphaned (its server died mid-job); reap
/// it. Sits above the max delegate timeout plus slack.
const RUNNING_TTL_MS: u64 = (3600 + 600) * 1000;
/// Hard cap on retained job files; newest kept, older reaped.
const MAX_RETAINED: usize = 256;

/// Per-process counter making two job ids minted in the same millisecond differ.
static JOB_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum JobState {
    Running,
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JobRecord {
    pub(crate) job_id: String,
    pub(crate) profile: String,
    pub(crate) state: JobState,
    pub(crate) started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) envelope: Option<serde_json::Value>,
}

pub(crate) fn jobs_dir() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("jobs"))
}

/// A fresh, process-unique, filesystem-safe job id: `started_at` (epoch ms) plus
/// a monotonic counter.
pub(crate) fn new_job_id(started_at: u64) -> String {
    let n = JOB_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("d-{started_at}-{n}")
}

/// True iff `id` is safe as a single path component (no separators, no
/// traversal). Job ids reaching `delegate_result` / `mcp-await-job` come from
/// tool input, so this guards the path join.
pub(crate) fn is_safe_job_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn job_path(job_id: &str) -> Result<PathBuf> {
    Ok(jobs_dir()?.join(format!("{job_id}.json")))
}

/// Persist a record atomically: write a sibling tmp file, then rename over the
/// final path (rename is atomic on the same filesystem, so a reader sees either
/// the old file or the fully-written new one, never a torn write).
fn write_atomic(record: &JobRecord) -> Result<()> {
    let dir = jobs_dir()?;
    std::fs::create_dir_all(&dir)?;
    let bytes = serde_json::to_vec(record)?;
    let tmp_path = dir.join(format!("{}.json.tmp", record.job_id));
    std::fs::write(&tmp_path, &bytes)?;
    std::fs::rename(&tmp_path, dir.join(format!("{}.json", record.job_id)))?;
    Ok(())
}

/// Write the initial `running` record for a freshly-started background job.
pub(crate) fn write_running(job_id: &str, profile: &str, started_at: u64) -> Result<()> {
    write_atomic(&JobRecord {
        job_id: job_id.to_string(),
        profile: profile.to_string(),
        state: JobState::Running,
        started_at,
        envelope: None,
    })
}

/// Finalize a job: overwrite its file with the completed envelope.
pub(crate) fn write_done(
    job_id: &str,
    profile: &str,
    started_at: u64,
    envelope: serde_json::Value,
) -> Result<()> {
    write_atomic(&JobRecord {
        job_id: job_id.to_string(),
        profile: profile.to_string(),
        state: JobState::Done,
        started_at,
        envelope: Some(envelope),
    })
}

/// Read a job record, or `None` if the file is absent or unparseable.
pub(crate) fn read(job_id: &str) -> Option<JobRecord> {
    let bytes = std::fs::read(job_path(job_id).ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Delete a job file (best-effort). Called after a fallback `delegate_result`
/// hands the envelope back.
pub(crate) fn remove(job_id: &str) {
    if let Ok(path) = job_path(job_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Best-effort GC at server startup: drop `done` files past their TTL and
/// `running` files older than the max delegate lifetime (orphaned by a dead
/// server), sweep stray `.tmp` from a crash mid-write, then cap the retained
/// count to the newest [`MAX_RETAINED`].
pub(crate) fn gc(now: u64) {
    let Ok(dir) = jobs_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut kept: Vec<(u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            let _ = std::fs::remove_file(&path); // stray tmp / foreign file
            continue;
        }
        let record = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<JobRecord>(&b).ok());
        let Some(record) = record else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let age = now.saturating_sub(record.started_at);
        let expired = match record.state {
            JobState::Done => age > DONE_TTL_MS,
            JobState::Running => age > RUNNING_TTL_MS,
        };
        if expired {
            let _ = std::fs::remove_file(&path);
        } else {
            kept.push((record.started_at, path));
        }
    }
    if kept.len() > MAX_RETAINED {
        kept.sort_by_key(|k| std::cmp::Reverse(k.0)); // newest first
        for (_, path) in kept.into_iter().skip(MAX_RETAINED) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
#[path = "../../tests/inline/mcp_jobs.rs"]
mod tests;
