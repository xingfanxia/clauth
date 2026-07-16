//! `clauth daemon` — headless scheduler owner.
//!
//! Runs the exact same background refresher the TUI runs (`spawn_refresher`),
//! but with no ratatui loop. Its jobs each tick:
//!   1. execute any auto-switch the scheduler queued (`pending_switch` /
//!      `pending_switch_off`) — this is what makes unattended auto-switch work
//!      with the TUI closed, the operator's core requirement;
//!   2. rewrite `~/.clauth/status.json` atomically (the external read feed);
//!   3. pick up external config changes (a new `clauth login`, a TUI edit).
//!
//! The scheduler already persists `usage_cache.json` inside `apply_outcome`, so
//! the daemon and the TUI share one cache. A single-instance advisory lock keeps
//! two schedulers from double-firing.

pub(crate) mod log_rotate;
mod probe;
mod status_json;
mod tick;
mod types;

/// The single-fetcher lease + the header dot's daemon presence/health probe
/// (dual-scheduler dedup, #27).
pub(crate) use probe::{DaemonHealth, FetchLease, daemon_health};
/// Small daemon state types + the backoff schedule, re-exported so callers keep
/// referencing them as `super::…` after the extraction.
pub(crate) use types::{SwitchBackoff, switch_backoff_ms};

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::claude::{
    LinkState, classify_credentials_link, is_first_login, link_profile_credentials,
    live_credentials_are_shell,
};
use crate::lockorder::RankedMutex;
use crate::logline::logline;
use crate::profile::{
    AppConfig, ConfigHandle, app_state_mtime, atomic_write_600, clauth_dir, load_config, mkdir_700,
};
use crate::usage::{
    ActivityStore, FetchStatus, KickBlocks, LastFetchedAt, NextRefreshPerProfile, PendingSwitch,
    PendingSwitchOff, PollStreaks, RefetchQueue, StatusStore, SuppressedGenericStore,
    ThirdPartyList, ThirdPartyStatusStore, ThirdPartyUsageStore, TokenList, UsageStore,
    bootstrap_fetch, bootstrap_third_party, collect_third_party_entries, collect_tokens,
    spawn_refresher,
};
use status_json::{LiveSignals, build_status};

/// Main-loop cadence. The scheduler ticks on its own 1s timer; this loop only
/// executes queued switches/config edits and rewrites `status.json`, so 1s is plenty.
const TICK: Duration = Duration::from_secs(1);
/// How often (in `TICK`s) the run loop checks `daemon.log` for size-capping
/// ~5 min at the 1s tick — rare enough to be free, frequent enough to
/// bound a busy log.
const LOG_ROTATE_EVERY_TICKS: u64 = 300;
const STATUS_FILE: &str = "status.json";
const LOCK_FILE: &str = "clauthd.lock";
/// The single-fetcher lease file (#27). A peer of [`LOCK_FILE`] in `~/.clauth`,
/// held for life by whichever instance (daemon or a TUI) is the current usage
/// fetcher. See [`FetchLease`](probe::FetchLease).
const FETCH_LOCK_FILE: &str = "usage-fetch.lock";

/// Anti-wedge watchdog: abort if no tick completes within this window.
/// `TICK` is 1s, so ~30 missed ticks. The blocking `StateLock` has no deadline
/// and a switch runs a `/usr/bin/security` subprocess inside it, so a stuck
/// keychain or a wedged flock holder can freeze the single-threaded run loop;
/// `std::process::abort()` then lets launchd's `KeepAlive{SuccessfulExit=false}`
/// restart the daemon (boot()'s relink + atomic writes make restart safe).
///
/// Tightened 60s→30s for the single-fetcher lease (#27): a wedged-alive daemon
/// keeps holding `usage-fetch.lock`, so no other instance can fetch until it
/// dies — 30s frees the lease about as fast as the retired TUI freshness re-arm
/// did. TENSION: a legit switch's keychain shell-out can block up to its own 20s
/// kill deadline inside the `StateLock`, leaving only ~10s of slack. If that ever
/// false-aborts, bound the keychain shell-out (the real fix), do NOT loosen this
/// deadline — the lease's wedged-daemon recovery depends on it.
///
/// SCOPE: `heartbeat` is stamped by the MAIN loop only, so this covers a wedged
/// main loop. A wedged SCHEDULER thread (which is what actually holds the lease)
/// keeps the main loop ticking and the feed fresh, so it trips nothing and the
/// lease is never freed — pre-existing (the retired probe keyed on the same
/// main-loop freshness), tracked in `docs/todo.md`. Do not read this deadline as
/// covering the fetch path itself.
const WATCHDOG_DEADLINE: Duration = Duration::from_secs(30);
/// How often the watchdog re-checks the tick heartbeat.
const WATCHDOG_POLL: Duration = Duration::from_secs(10);

