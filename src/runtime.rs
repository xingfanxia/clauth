//! Per-profile persistent `CLAUDE_CONFIG_DIR` used by `clauth start`.
//!
//! All `clauth start <profile>` sessions for the same profile share a
//! runtime tree at `~/.clauth/profiles/<profile>/runtime/`. Its
//! `.credentials.json` mirrors the profile's canonical creds so concurrent
//! sessions observe a single chain of refresh tokens. A watchdog thread in
//! each parent process keeps the runtime tree and canonical state in sync.
//!
//! Two transport modes, picked per profile at acquire time:
//!
//! - **Real symlinks** (Unix, plus Windows with developer mode or admin):
//!   the runtime tree is a forest of symlinks into `~/.claude/`, and
//!   `.credentials.json` is a symlink into the profile's canonical creds.
//!   The watchdog only repairs the `.credentials.json` link when Claude
//!   Code's `unlink + write` re-login replaces it with a regular file.
//!
//! - **Fake symlinks** (Windows without symlink privilege): the runtime
//!   tree is built by recursive copy, and `.credentials.json` is a regular
//!   file. The watchdog walks both sides every tick and reconciles by
//!   "latest mtime wins" so a re-login on either side propagates to the
//!   other before another session can pick up a stale refresh token.
//!
//! Reference counting lives in a sibling `sessions/` directory: each
//! session creates `sessions/<pid>` and holds an exclusive `flock(2)` on it
//! for its lifetime. New sessions prune entries whose lock is free
//! (previous holder died) and tear the runtime tree down when no live
//! sessions remain.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::claude::{build_claude_settings_json, create_symlink};
use crate::lock::with_state_lock;
use crate::profile::{Profile, atomic_write, home_dir, profile_dir};

/// Watchdog tick. 1s instead of a longer interval because fake-symlink mode
/// needs a tight upper bound on how long a session can read stale credentials
/// after a sibling refreshes — every additional second is another window in
/// which a 401 could revoke an already-rotated refresh token chain.
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkMode {
    /// OS-level symlinks. Used on Unix unconditionally and on Windows when
    /// the process can create symlinks (developer mode or admin).
    Real,
    /// Bidirectional mtime-based mirror. Used on Windows when the OS denies
    /// symlink creation.
    Fake,
}

fn runtime_dir(name: &str) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join("runtime"))
}

fn sessions_dir(name: &str) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join("sessions"))
}

fn canonical_credentials(name: &str) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join("credentials.json"))
}

/// Live-session guard. On drop: stops the watchdog, syncs a final time,
/// drops the PID file, and tears the runtime down when this was the last
/// session for the profile.
pub(crate) struct ProfileRuntime {
    name: String,
    runtime: PathBuf,
    pid_file: PathBuf,
    claude_home: PathBuf,
    mode: LinkMode,
    /// Held for the lifetime of the session so a sibling process's
    /// `try_lock` reveals we're still alive.
    _pid_lock: File,
    watchdog_signal: Option<Sender<()>>,
    watchdog_handle: Option<JoinHandle<()>>,
}

impl ProfileRuntime {
    pub(crate) fn acquire(profile: &Profile) -> Result<Self> {
        let name = profile.name.clone();
        let claude_home = home_dir()?.join(".claude");
        if !claude_home.exists() {
            anyhow::bail!("~/.claude not found; install Claude Code first");
        }
        let runtime = runtime_dir(&name)?;
        let sessions = sessions_dir(&name)?;
        let pid_file = sessions.join(std::process::id().to_string());
        let canonical = canonical_credentials(&name)?;

        let (pid_lock, mode) = with_state_lock(|| {
            std::fs::create_dir_all(&sessions)
                .with_context(|| format!("failed to create {}", sessions.display()))?;
            let active = prune_stale_sessions(&sessions)?;
            // No live siblings — rebuild the tree from scratch so stale
            // symlinks/copies to entries that have since vanished from
            // ~/.claude/ don't carry over.
            if active == 0 && runtime.exists() {
                std::fs::remove_dir_all(&runtime)
                    .with_context(|| format!("failed to clear {}", runtime.display()))?;
            }
            std::fs::create_dir_all(&runtime)
                .with_context(|| format!("failed to create {}", runtime.display()))?;
            let mode = detect_link_mode(&runtime)?;
            build_runtime_dir(&runtime, &claude_home, profile, &canonical, mode)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&pid_file)
                .with_context(|| format!("failed to open {}", pid_file.display()))?;
            file.lock()
                .with_context(|| format!("failed to lock {}", pid_file.display()))?;
            Ok::<_, anyhow::Error>((file, mode))
        })?;

