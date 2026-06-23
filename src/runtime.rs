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
use crate::profile::{
    ClaudeCredentials, Profile, atomic_write, atomic_write_600, claude_dir, profile_subpath,
};

/// Watchdog tick. 1s instead of a longer interval because fake-symlink mode
/// needs a tight upper bound on how long a session can read stale credentials
/// after a sibling refreshes — every additional second is another window in
/// which a 401 could revoke an already-rotated refresh token chain.
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(1);

/// `.claude.json` cross-profile sync cadence. Tighter than the credential
/// watchdog because Claude Code rewrites `.claude.json` constantly; 100ms keeps
/// the window in which one profile observes another's stale shared state small.
/// Also bounds watchdog-thread shutdown latency to one tick of this interval.
const CJSON_INTERVAL: Duration = Duration::from_millis(100);

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
    profile_subpath(name, "runtime")
}

fn sessions_dir(name: &str) -> Result<PathBuf> {
    profile_subpath(name, "sessions")
}

/// True iff the profile has at least one live `clauth start` session. A missing
/// or unreadable sessions dir returns false (the profile is idle).
pub(crate) fn has_live_session(name: &str) -> bool {
    let Ok(dir) = sessions_dir(name) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    entries.flatten().any(|e| is_session_alive(&e.path()))
}

/// Count of live `clauth start` sessions for the profile. Additive sibling of
/// [`has_live_session`] (left untouched — it gates token rotation); a missing or
/// unreadable sessions dir counts as zero.
pub(crate) fn live_session_count(name: &str) -> usize {
    let Ok(dir) = sessions_dir(name) else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| is_session_alive(&e.path()))
        .count()
}

fn canonical_credentials(name: &str) -> Result<PathBuf> {
    profile_subpath(name, "credentials.json")
}

fn rotation_lock_path(name: &str) -> Result<PathBuf> {
    profile_subpath(name, "rotation.lock")
}

/// Cross-process advisory lock serializing a token rotation against a
/// `clauth start` session acquire for the SAME profile.
///
/// A refresh token is single-use: once `oauth::refresh` spends it the server
/// kills it, and a second refresh of the same token 401s and burns the whole
/// chain. The global state flock (`with_state_lock`) cannot guard this because
/// it must be released across the network round trip; the per-PID session
/// flocks only track liveness, not "a rotation is in flight". This lock is
/// held for the FULL rotate HTTP window (which `with_state_lock` cannot be),
/// and `ProfileRuntime::acquire` takes the same lock before it stamps its
/// session PID file — so the two operations are mutually exclusive:
///
/// - rotate wins the race → acquire blocks until the new pair is persisted,
///   then the session starts against the rotated token;
/// - acquire wins the race → it creates its session PID file before releasing,
///   so rotate's in-lock `has_live_session` re-check sees the live session and
///   skips (the token is never spent).
///
/// Distinct from `~/.clauth/.lock` (global state) and `sessions/<pid>`
/// (per-session liveness). Blocking `flock`; the holder window is short.
#[must_use]
pub(crate) struct RotationGuard {
    // Drops before `_rank` (declaration order): the flock releases, then the
    // ROTATION rank pops — never the reverse.
    _file: File,
    _rank: crate::lockorder::RankGuard,
}

impl RotationGuard {
    /// Acquire the per-profile rotation lock, blocking until any in-flight
    /// rotation or acquire for this profile releases it. Creates the directory
    /// if missing (a profile with no session yet has no `sessions/`).
    pub(crate) fn acquire(name: &str) -> Result<Self> {
        let path = rotation_lock_path(name)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file =
            open_pid_file(&path).with_context(|| format!("failed to open {}", path.display()))?;
        file.lock()
            .with_context(|| format!("failed to lock {}", path.display()))?;
        // ROTATION is the outermost rank — held across the OAuth HTTP round
        // trip, before `config` and the state flock are ever taken.
        let _rank = crate::lockorder::RankGuard::enter::<crate::lockorder::rank::Rotation>();
        Ok(Self { _file: file, _rank })
    }
}