/// One watchdog evaluation: if the main loop last completed a tick more than
/// `deadline_ms` ago, invoke `on_stall`. Production passes `std::process::abort`;
/// tests inject a flag/panic. A zero `last_tick_ms` (no tick yet — boot in
/// progress) never trips. Pure so the abort decision is unit-testable.
fn watchdog_check(last_tick_ms: u64, now_ms: u64, deadline_ms: u64, on_stall: impl FnOnce()) {
    if last_tick_ms != 0 && now_ms.saturating_sub(last_tick_ms) > deadline_ms {
        on_stall();
    }
}

/// Tighten an existing `~/.clauth` tree on boot. `mkdir_700` only
/// sets the mode on dirs it CREATES; a tree from an older build or created by the
/// CLI under a permissive umask can be 0o755 (world-traversable → world-readable
/// `daemon.log`, enumerable account names). Two things are enforced, both
/// best-effort (a failure must never stop the daemon):
/// - `~/.clauth` and `~/.clauth/profiles` → 0o700. Tightening the root alone
///   blocks other-user traversal into every per-profile subdir regardless of the
///   subdirs' own modes, so they need not be walked.
/// - `~/.clauth/daemon.log` → 0o600. The daemon does not create this file —
///   launchd opens it (`StandardErrorPath`) at the process umask (~0o644) before
///   `exec` — so clauth chmods it here to match the `0o600` SECURITY.md pledges
///   (it can echo a config-parse error carrying a `config.toml` api_key snippet).
///   The already-open launchd fd keeps appending to the now-0o600 inode.
#[cfg(unix)]
fn migrate_clauth_perms_700(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    for d in [dir.to_path_buf(), dir.join("profiles")] {
        if d.is_dir() {
            let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
        }
    }
    let log = dir.join("daemon.log");
    if log.is_file() {
        let _ = std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o600));
    }
}
#[cfg(not(unix))]
fn migrate_clauth_perms_700(_dir: &std::path::Path) {}

/// `clauth daemon` — build the shared stores, run the scheduler headless, and
/// loop executing auto-switches + rewriting `status.json` until killed.
pub(crate) fn serve() -> Result<()> {
    // First thing, before any output (including the standing-by line below):
    // daemon stderr IS daemon.log, and undated lines cost real forensics time
    // (2026-07-09 — see `logline`).
    crate::logline::enable_timestamps();
    log_rotate::warn_if_log_cap_defeated();
    crate::platform::init();
    crate::runtime::gc_stale_runtimes();

    let dir = clauth_dir()?;
    // Create ~/.clauth at 0o700 (was create_dir_all → umask 0o755),
    // then tighten an existing looser tree (older builds / CLI umask left it 0o755).
    mkdir_700(&dir).context("failed to create ~/.clauth")?;
    migrate_clauth_perms_700(&dir);

    // Single-instance guard as STANDBY: hold an exclusive advisory lock
    // for our lifetime so two daemons can't both run a scheduler. A second
    // instance BLOCKS on the lock rather than exiting clean — so when a manually
    // run `clauth daemon` holds it, the launchd instance parks here and takes
    // over the instant the manual one exits, keeping the plist's
    // `KeepAlive{SuccessfulExit=false}` valid (a clean exit must never have to be
    // restarted). A dead holder's advisory flock auto-releases, so standby is
    // never orphaned.
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(LOCK_FILE))
        .context("failed to open the clauth daemon lock file")?;
    if lock_file.try_lock().is_err() {
        logline!("clauth daemon: another instance holds the lock: standing by until it exits");
        lock_file
            .lock()
            .context("failed to acquire the clauth daemon lock")?;
    }
    // Held for the process lifetime; the flock releases when the process exits.
    let _lock = lock_file;

    let config = load_config()?;
    let mut daemon = Daemon::new(config, dir.join(STATUS_FILE));
    daemon.boot();
    logline!(
        "clauth daemon: running (status → {})",
        daemon.status_path.display()
    );
    daemon.run();
    Ok(())
}