        let (tx, rx) = channel::<()>();
        let watchdog_runtime = runtime.clone();
        let watchdog_canonical = canonical.clone();
        let watchdog_claude_home = claude_home.clone();
        let watchdog_handle = thread::spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(WATCHDOG_INTERVAL) {
                let _ = tick(
                    mode,
                    &watchdog_runtime,
                    &watchdog_claude_home,
                    &watchdog_canonical,
                );
            }
        });

        Ok(Self {
            name,
            runtime,
            pid_file,
            claude_home,
            mode,
            _pid_lock: pid_lock,
            watchdog_signal: Some(tx),
            watchdog_handle: Some(watchdog_handle),
        })
    }

    pub(crate) fn config_dir(&self) -> &Path {
        &self.runtime
    }
}

impl Drop for ProfileRuntime {
    fn drop(&mut self) {
        self.watchdog_signal.take();
        if let Some(h) = self.watchdog_handle.take() {
            let _ = h.join();
        }

        if let Ok(canonical) = canonical_credentials(&self.name) {
            let _ = tick(self.mode, &self.runtime, &self.claude_home, &canonical);
        }

        let _ = with_state_lock(|| {
            let _ = std::fs::remove_file(&self.pid_file);
            let sessions = sessions_dir(&self.name)?;
            let still_active = prune_stale_sessions(&sessions).unwrap_or(0);
            if still_active == 0 {
                let _ = std::fs::remove_dir_all(&self.runtime);
                let _ = std::fs::remove_dir(&sessions);
            }
            Ok::<_, anyhow::Error>(())
        });
    }
}

/// Probe the OS by attempting a real symlink in the runtime root. Anything
/// other than success — privilege denial, unsupported filesystem, the
/// `cfg(not(any(unix, windows)))` fallback — drops to fake-symlink mode.
fn detect_link_mode(runtime: &Path) -> Result<LinkMode> {
    let probe_target = runtime.join(".clauth-probe-target");
    let probe_link = runtime.join(".clauth-probe-link");
    let _ = std::fs::remove_file(&probe_target);
    let _ = std::fs::remove_file(&probe_link);
    std::fs::write(&probe_target, b"")
        .with_context(|| format!("failed to write {}", probe_target.display()))?;
    let mode = match try_real_symlink(&probe_target, &probe_link) {
        Ok(()) => LinkMode::Real,
        Err(_) => LinkMode::Fake,
    };
    let _ = std::fs::remove_file(&probe_link);
    let _ = std::fs::remove_file(&probe_target);
    Ok(mode)
}

#[cfg(unix)]
fn try_real_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn try_real_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(not(any(unix, windows)))]
fn try_real_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no symlink support",
    ))
}