/// Open or create a PID file without truncating — used for session liveness
/// tracking via flock. `O_CREAT` without truncate preserves any existing lock
/// held by a sibling that raced us to create the file.
pub(crate) fn open_pid_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Live-session guard. On drop: stops the watchdog, runs a final sync
/// (errors surface to stderr), drops the PID file, and tears the runtime
/// down when this was the last session for the profile.
pub(crate) struct ProfileRuntime {
    runtime: PathBuf,
    pid_file: PathBuf,
    claude_home: PathBuf,
    canonical: PathBuf,
    sessions: PathBuf,
    mode: LinkMode,
    /// Held for the lifetime of the session so a sibling process's
    /// `try_lock` reveals we're still alive.
    _pid_lock: File,
    /// Wrapped in Option so Drop can take() it before joining the watchdog,
    /// signalling the thread to exit.
    watchdog_signal: Option<Sender<()>>,
    watchdog_handle: Option<JoinHandle<()>>,
}

impl ProfileRuntime {
    pub(crate) fn acquire(profile: &Profile) -> Result<Self> {
        let name = &profile.name;
        let claude_home = claude_dir()?;
        if !claude_home.exists() {
            anyhow::bail!("~/.claude not found; install Claude Code first");
        }
        let runtime = runtime_dir(name)?;
        let sessions = sessions_dir(name)?;
        let pid_file = sessions.join(std::process::id().to_string());
        let canonical = canonical_credentials(name)?;

        // Hold the per-profile rotation lock across the session-stamp window so
        // a concurrent `oauth::rotate_one_inner` for this profile cannot spend the
        // single-use refresh token while we are starting up. Ordering rule
        // (matches `oauth::rotate_one_inner`): RotationGuard OUTERMOST, then the
        // state flock inside. Dropped right after the PID file is locked — from
        // then on the PID flock itself signals liveness, and rotate's in-lock
        // `has_live_session` re-check observes it.
        let _rotation_guard = RotationGuard::acquire(name)?;

        let (pid_lock, mode) = with_state_lock(|| {
            std::fs::create_dir_all(&sessions)
                .with_context(|| format!("failed to create {}", sessions.display()))?;
            let active = prune_stale_sessions(&sessions)?;
            // No live siblings — rebuild from scratch so stale symlinks/copies
            // to entries that have since vanished from ~/.claude/ don't carry over.
            if active == 0 && runtime.symlink_metadata().is_ok() {
                std::fs::remove_dir_all(&runtime)
                    .with_context(|| format!("failed to clear {}", runtime.display()))?;
            }
            std::fs::create_dir_all(&runtime)
                .with_context(|| format!("failed to create {}", runtime.display()))?;
            let mode = detect_link_mode(&runtime)?;
            build_runtime_dir(&runtime, &claude_home, profile, &canonical, mode)?;
            let file = open_pid_file(&pid_file)
                .with_context(|| format!("failed to open {}", pid_file.display()))?;
            file.lock()
                .with_context(|| format!("failed to lock {}", pid_file.display()))?;
            Ok::<_, anyhow::Error>((file, mode))
        })?;

        let (tx, rx) = channel::<()>();
        let watchdog_runtime = runtime.clone();
        let watchdog_canonical = canonical.clone();
        let watchdog_claude_home = claude_home.clone();
        #[allow(clippy::expect_used, reason = "thread spawn failure is unrecoverable")]
        let watchdog_handle = thread::Builder::new()
            .name(format!("clauth-wdog-{name}"))
            .spawn(move || {
                // One thread, two cadences: `.claude.json` reconciles every
                // CJSON_INTERVAL; credentials reconcile every ~WATCHDOG_INTERVAL,
                // counted in cjson ticks. Loop exits on Disconnected (sender
                // dropped in Drop) or Ok(()).
                let cred_every =
                    (WATCHDOG_INTERVAL.as_millis() / CJSON_INTERVAL.as_millis()).max(1);
                let mut until_cred = cred_every;
                while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(CJSON_INTERVAL) {
                    if let Err(e) = crate::claude_json::sync_once() {
                        eprintln!("clauth: .claude.json sync failed: {e}");
                    }
                    until_cred -= 1;
                    if until_cred == 0 {
                        until_cred = cred_every;
                        if let Err(e) = tick(
                            mode,
                            &watchdog_runtime,
                            &watchdog_claude_home,
                            &watchdog_canonical,
                        ) {
                            eprintln!("clauth: watchdog tick failed: {e}");
                        }
                    }
                }
            })
            .expect("failed to spawn watchdog thread");

        Ok(Self {
            runtime,
            pid_file,
            claude_home,
            canonical,
            sessions,
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
        // Drop the sender to signal the watchdog, then join.
        drop(self.watchdog_signal.take());
        if let Some(h) = self.watchdog_handle.take() {
            let _ = h.join();
        }

        if let Err(e) = tick(self.mode, &self.runtime, &self.claude_home, &self.canonical) {
            eprintln!("clauth: final sync failed: {e}");
        }

        // Flush this session's last `.claude.json` changes to the global file
        // and siblings before a possible teardown removes this runtime copy.
        if let Err(e) = crate::claude_json::sync_once() {
            eprintln!("clauth: final .claude.json sync failed: {e}");
        }

        if let Err(e) = with_state_lock(|| {
            if let Err(e) = std::fs::remove_file(&self.pid_file)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!("clauth: remove pid file failed: {e}");
            }
            let still_active = prune_stale_sessions(&self.sessions).unwrap_or(1);
            if still_active == 0 {
                let _ = std::fs::remove_dir_all(&self.runtime);
                let _ = std::fs::remove_dir(&self.sessions);
            }
            Ok::<_, anyhow::Error>(())
        }) {
            eprintln!("clauth: drop cleanup failed: {e}");
        }
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

/// Walk `sessions/`, drop entries whose owner has died, return the live count.
/// Caller holds the cross-process state lock so two simultaneous starts can't
/// both conclude "no other sessions" and tear down the runtime under each other.
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
    // Open without O_CREAT: creating the file would race with another session
    // that just created it but hasn't locked it yet, producing a false
    // "unlocked = dead" reading. try_lock succeeds iff no other open fd holds
    // an exclusive flock, i.e. the previous owner has exited.
    let Ok(file) = OpenOptions::new().read(true).write(true).open(pid_file) else {
        return false;
    };
    // Any I/O error: treat as alive so we don't race a live session.
    file.try_lock().is_err()
}

/// Build or incrementally update the runtime tree.
///
/// Called on every `acquire`, including when `active > 0` (siblings already
/// built the tree). The walk always runs; entries whose runtime counterpart
/// already exists are skipped, so `~/.claude/` additions after the first build
/// are picked up without disturbing the rest.
///
/// Shared vs. per-profile layout:
/// - **Shared via symlink/copy across all profiles:** every top-level entry
///   in `~/.claude/` except `settings.json` and `.credentials.json` —
///   this includes `projects/`, `todos/`, `statsig/`, `sessions/`, `cache/`,
///   `commands/`, `plugins/`, `tasks/`, `teams/`, `hooks/`, `history.jsonl`,
///   and similar. Claude Code treats these as user-global state so sharing is
///   intentional; per-profile isolation would hide project history and
///   installed commands.
/// - **Per-profile:** `settings.json` (merged with profile overrides),
///   `.credentials.json` (the profile's own OAuth token chain), and
///   `.claude.json` (a copy seeded from `~/.claude.json`). Settings are
///   rewritten when changed; credentials are reconciled without using the
///   shared `~/.claude/.credentials.json` copy; `.claude.json` is reconciled
///   across all profiles by `crate::claude_json`, which propagates every field
///   except the account-specific ones (`oauthAccount` + billing caches).
fn build_runtime_dir(
    runtime: &Path,
    claude_home: &Path,
    profile: &Profile,
    canonical: &Path,
    mode: LinkMode,
) -> Result<()> {
    let mut pending: Vec<(PathBuf, PathBuf)> = Vec::new();
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
        pending.push((entry.path(), dst));
    }
    materialize_entries(pending, mode)?;
    write_merged_settings(runtime, claude_home, profile)?;

    let creds_link = runtime.join(".credentials.json");
    reconcile_credentials(&creds_link, canonical, mode)?;

    seed_claude_json(runtime, claude_home)?;

    Ok(())
}

/// Compute this profile's merged `settings.json` and write it into the runtime
/// tree only when absent or byte-different. Concurrent sessions on the same
/// profile each compute the same merge, so a byte-identical result needn't win
/// a last-writer race and stomp a sibling's write.
fn write_merged_settings(runtime: &Path, claude_home: &Path, profile: &Profile) -> Result<()> {
    let settings_src = claude_home.join("settings.json");
    let merged = build_claude_settings_json(&settings_src, profile, &[])?;
    let settings_dst = runtime.join("settings.json");
    let needs_write = std::fs::read(&settings_dst)
        .map(|existing| existing != merged.as_bytes())
        .unwrap_or(true);
    if needs_write {
        atomic_write(&settings_dst, merged).context("failed to write runtime settings.json")?;
    }
    Ok(())
}

/// Seed this profile's private copy of `~/.claude.json`. Claude Code's big
/// config file embeds an account-specific `oauthAccount` block (plus billing
/// caches) that must NOT be shared across profiles — CC trusts the cached
/// identity and won't re-derive it from the token on a normal startup, so a
/// shared symlink leaks one account's identity into another. The background
/// syncer (`crate::claude_json`) keeps the non-per-profile fields converged
/// across all copies (latest write wins). A freshly seeded copy inherits the
/// global file's `oauthAccount`; that profile's next OAuth login overwrites it
/// with the correct identity, which the syncer then preserves.
///
/// Seeds from the global file when this profile has no real copy yet, or
/// migrates the old shared symlink (pre-per-profile behavior) to a copy.
/// `atomic_write` renames over the path, replacing a symlink in one step — no
/// window where a sibling session sees the file missing. Existing real copies
/// keep their own identity and synced shared fields.
fn seed_claude_json(runtime: &Path, claude_home: &Path) -> Result<()> {
    let Some(home) = claude_home.parent() else {
        return Ok(());
    };
    let global = home.join(".claude.json");
    let dst = runtime.join(".claude.json");
    let is_symlink = dst
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink());
    if (is_symlink || !dst.exists())
        && let Ok(bytes) = std::fs::read(&global)
    {
        atomic_write(&dst, &bytes).with_context(|| format!("failed to seed {}", dst.display()))?;
    }
    Ok(())
}