/// `clauth status --json` — single-shot serializer. Reads the on-disk caches and
/// prints the same shape the daemon writes, then exits. No scheduler; freshness
/// and next-refresh are derived from cache mtimes.
pub(crate) fn status_oneshot() -> Result<()> {
    let config = load_config()?;
    let interval = config.state.refresh_interval_ms;
    let body = build_status(&config, interval, None);
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

/// True when the live credentials diverge from the active profile's stored chain
/// and it isn't a first-login adoption — the daemon cannot prompt, so it skips
/// the switch and leaves the resolution to the operator (TUI Divergence modal).
///
/// Claude Code's logged-out shell (both tokens blanked after its own refresh
/// died) is exempt: it classifies Diverged, but an empty login is not
/// "unsaved credentials" — deferring on it wedges every headless switch behind
/// a TUI decision about nothing while running sessions sit at "Login expired"
/// (observed 2026-07-15). An unreadable/unparseable live file still defers:
/// it may be a CC write in progress.
fn active_diverged_unsaved(active: &str) -> bool {
    matches!(
        classify_credentials_link(active).ok(),
        Some(LinkState::Diverged)
    ) && !is_first_login(active).unwrap_or(false)
        && !live_credentials_are_shell()
}

/// Owns the shared `Arc` stores (cloned into the scheduler) plus main-loop-only
/// state. Only the main thread touches `self`; the scheduler holds `Arc` clones
/// of the individual stores.
struct Daemon {
    config: ConfigHandle,
    usage_tokens: TokenList,
    usage_store: UsageStore,
    usage_status: StatusStore,
    refresh_interval: Arc<AtomicU64>,
    next_refresh_per_profile: NextRefreshPerProfile,
    activity: ActivityStore,
    last_fetched: LastFetchedAt,
    poll_streaks: PollStreaks,
    pending_switch: PendingSwitch,
    pending_switch_off: PendingSwitchOff,
    refetch_queue: RefetchQueue,
    third_party_tokens: ThirdPartyList,
    third_party_usage_store: ThirdPartyUsageStore,
    third_party_status: ThirdPartyStatusStore,
    shutting_down: Arc<AtomicBool>,
    /// Last-seen `profiles.toml` mtime — drives external-change reload. Bumped to
    /// the post-write value after every self-initiated switch so the daemon never
    /// reloads its own write.
    last_state_mtime: Option<SystemTime>,
    /// Epoch-ms of the last completed main-loop tick — the watchdog's liveness
    /// signal. `0` until the first tick completes.
    heartbeat: Arc<AtomicU64>,
    /// Backoff/dedup state for a persistently-failing switch: a target stuck on
    /// a transient failure retries on a widening schedule instead of 1/tick,
    /// and re-logs only when the target or reason changes.
    switch_backoff: Option<SwitchBackoff>,
    /// Count of ACTUAL failure-log emissions (post-dedup) — the observable proof a
    /// stuck switch isn't logging 1/tick. Read by tests.
    switch_failure_logs: u64,
    status_path: PathBuf,
}

impl Daemon {
    fn new(config: AppConfig, status_path: PathBuf) -> Self {
        let usage_tokens: TokenList = Arc::new(RankedMutex::new(collect_tokens(&config)));
        let third_party_tokens: ThirdPartyList = Arc::new(RankedMutex::new(
            collect_third_party_entries(&config.profiles),
        ));
        let refresh_interval = Arc::new(AtomicU64::new(config.state.refresh_interval_ms));
        Self {
            config: Arc::new(RankedMutex::new(config)),
            usage_tokens,
            usage_store: Arc::new(RankedMutex::new(HashMap::new())),
            usage_status: Arc::new(RankedMutex::new(HashMap::new())),
            refresh_interval,
            next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
            activity: Arc::new(RankedMutex::new(HashMap::new())),
            last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
            poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
            pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
            pending_switch_off: Arc::new(RankedMutex::new(false)),
            refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
            third_party_tokens,
            third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
            third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
            // Never set by the daemon: process exit IS its shutdown (a
            // supervisor restarts crashes; the singleton flock releases on
            // exit). The flag exists for `spawn_refresher`'s contract — its
            // real writer is the TUI's quit path.
            shutting_down: Arc::new(AtomicBool::new(false)),
            last_state_mtime: app_state_mtime(),
            heartbeat: Arc::new(AtomicU64::new(0)),
            switch_backoff: None,
            switch_failure_logs: 0,
            status_path,
        }
    }

    /// Re-establish the active profile's credential symlink, seed usage from the
    /// on-disk caches, and launch the scheduler. Mirrors the TUI's bootstrap.
    fn boot(&self) {
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let active = self
            .config
            .lock()
            .expect("config mutex poisoned")
            .state
            .active_profile
            .as_deref()
            .map(str::to_string);
        if let Some(active) = active {
            let _ = link_profile_credentials(&active);
        }

        let (snapshot, third_party) = {
            #[allow(
                clippy::expect_used,
                reason = "config mutex poisoning is unrecoverable"
            )]
            let cfg = self.config.lock().expect("config mutex poisoned");
            (
                collect_tokens(&cfg),
                collect_third_party_entries(&cfg.profiles),
            )
        };
        let interval = self.refresh_interval.load(Ordering::Relaxed);
        bootstrap_fetch(
            &self.usage_store,
            &self.usage_status,
            &self.last_fetched,
            &snapshot,
            interval,
        );
        bootstrap_third_party(
            &self.third_party_usage_store,
            &self.third_party_status,
            &self.last_fetched,
            &third_party,
            interval,
        );
        self.spawn_scheduler();
    }

    /// Bundle scheduler `Arc`s and launch the background refresher (same call the
    /// TUI's `start_scheduler` makes). The suppressed-generic set is daemon-local.
    fn spawn_scheduler(&self) {
        let suppressed: SuppressedGenericStore = Arc::new(RankedMutex::new(HashSet::new()));
        // Daemon-local like `suppressed`: the daemon never renders pills, so the
        // block map only backs the scheduler's own gate + its write-through
        // cache files (which a standdown TUI mirrors).
        let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(std::collections::HashMap::new()));
        spawn_refresher(
            Arc::clone(&self.config),
            Arc::clone(&self.usage_tokens),
            Arc::clone(&self.usage_store),
            Arc::clone(&self.usage_status),
            Arc::clone(&self.refresh_interval),
            Arc::clone(&self.next_refresh_per_profile),
            Arc::clone(&self.activity),
            Arc::clone(&self.last_fetched),
            Arc::clone(&self.poll_streaks),
            kick_blocks,
            Arc::clone(&self.pending_switch),
            Arc::clone(&self.pending_switch_off),
            Arc::clone(&self.refetch_queue),
            Arc::clone(&self.third_party_tokens),
            Arc::clone(&self.third_party_usage_store),
            Arc::clone(&self.third_party_status),
            suppressed,
            Arc::clone(&self.shutting_down),
            // Single-fetcher lease (#27): the daemon competes for `usage-fetch.lock`
            // like any instance. It normally boots first (launchd) and wins the
            // lease for life, but if a TUI is already fetching, the daemon stands
            // its refresher down and hydrates instead — the main loop still writes
            // `status.json` every tick regardless of who fetches. A fresh lease per
            // scheduler; the tick thread's clone keeps the flock held for the
            // process lifetime.
            Arc::new(FetchLease::new()),
        );
    }

    /// Main loop. Writes an initial `status.json` immediately (so a reader that
    /// attaches before the first fetch has something to read), then each tick
    /// reloads external config changes, executes queued switches, and rewrites
    /// `status.json`. Runs until the process is killed.
    fn run(&mut self) {
        self.write_status();
        // Stamp the first heartbeat before the watchdog starts so it never trips
        // on the zero-heartbeat boot window, then spawn it.
        self.heartbeat
            .store(crate::usage::now_ms(), Ordering::Relaxed);
        self.spawn_watchdog();
        // daemon.log lives beside status.json; cap it on a ~5-min cadence (and at
        // boot, tick 0) so a pre-fix crash-loop log or a busy period can't grow it
        // unbounded. The check is a cheap stat that no-ops well
        // under the cap.
        let log_path = self.status_path.with_file_name("daemon.log");
        let mut ticks: u64 = 0;
        loop {
            if ticks.is_multiple_of(LOG_ROTATE_EVERY_TICKS) {
                let _ = log_rotate::rotate_log_if_large(
                    &log_path,
                    log_rotate::LOG_MAX_BYTES,
                    log_rotate::LOG_KEEP_BYTES,
                );
            }
            std::thread::sleep(TICK);
            self.tick();
            self.heartbeat
                .store(crate::usage::now_ms(), Ordering::Relaxed);
            ticks = ticks.wrapping_add(1);
        }
    }

    /// Spawn the anti-wedge watchdog. It observes the main loop's tick
    /// heartbeat and `std::process::abort`s if a tick hasn't completed within
    /// [`WATCHDOG_DEADLINE`] — launchd's `KeepAlive{SuccessfulExit=false}` then
    /// restarts the daemon. This is the backstop for the deadline-free
    /// `StateLock`: a switch's `/usr/bin/security` subprocess or a wedged flock
    /// holder can otherwise freeze the single-threaded loop indefinitely, with
    /// nothing to restart a hung-but-alive process at 3am.
    fn spawn_watchdog(&self) {
        let heartbeat = Arc::clone(&self.heartbeat);
        let spawned = std::thread::Builder::new()
            .name("clauth-daemon-watchdog".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(WATCHDOG_POLL);
                    watchdog_check(
                        heartbeat.load(Ordering::Relaxed),
                        crate::usage::now_ms(),
                        WATCHDOG_DEADLINE.as_millis() as u64,
                        || {
                            logline!(
                                "clauth daemon: watchdog: no tick within {}s; aborting for a \
                                 clean launchd restart",
                                WATCHDOG_DEADLINE.as_secs()
                            );
                            std::process::abort();
                        },
                    );
                }
            });
        if let Err(e) = spawned {
            // No watchdog = a wedged loop hangs forever with launchd seeing a
            // live process. Say so loudly; the daemon still runs.
            logline!(
                "clauth daemon: failed to spawn the anti-wedge watchdog: {e}. \
                 A stalled tick will NOT auto-restart this process"
            );
        }
    }

    /// Snapshot the live freshness/countdown stores — each snapshot's lock is
    /// fully released at the end of its own statement, so none is ever held
    /// when the `config` lock below is taken — then build and atomically
    /// write `status.json`.
    ///
    /// The config is snapshotted too: [`build_status`] stats and reads each
    /// profile's cache files and sweeps the session flocks, and holding CONFIG
    /// across that disk work every tick stalls every other config user (a switch,
    /// a TUI edit) behind it. The clone is a handful of small strings.
    fn write_status(&self) {
        let interval = self.refresh_interval.load(Ordering::Relaxed);
        let status_snap: HashMap<String, FetchStatus> = self
            .usage_status
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        let next_snap: HashMap<String, u64> = self
            .next_refresh_per_profile
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        // Snapshot the 429 streaks so build_status can publish `stale` (a
        // deep-slot stuck RateLimited). Lower rank than config, like the stores
        // above — snapshot + release before the config lock. Projected to the 429
        // axis on purpose: `stale` is contracted as a stuck THROTTLE
        // (`wiki/daemon.md`), so a refresh-fail streak must not leak into it.
        let streaks_snap: HashMap<String, u32> = self
            .poll_streaks
            .lock()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.rate_limit)).collect())
            .unwrap_or_default();
        // The in-flight switch target (accepted, not yet applied), so the UI
        // shows in-flight truth instead of a timing heuristic. Snapshot + release
        // before the config lock, like the freshness stores above. The set holds
        // at most one scheduler target in practice (`scan_auto_switch` skips
        // while one is pending); `min` keeps the snapshot deterministic anyway.
        let pending_snap: Option<String> = self
            .pending_switch
            .lock()
            .ok()
            .and_then(|q| q.iter().min().cloned());
        let live = LiveSignals {
            status: &status_snap,
            next_refresh: &next_snap,
            streaks: &streaks_snap,
            pending_switch: pending_snap.as_deref(),
        };
        let cfg_snap = {
            #[allow(
                clippy::expect_used,
                reason = "config mutex poisoning is unrecoverable"
            )]
            let cfg = self.config.lock().expect("config poisoned");
            cfg.clone()
        };
        let body = build_status(&cfg_snap, interval, Some(&live));
        match serde_json::to_vec_pretty(&body) {
            Ok(json) => {
                if let Err(e) = atomic_write_600(&self.status_path, &json) {
                    logline!("clauth daemon: failed to write status.json: {e}");
                }
            }
            Err(e) => logline!("clauth daemon: failed to serialize status.json: {e}"),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/inline/daemon_watchdog.rs"]
mod watchdog_tests;

#[cfg(test)]
#[path = "../../tests/inline/daemon_mod.rs"]
mod tests;