/// Walk `sessions/`, drop entries whose owner has died, return the live
/// count. Caller holds the cross-process state lock so two simultaneous
/// starts can't both conclude "no other sessions" and tear down the
/// runtime under each other.
fn prune_stale_sessions(sessions: &Path) -> Result<usize> {
    let entries = match std::fs::read_dir(sessions) {
        Ok(e) => e,
        Err(_) => return Ok(0),
    };
    let mut alive = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if is_session_alive(&path) {
            alive += 1;
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(alive)
}

fn is_session_alive(pid_file: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(pid_file) else {
        return false;
    };
    // try_lock succeeds iff no other open file description holds an
    // exclusive flock — i.e. the previous owner has exited. Any I/O error
    // also surfaces as Err; we'd rather leak a session file than race a
    // live one, so Err means "treat as alive".
    file.try_lock().is_err()
}

/// Build or incrementally update the runtime tree.
///
/// Called on every `acquire`, including when `active > 0` (live siblings
/// already built the tree). The walk always runs; entries whose runtime
/// counterpart already exists are skipped, so new `~/.claude/` additions
/// after the first session's build are picked up without disturbing the rest.
///
/// Shared vs. per-profile layout:
/// - **Shared via symlink/copy across all profiles:** every top-level entry
///   in `~/.claude/` except `settings.json` and `.credentials.json` —
///   this includes `projects/`, `todos/`, `statsig/`, `sessions/`, `cache/`,
///   `commands/`, `plugins/`, `tasks/`, `teams/`, `hooks/`, `history.jsonl`,
///   `.claude.json`, and similar. Claude Code treats these as user-global
///   state so sharing is intentional; per-profile isolation would hide
///   project history and installed commands.
/// - **Per-profile:** `settings.json` (merged with profile overrides) and
///   `.credentials.json` (the profile's own OAuth token chain). These are
///   rewritten on every acquire and are never symlinked to the shared copy.
fn build_runtime_dir(
    runtime: &Path,
    claude_home: &Path,
    profile: &Profile,
    canonical: &Path,
    mode: LinkMode,
) -> Result<()> {
    // Re-walk on every acquire so entries added to ~/.claude/ after the
    // first session's build are picked up. Existing entries stay as-is.
    for entry in std::fs::read_dir(claude_home)
        .with_context(|| format!("failed to read {}", claude_home.display()))?
    {
        let entry = entry?;
        let file_name = entry.file_name();
        if file_name == "settings.json" || file_name == ".credentials.json" {
            continue;
        }
        let dst = runtime.join(&file_name);
        if dst.symlink_metadata().is_ok() {
            continue;
        }
        materialize_entry(&entry.path(), &dst, mode)?;
    }

    let settings_src = claude_home.join("settings.json");
    let merged = build_claude_settings_json(&settings_src, profile, &[])?;
    let settings_dst = runtime.join("settings.json");
    // Only write when absent or content differs — concurrent sessions on the
    // same profile each compute the same merge, so a byte-identical result
    // doesn't need to win a last-writer race and stomp a sibling's write.
    let needs_write = std::fs::read(&settings_dst)
        .map(|existing| existing != merged.as_bytes())
        .unwrap_or(true);
    if needs_write {
        atomic_write(&settings_dst, merged).context("failed to write runtime settings.json")?;
    }

    let creds_link = runtime.join(".credentials.json");
    if creds_link.symlink_metadata().is_ok() {
        let _ = std::fs::remove_file(&creds_link);
    }
    if canonical.exists() {
        install_credentials(canonical, &creds_link, mode)?;
    }

    // Share `~/.claude.json` (Claude Code's per-user state) so project
    // history stays in sync with the user's normal sessions.
    if let Some(home) = claude_home.parent() {
        let claude_json = home.join(".claude.json");
        let dst = runtime.join(".claude.json");
        if claude_json.exists() && dst.symlink_metadata().is_err() {
            materialize_entry(&claude_json, &dst, mode)?;
        }
    }

    Ok(())
}

fn materialize_entry(src: &Path, dst: &Path, mode: LinkMode) -> Result<()> {
    match mode {
        LinkMode::Real => link_entry(src, dst),
        LinkMode::Fake => copy_tree(src, dst),
    }
}

fn install_credentials(canonical: &Path, dst: &Path, mode: LinkMode) -> Result<()> {
    match mode {
        LinkMode::Real => create_symlink(canonical, dst),
        LinkMode::Fake => std::fs::copy(canonical, dst)
            .map(|_| ())
            .with_context(|| format!("failed to copy creds to {}", dst.display())),
    }
}

/// Recursive `std::fs::copy`. Directories are created at the destination
/// and walked; files are copied byte-for-byte. Used in fake-symlink mode
/// when the OS won't grant the process symlink creation rights.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let meta = src
        .symlink_metadata()
        .with_context(|| format!("failed to stat {}", src.display()))?;
    if meta.file_type().is_dir() {
        std::fs::create_dir_all(dst)
            .with_context(|| format!("failed to create {}", dst.display()))?;
        for entry in
            std::fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))?
        {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst)
            .map(|_| ())
            .with_context(|| format!("failed to copy {} -> {}", src.display(), dst.display()))
    }
}