fn materialize_entry(src: &Path, dst: &Path, mode: LinkMode) -> Result<()> {
    match mode {
        LinkMode::Real => link_entry(src, dst),
        LinkMode::Fake => copy_tree(src, dst),
    }
}

/// Materialize the pending top-level entries into the runtime tree.
///
/// Real mode creates symlinks serially (near-free). Fake mode is a recursive
/// byte copy, so the independent top-level subtrees are fanned across a bounded
/// worker pool to cut acquire wall-time on a large `~/.claude/`. Stays inside
/// the caller's single `with_state_lock` hold — the lock is never released;
/// threads only parallelize the copy. Each subtree is disjoint (no shared dst);
/// credential reconciliation still runs serially after this returns.
fn serialize_entries(pending: &[(PathBuf, PathBuf)], mode: LinkMode) -> Result<()> {
    for (src, dst) in pending {
        materialize_entry(src, dst, mode)?;
    }
    Ok(())
}

fn materialize_entries(pending: Vec<(PathBuf, PathBuf)>, mode: LinkMode) -> Result<()> {
    if mode == LinkMode::Real || pending.len() < 2 {
        return serialize_entries(&pending, mode);
    }

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(pending.len());
    if workers < 2 {
        return serialize_entries(&pending, mode);
    }

    let next = std::sync::atomic::AtomicUsize::new(0);
    let first_err = std::sync::Mutex::new(None::<anyhow::Error>);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let idx = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((src, dst)) = pending.get(idx) else {
                        break;
                    };
                    if let Err(e) = materialize_entry(src, dst, mode) {
                        let mut slot = first_err.lock().unwrap_or_else(|p| p.into_inner());
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                        break;
                    }
                }
            });
        }
    });

    match first_err.into_inner().unwrap_or_else(|p| p.into_inner()) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn reconcile_credentials(runtime_path: &Path, canonical: &Path, mode: LinkMode) -> Result<()> {
    match mode {
        LinkMode::Real => {
            sync_credentials_unlocked(runtime_path, canonical)?;
            let meta = runtime_path.symlink_metadata().ok();
            if meta.is_some_and(|m| m.file_type().is_symlink() || m.is_file()) {
                return Ok(());
            }
            if canonical.exists() {
                create_symlink(canonical, runtime_path)?;
            }
        }
        LinkMode::Fake => {
            mirror_credentials(runtime_path, canonical)?;
        }
    }
    Ok(())
}