/// One watchdog iteration. Real mode only repairs `.credentials.json`
/// (the rest of the runtime is symlinks that need no maintenance). Fake
/// mode reconciles every tree file by mtime and the credentials file too.
fn tick(mode: LinkMode, runtime: &Path, claude_home: &Path, canonical: &Path) -> Result<()> {
    match mode {
        LinkMode::Real => {
            let _ = sync_credentials(runtime, canonical)?;
            Ok(())
        }
        LinkMode::Fake => with_state_lock(|| {
            mirror_tree(claude_home, runtime)?;
            mirror_credentials(&runtime.join(".credentials.json"), canonical)?;
            Ok(())
        }),
    }
}

/// If Claude Code's internal refresh has replaced `<runtime>/.credentials.json`
/// with a regular file, copy its bytes into the profile's canonical creds
/// and swap the file back to a symlink so canonical stays the single source
/// of truth. Returns `true` when bytes were actually written. Real-symlink
/// mode only — fake-symlink mode uses [`mirror_credentials`].
pub(crate) fn sync_credentials(runtime: &Path, canonical: &Path) -> Result<bool> {
    let link_path = runtime.join(".credentials.json");
    with_state_lock(|| sync_credentials_unlocked(&link_path, canonical))
}

fn sync_credentials_unlocked(link_path: &Path, canonical: &Path) -> Result<bool> {
    let Ok(meta) = link_path.symlink_metadata() else {
        return Ok(false);
    };
    if meta.file_type().is_symlink() {
        return Ok(false);
    }
    let bytes = std::fs::read(link_path).context("failed to read live credentials")?;
    // Skip if CC's write is mid-flight (partial JSON). Next watchdog tick
    // will catch the completed write.
    if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
        return Ok(false);
    }
    let canonical_bytes = std::fs::read(canonical).ok();
    let differs = canonical_bytes.as_deref() != Some(bytes.as_slice());
    if differs {
        atomic_write(canonical, &bytes)?;
    }
    if canonical.exists() {
        let tmp = link_path.with_file_name(".credentials.json.tmp");
        let _ = std::fs::remove_file(&tmp);
        create_symlink(canonical, &tmp)?;
        std::fs::rename(&tmp, link_path)?;
    } else {
        std::fs::remove_file(link_path)?;
    }
    Ok(differs)
}

/// Bidirectional mtime-based mirror between `runtime/.credentials.json` and
/// the profile's canonical creds. "Latest mtime wins": the newer side is
/// copied over the older. Skips partial writes (invalid JSON). Used in
/// fake-symlink mode only.
fn mirror_credentials(runtime_path: &Path, canonical: &Path) -> Result<()> {
    let runtime_meta = runtime_path.metadata().ok();
    let canonical_meta = canonical.metadata().ok();

    match (runtime_meta, canonical_meta) {
        (Some(rm), Some(cm)) => {
            let rt_time = rm.modified().ok();
            let ca_time = cm.modified().ok();
            match (rt_time, ca_time) {
                (Some(rt), Some(ca)) if rt > ca => {
                    copy_if_valid_json(runtime_path, canonical)?;
                }
                (Some(rt), Some(ca)) if ca > rt => {
                    copy_if_valid_json(canonical, runtime_path)?;
                }
                _ => {}
            }
        }
        (Some(_), None) => {
            copy_if_valid_json(runtime_path, canonical)?;
        }
        (None, Some(_)) => {
            copy_if_valid_json(canonical, runtime_path)?;
        }
        (None, None) => {}
    }
    Ok(())
}

fn copy_if_valid_json(src: &Path, dst: &Path) -> Result<()> {
    let bytes = std::fs::read(src).with_context(|| format!("failed to read {}", src.display()))?;
    if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
        return Ok(());
    }
    if std::fs::read(dst).ok().as_deref() == Some(bytes.as_slice()) {
        return Ok(());
    }
    atomic_write(dst, &bytes).with_context(|| format!("failed to write {}", dst.display()))
}