/// Used in fake-symlink mode when the OS denies symlink creation rights.
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

/// One watchdog iteration. Real mode only repairs `.credentials.json` (the rest
/// is symlinks needing no maintenance). Fake mode reconciles every tree file by
/// mtime, plus the credentials file.
fn tick(mode: LinkMode, runtime: &Path, claude_home: &Path, canonical: &Path) -> Result<()> {
    match mode {
        LinkMode::Real => {
            let _ = sync_credentials(runtime, canonical)?;
            Ok(())
        }
        LinkMode::Fake => {
            // Bulk tree walk + copies run WITHOUT the state lock: on a large
            // ~/.claude/ holding the lock across the walk stalled every
            // concurrent acquire / CLI switch for hundreds of ms per tick.
            // Lockless-safe: every per-file merge is independent, self-converging
            // under "latest mtime wins" + byte-equality skip, and never deletes
            // — a file changing in the TOCTOU window re-converges next tick.
            // mirror_tree skips settings.json / .credentials.json, so it never
            // races build_runtime_dir's per-profile writes. Only credential
            // reconciliation (must not interleave with acquire/switch credential
            // writes) stays under the lock.
            mirror_tree(claude_home, runtime)?;
            with_state_lock(|| mirror_credentials(&runtime.join(".credentials.json"), canonical))
        }
    }
}

/// If Claude Code's internal refresh replaced `<runtime>/.credentials.json` with
/// a regular file, copy its bytes into canonical creds and swap the file back to
/// a symlink so canonical stays the single source of truth. Returns `true` when
/// bytes were written. Real-symlink mode only — fake mode uses
/// [`mirror_credentials`].
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
    let runtime_bytes = std::fs::read(link_path).context("failed to read live credentials")?;
    // Skip if CC's write is mid-flight (partial, invalid, or empty object).
    // {} deserializes as ClaudeCredentials { claude_ai_oauth: None } because
    // the field is Option — require Some to confirm a completed write.
    let Ok(runtime_creds) = serde_json::from_slice::<ClaudeCredentials>(&runtime_bytes) else {
        return Ok(false);
    };
    if runtime_creds.claude_ai_oauth.is_none() {
        return Ok(false);
    }
    let canonical_bytes = std::fs::read(canonical).ok();
    let differs = canonical_bytes.as_deref() != Some(runtime_bytes.as_slice());
    let mut wrote_canonical = false;
    if differs {
        // Bytes differ. The keep-canonical-vs-adopt-runtime decision (write
        // recency primary, `expires_at` as the tie-break) lives in
        // `resolve_credential_winner` — see its doc for why mtime, not expiry,
        // is the signal.
        let canonical_exp = canonical_bytes.as_deref().and_then(|cb| {
            let c = serde_json::from_slice::<ClaudeCredentials>(cb).ok()?;
            Some(c.claude_ai_oauth?.expires_at.unwrap_or(0))
        });
        let runtime_exp = runtime_creds
            .claude_ai_oauth
            .as_ref()
            .map(|o| o.expires_at.unwrap_or(0));
        let canonical_mtime = std::fs::metadata(canonical)
            .ok()
            .and_then(|m| m.modified().ok());
        let runtime_mtime = meta.modified().ok();
        if resolve_credential_winner(canonical_exp, runtime_exp, canonical_mtime, runtime_mtime) {
            // Canonical written at/after the runtime re-login (or wins the
            // tie-break); don't overwrite it with the runtime bytes.
            eprintln!(
                "clauth: watchdog kept canonical credentials \
                 (canonical written more recently than runtime); \
                 not overwriting with runtime re-login bytes"
            );
        } else {
            atomic_write_600(canonical, &runtime_bytes)?;
            wrote_canonical = true;
        }
    }
    relink_to_canonical(link_path, canonical)?;
    Ok(wrote_canonical)
}