/// Walk both `~/.claude/` and the runtime tree; for any file present on
/// one or both sides, copy the newer bytes onto the older. Files that exist
/// on only one side are seeded onto the other — Claude Code may create
/// state under the runtime tree (new project history, scratch files) and
/// the user may add entries under `~/.claude/` between ticks, both of
/// which must propagate. **No deletion**: a file missing from one side is
/// treated as "not yet seen", never as "intentionally removed", so a
/// content-only mirror never destroys data. Top-level `settings.json` and
/// `.credentials.json` are skipped — settings is intentionally a rewritten
/// copy and credentials has its own mirror with stricter validation.
/// Used in fake-symlink mode only.
fn mirror_tree(claude_home: &Path, runtime: &Path) -> Result<()> {
    let skip_top: HashSet<&str> = ["settings.json", ".credentials.json"].into_iter().collect();
    for name in union_children(claude_home, runtime) {
        if name.to_str().is_some_and(|n| skip_top.contains(n)) {
            continue;
        }
        merge_path(&claude_home.join(&name), &runtime.join(&name))?;
    }
    Ok(())
}

/// Unioned child-name set of two directories. Either side absent or
/// unreadable contributes an empty set. Returned names are sorted for
/// deterministic test output and stable iteration order.
fn union_children(a: &Path, b: &Path) -> Vec<std::ffi::OsString> {
    let mut names: HashSet<std::ffi::OsString> = HashSet::new();
    for dir in [a, b] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                names.insert(entry.file_name());
            }
        }
    }
    let mut out: Vec<_> = names.into_iter().collect();
    out.sort();
    out
}

/// Reconcile one path between canonical (`a`) and runtime (`b`) sides.
/// Directories recurse via the same union-walk; files merge by mtime.
fn merge_path(a: &Path, b: &Path) -> Result<()> {
    let a_meta = a.symlink_metadata().ok();
    let b_meta = b.symlink_metadata().ok();

    let a_is_dir = a_meta.as_ref().is_some_and(|m| m.file_type().is_dir());
    let b_is_dir = b_meta.as_ref().is_some_and(|m| m.file_type().is_dir());

    if a_is_dir || b_is_dir {
        if a_is_dir && !b.exists() {
            std::fs::create_dir_all(b)
                .with_context(|| format!("failed to create {}", b.display()))?;
        }
        if b_is_dir && !a.exists() {
            std::fs::create_dir_all(a)
                .with_context(|| format!("failed to create {}", a.display()))?;
        }
        for name in union_children(a, b) {
            merge_path(&a.join(&name), &b.join(&name))?;
        }
        return Ok(());
    }

    match (a_meta, b_meta) {
        (Some(am), Some(bm)) => {
            let a_time = am.modified().ok();
            let b_time = bm.modified().ok();
            if mtime_newer(a_time, b_time) {
                std::fs::copy(a, b).with_context(|| {
                    format!("failed to copy {} -> {}", a.display(), b.display())
                })?;
            } else if mtime_newer(b_time, a_time) {
                std::fs::copy(b, a).with_context(|| {
                    format!("failed to copy {} -> {}", b.display(), a.display())
                })?;
            }
        }
        (Some(_), None) => {
            std::fs::copy(a, b)
                .with_context(|| format!("failed to copy {} -> {}", a.display(), b.display()))?;
        }
        (None, Some(_)) => {
            std::fs::copy(b, a)
                .with_context(|| format!("failed to copy {} -> {}", b.display(), a.display()))?;
        }
        (None, None) => {}
    }
    Ok(())
}

fn mtime_newer(a: Option<SystemTime>, b: Option<SystemTime>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        _ => false,
    }
}

#[cfg(unix)]
fn link_entry(src: &Path, dst: &Path) -> Result<()> {
    std::os::unix::fs::symlink(src, dst)
        .with_context(|| format!("failed to symlink {} -> {}", dst.display(), src.display()))
}

#[cfg(windows)]
fn link_entry(src: &Path, dst: &Path) -> Result<()> {
    let result = if src.is_dir() {
        std::os::windows::fs::symlink_dir(src, dst)
    } else {
        std::os::windows::fs::symlink_file(src, dst)
    };
    result.with_context(|| {
        format!(
            "failed to symlink {} -> {} (enable developer mode or run as admin)",
            dst.display(),
            src.display()
        )
    })
}

#[cfg(not(any(unix, windows)))]
fn link_entry(_src: &Path, _dst: &Path) -> Result<()> {
    anyhow::bail!("clauth start requires symlink support");
}

#[cfg(test)]
#[path = "../tests/inline/runtime.rs"]
mod tests;