/// Decide whether to keep the canonical credentials instead of adopting the
/// runtime file's bytes, given each side's token `expires_at` and file mtime.
/// Returns `true` to keep canonical.
///
/// The two files can hold INDEPENDENT, both-valid refresh-token chains: the
/// TUI/scheduler may rotate canonical while Claude Code writes a fresh
/// interactive re-login into the runtime file. So `expires_at` is the wrong
/// primary signal — it's a property of the token, not of which login the user
/// performed last. A forced rotate-all (`t` key) can stamp a canonical token
/// whose `expires_at` is marginally later than CC's fresh login; keeping
/// canonical there would silently discard that login and burn its chain.
///
/// Primary signal is write recency (mtime): CC's `unlink+write` re-login and
/// our `atomic_write` both bump mtime, so "most recently written wins" reflects
/// the intended-live login. `expires_at` is the tie-break only when mtimes are
/// equal/unavailable, and a full tie keeps canonical. A missing/unparseable
/// canonical (`canonical_exp` = `None`) always lets runtime win.
fn resolve_credential_winner(
    canonical_exp: Option<i64>,
    runtime_exp: Option<i64>,
    canonical_mtime: Option<std::time::SystemTime>,
    runtime_mtime: Option<std::time::SystemTime>,
) -> bool {
    match (canonical_exp, runtime_exp) {
        // Canonical present and parseable: mtime is the primary signal — trust
        // the most recently written file regardless of token expiry. expires_at
        // is the tie-break only when mtimes are equal/unavailable; canonical
        // wins that fallback tie.
        (Some(ce), Some(re)) => match (canonical_mtime, runtime_mtime) {
            (Some(cm), Some(rm)) if cm != rm => cm > rm,
            _ => ce >= re,
        },
        // Runtime has no token: nothing to adopt, keep canonical.
        (Some(_), None) => true,
        // Canonical missing or unparseable: runtime always wins, never let a
        // newer mtime on corrupt/absent canonical override that.
        _ => false,
    }
}

/// Repoint the runtime credential link at canonical so canonical stays the
/// single source of truth. Swaps via a temp symlink + atomic rename so a sibling
/// session never sees the path missing; if canonical is gone, removes the file.
fn relink_to_canonical(link_path: &Path, canonical: &Path) -> Result<()> {
    if canonical.exists() {
        let tmp = link_path.with_file_name(format!(".credentials.json.tmp.{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        create_symlink(canonical, &tmp)?;
        std::fs::rename(&tmp, link_path)?;
    } else {
        std::fs::remove_file(link_path)?;
    }
    Ok(())
}

/// Bidirectional mtime mirror between `runtime/.credentials.json` and canonical
/// creds: "latest mtime wins", newer side copied over older. Skips partial
/// writes (invalid JSON). Fake-symlink mode only.
fn mirror_credentials(runtime_path: &Path, canonical: &Path) -> Result<()> {
    let runtime_meta = runtime_path.metadata().ok();
    let canonical_meta = canonical.metadata().ok();

    if let Some((src, dst)) = newer_side(runtime_path, canonical, runtime_meta, canonical_meta) {
        copy_if_valid_creds(src, dst)?;
    }
    Ok(())
}

/// Resolve which credential side is newer or sole-present. Returns `(src, dst)`
/// where bytes should flow from `src` to `dst`, or `None` when equal/unknown.
fn newer_side<'a>(
    runtime_path: &'a Path,
    canonical: &'a Path,
    runtime_meta: Option<std::fs::Metadata>,
    canonical_meta: Option<std::fs::Metadata>,
) -> Option<(&'a Path, &'a Path)> {
    match (runtime_meta, canonical_meta) {
        (Some(rm), Some(cm)) => match rm.modified().ok().zip(cm.modified().ok()) {
            Some((rt, ca)) if rt > ca => Some((runtime_path, canonical)),
            Some((rt, ca)) if ca > rt => Some((canonical, runtime_path)),
            _ => None,
        },
        (Some(_), None) => Some((runtime_path, canonical)),
        (None, Some(_)) => Some((canonical, runtime_path)),
        (None, None) => None,
    }
}

fn copy_if_valid_creds(src: &Path, dst: &Path) -> Result<()> {
    let bytes = std::fs::read(src).with_context(|| format!("failed to read {}", src.display()))?;
    // Same guard as sync_credentials_unlocked: reject partial, invalid, or
    // empty-object writes before letting them stomp the canonical file.
    let Ok(creds) = serde_json::from_slice::<ClaudeCredentials>(&bytes) else {
        return Ok(());
    };
    if creds.claude_ai_oauth.is_none() {
        return Ok(());
    }
    if std::fs::read(dst).ok().as_deref() == Some(bytes.as_slice()) {
        return Ok(());
    }
    atomic_write_600(dst, &bytes).with_context(|| format!("failed to write {}", dst.display()))
}

/// Walk both `~/.claude/` and the runtime tree; copy the newer bytes onto the
/// older, seeding one-sided files onto the other — CC may create runtime-side
/// state (project history, scratch files) and the user may add `~/.claude/`
/// entries between ticks, both must propagate. **No deletion**: a file missing
/// from one side is "not yet seen", never "intentionally removed", so the mirror
/// never destroys data. Top-level `settings.json` / `.credentials.json` are
/// skipped (settings is a rewritten copy; credentials has its own stricter
/// mirror). Fake-symlink mode only.
fn mirror_tree(claude_home: &Path, runtime: &Path) -> Result<()> {
    // `.claude.json` is a per-profile copy reconciled by `crate::claude_json`,
    // not part of the `~/.claude/` tree — skip it here so the tree mirror never
    // copies it into `~/.claude/.claude.json`.
    let skip_top: HashSet<&str> = ["settings.json", ".credentials.json", ".claude.json"]
        .into_iter()
        .collect();
    for name in union_children(claude_home, runtime) {
        if name.to_str().is_some_and(|n| skip_top.contains(n)) {
            continue;
        }
        merge_path(&claude_home.join(&name), &runtime.join(&name))?;
    }
    Ok(())
}

/// Unioned child-name set of two directories. Absent/unreadable side
/// contributes nothing. Names sorted for deterministic, stable iteration.
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
            if files_match(a, b)? {
                return Ok(());
            }
            if mtime_newer(a_time, b_time) {
                copy_file(a, b)?;
            } else if mtime_newer(b_time, a_time) {
                copy_file(b, a)?;
            }
        }
        (Some(_), None) => {
            copy_file(a, b)?;
        }
        (None, Some(_)) => {
            copy_file(b, a)?;
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

fn files_match(a: &Path, b: &Path) -> Result<bool> {
    let a_bytes = std::fs::read(a).with_context(|| format!("failed to read {}", a.display()))?;
    let b_bytes = std::fs::read(b).with_context(|| format!("failed to read {}", b.display()))?;
    Ok(a_bytes == b_bytes)
}

/// Copy `src` onto `dst` via a PID-suffixed tmp + atomic rename. `mirror_tree`
/// runs lockless, so a concurrent reader (sibling session, user, or
/// `build_runtime_dir`) could observe `dst` mid-write; a raw `std::fs::copy`
/// truncates-then-streams (non-atomic, seen torn). The rename makes the swap
/// atomic on POSIX (observer sees old or complete-new); the PID suffix keeps two
/// processes off the same tmp name.
fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    let bytes = std::fs::read(src).with_context(|| format!("failed to read {}", src.display()))?;
    atomic_write(dst, &bytes)
        .with_context(|| format!("failed to copy {} -> {}", src.display(), dst.display()))
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
