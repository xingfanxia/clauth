//! `clauth daemon` — headless scheduler owner.
//!
//! Runs the exact same background refresher the TUI runs (`spawn_refresher`),
//! but with no ratatui loop. Its jobs each tick:
//!   1. execute any auto-switch the scheduler queued (`pending_switch` /
//!      `pending_switch_off`) — this is what makes unattended auto-switch work
//!      with the TUI closed, the operator's core requirement;
//!   2. rewrite `~/.clauth/status.json` atomically (the menu-bar read format);
//!   3. pick up external config changes (a new `clauth login`, a TUI edit).
//!
//! The scheduler already persists `usage_cache.json` inside `apply_outcome`, so
//! the daemon and the TUI share one cache. A single-instance advisory lock keeps
//! two schedulers from double-firing.

// The control socket is a unix-domain socket (`std::os::unix::net`); it does not
// exist on Windows. Gating it keeps `cargo check --target *-windows-*` (and the
// release build) green — the daemon runs its scheduler + status.json there
// without a socket.
pub(crate) mod log_rotate;
mod probe;
#[cfg(unix)]
mod socket;
mod status_json;
mod tick;
// TOK-3 tokens.json feed. Gated out of `cfg(test)`: it detaches loader threads
// whose atomic writes would outlive a test's `HOME_OVERRIDE` and hit the real
// `~/.clauth`/`~/.claude` (same rationale as the TUI's `app.rs` token wiring).
// The normal build clippy/`cargo build` check still compiles it.
#[cfg(not(test))]
mod tokens_snapshot;
mod types;
mod waker;

/// The single-fetcher lease + the header dot's daemon presence/health probe
/// (dual-scheduler dedup, #27).
pub(crate) use probe::{DaemonHealth, FetchLease, daemon_health};
/// The `status.json` schema version, re-exported so `clauth doctor` can compare
/// it against the daemon's on-disk value (version/schema skew check, TECH-12).
pub(crate) use status_json::SCHEMA_VERSION;
/// Small daemon state types + the backoff schedule, re-exported so callers keep
/// referencing them as `super::…` / `crate::daemon::…` after the extraction.
pub(crate) use types::{ConfigOp, LastError, LastSwitch, SwitchBackoff, switch_backoff_ms};

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::claude::{
    LinkState, classify_credentials_link, is_first_login, link_profile_credentials,
    live_credentials_are_shell,
};
use crate::lockorder::{RankedMutex, rank};
use crate::logline::logline;
use crate::profile::{
    AppConfig, ConfigHandle, ReloadFingerprint, atomic_write_600_fast, clauth_dir, load_config,
    mkdir_700, reload_fingerprint,
};
use crate::usage::{
    ActivityStore, FetchStatus, KickBlocks, LastFetchedAt, NextRefreshPerProfile, PendingSwitch,
    PendingSwitchOff, PollStreaks, RefetchQueue, StatusStore, SuppressedGenericStore,
    ThirdPartyList, ThirdPartyStatusStore, ThirdPartyUsageStore, TokenList, UsageStore,
    bootstrap_fetch, bootstrap_third_party, collect_third_party_entries, collect_tokens,
    select_switch_winner, spawn_refresher,
};
use status_json::{LiveSignals, build_status};

/// Queue of pending [`ConfigOp`]s. Standalone leaf lock (see [`rank::PendingConfigOps`]):
/// the socket pushes; the main loop drains into a `Vec` and releases before it
/// takes `config`.
pub(crate) type PendingConfigOps = Arc<RankedMutex<Vec<ConfigOp>, rank::PendingConfigOps>>;

/// Main-loop cadence. The scheduler ticks on its own 1s timer; this loop only
/// executes queued switches/config edits and rewrites `status.json`, so 1s is plenty.
const TICK: Duration = Duration::from_secs(1);
/// How often (in `TICK`s) the run loop checks `daemon.log` for size-capping
/// (TECH-12). ~5 min at the 1s tick — rare enough to be free, frequent enough to
/// bound a busy log.
const LOG_ROTATE_EVERY_TICKS: u64 = 300;
const STATUS_FILE: &str = "status.json";
#[cfg(unix)]
const SOCK_FILE: &str = "clauthd.sock";
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

/// Tighten an existing `~/.clauth` tree on boot (TECH-9 #13), before
/// `load_config` runs its own walk. `mkdir_700` only sets the mode on dirs it
/// CREATES; a tree from an older build or created by the CLI under a permissive
/// umask can be 0o755 (world-traversable → world-readable `daemon.log`,
/// enumerable account names). Delegates to [`crate::profile::enforce_clauth_perms`]
/// for the whole tree (dirs → 0o700, files → 0o600, symlinks skipped), which also
/// covers the launchd-created `daemon.log`: launchd opens it (`StandardErrorPath`)
/// at the process umask (~0o644) before `exec`, so this tightens it to the `0o600`
/// SECURITY.md pledges (it can echo a config-parse error carrying a `config.toml`
/// api_key snippet); the already-open launchd fd keeps appending to the now-0o600
/// inode. Best-effort — a chmod failure never stops the daemon.
fn migrate_clauth_perms_700(dir: &std::path::Path) {
    crate::profile::enforce_clauth_perms(dir);
}

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
    // TECH-9 #13: create ~/.clauth at 0o700 (was create_dir_all → umask 0o755),
    // then tighten an existing looser tree (older builds / CLI umask left it 0o755).
    mkdir_700(&dir).context("failed to create ~/.clauth")?;
    migrate_clauth_perms_700(&dir);

    // Single-instance guard as STANDBY (TECH-3): hold an exclusive advisory lock
    // for our lifetime so two daemons can't both run a scheduler. A second
    // instance BLOCKS on the lock rather than exiting clean — so when a manually
    // run `clauth daemon` holds it, the launchd instance parks here and takes
    // over the instant the manual one exits, keeping the plist's
    // `KeepAlive{SuccessfulExit=false}` valid (a clean exit must never have to be
    // restarted). A dead holder's advisory flock auto-releases, so standby is
    // never orphaned.
    let lock_file = crate::profile::open_state_file(&dir.join(LOCK_FILE))
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
    warn_if_spend_is_uncapped(&config);
    let mut daemon = Daemon::new(config, dir.join(STATUS_FILE));
    daemon.boot();
    logline!(
        "clauth daemon: running (status → {})",
        daemon.status_path.display()
    );
    daemon.run();
    Ok(())
}

/// Say so at boot when a chain member is armed to spend with nothing to stop it:
/// billing enabled is the operator's to know, but "the ceiling you set only
/// gates when spending STARTS" is not something a headless run would ever
/// discover. The TUI warns on the member card; nobody is watching that here.
///
/// Names each member rather than counting them — the operator has to know which
/// account to go fix.
fn warn_if_spend_is_uncapped(config: &crate::profile::AppConfig) {
    let uncapped: Vec<&str> = config
        .state
        .fallback_chain
        .iter()
        .filter_map(|name| config.find(name))
        .filter(|p| crate::fallback::spend_is_uncapped(config, p.max_auto_spend.unwrap_or(0.0)))
        .map(|p| p.name.as_str())
        .collect();
    if !uncapped.is_empty() {
        logline!(
            "clauth daemon: {} can spend with no cap. {}. without one, max spend only gates when \
             billing starts, not when it stops",
            uncapped.join(", "),
            crate::fallback::uncapped_spend_fix(),
        );
    }
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
/// A logged-out shell (see [`live_credentials_are_shell`]) is exempt: an empty
/// login is not "unsaved credentials", and deferring on it wedges every
/// headless switch behind a TUI decision about nothing while running sessions
/// sit at "Login expired" (observed 2026-07-15). An unreadable/torn live file
/// still defers — it may be a CC write in progress. A clauth-owned SYMLINK is
/// exempt too ([`crate::claude::live_login_is_clauth_symlink`]): its content
/// is a profile store by construction, so there is nothing unsaved — and the
/// archive refuses symlinks, so gating on one deadlocks the switch
/// (CLA-SPLIT-3, observed 2026-07-21).
fn active_diverged_unsaved(active: &str) -> bool {
    matches!(
        classify_credentials_link(active).ok(),
        Some(LinkState::Diverged)
    ) && !is_first_login(active).unwrap_or(false)
        && !live_credentials_are_shell()
        && !crate::claude::live_login_is_clauth_symlink()
}

/// Owns the shared `Arc` stores (cloned into the scheduler) plus main-loop-only
/// state. Only the main thread touches `self`; the scheduler and any socket
/// thread hold `Arc` clones of the individual stores.
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
    pending_config_ops: PendingConfigOps,
    refetch_queue: RefetchQueue,
    third_party_tokens: ThirdPartyList,
    third_party_usage_store: ThirdPartyUsageStore,
    third_party_status: ThirdPartyStatusStore,
    shutting_down: Arc<AtomicBool>,
    /// Last-seen reload fingerprint (`profiles.toml` mtime + per-account
    /// config.toml count/newest-mtime) — drives external-change reload. Bumped to
    /// the post-write value after every self-initiated switch so the daemon never
    /// reloads its own write.
    last_reload_fp: ReloadFingerprint,
    /// Epoch-ms of the last completed main-loop tick — the watchdog's liveness
    /// signal (TECH-3). `0` until the first tick completes.
    heartbeat: Arc<AtomicU64>,
    /// Last switch skip/failure reason, surfaced in `status.json` (TECH-6). Sticky
    /// (kept with its timestamp until a newer reason replaces it) so a transient
    /// stall is still visible after it clears. Main-thread-only.
    last_error: Option<LastError>,
    /// Last executed switch, surfaced in `status.json` (TECH-8). Main-thread-only.
    last_switch: Option<LastSwitch>,
    /// Backoff/dedup state for a persistently-failing switch (TECH-8),
    /// keyed BY HARNESS (CDX-4 review MED): the drain attempts one winner per
    /// harness per tick, so a single shared slot ping-ponged between a stuck
    /// claude target and a stuck codex target — each tick the non-slot target
    /// bypassed its `not_before` gate and re-logged, re-arming the 1/tick
    /// storm this backoff exists to kill. One slot per harness keeps them
    /// independent.
    switch_backoff: std::collections::HashMap<crate::profile::Harness, SwitchBackoff>,
    /// Fingerprint of the last live login `follow_live_login` examined and
    /// could not act on (PROVEN-foreign owner). Skips per-tick re-examination
    /// — and re-arms the moment the live login changes. Deliberately NOT set
    /// for probe failures, capture failures, or rescue retries (RESCUE-1/2b):
    /// memoizing a transient outage against the login was how one bad probe
    /// wedged the daemon for good. Main-thread-only; persisted across
    /// restarts via [`FollowState`].
    follow_memo: Option<u64>,
    /// Epoch-ms before which `follow_live_login`'s NETWORK tier (identity
    /// probe + dead-login rescue) stays quiet — the timed-retry half of the
    /// memo split above. `0` = free to probe. Main-thread-only; persisted
    /// across restarts via [`FollowState`] so a respawn can't void the
    /// anti-rotation-storm window.
    follow_retry_at: u64,
    /// Fingerprint of the last duplicate-login set `warn_duplicate_logins`
    /// named (CAP-1 tripwire) — one warning per distinct set, not per tick.
    /// Main-thread-only.
    dup_memo: Option<u64>,
    /// Dedup for `codex_follow_live`'s log-only states (foreign / anchorless /
    /// unparseable live codex login) — one line per distinct live state, not
    /// per tick. NOT persisted: unlike the claude memo, the codex follow does
    /// no network and burns nothing, so a restart re-logging one line is
    /// harmless. Main-thread-only.
    codex_follow_memo: Option<u64>,
    /// Count of ACTUAL failure-log emissions (post-dedup) — the observable proof a
    /// stuck switch isn't logging 1/tick (TECH-8). Read by tests.
    switch_failure_logs: u64,
    status_path: PathBuf,
    /// Wakes the main loop the instant a socket op is enqueued so switches/config
    /// edits/refreshes apply in well under a tick instead of waiting out the ~1s
    /// sleep. Shared with the socket thread via `SocketHandles`.
    waker: Arc<waker::TickWaker>,
}

/// The durable half of the follow/rescue backoff (RESCUE-2b): `follow_memo` +
/// `follow_retry_at` survive a daemon restart, so a respawn (launchd
/// KeepAlive, `pkill` deploys, crash loops) can't void the 30-min anti-storm
/// window and re-spend a single-use refresh token per boot. Values are a
/// token-hash and an epoch-ms instant — no secrets.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct FollowState {
    memo: Option<u64>,
    retry_at: u64,
}

fn follow_state_path() -> Option<PathBuf> {
    crate::profile::clauth_dir()
        .ok()
        .map(|d| d.join("daemon-follow.json"))
}

fn load_follow_state() -> FollowState {
    follow_state_path()
        .filter(|p| p.exists())
        .and_then(|p| crate::profile::read_json_file(&p).ok())
        .unwrap_or_default()
}

/// Best-effort persist, on change only. A failure degrades to the old
/// in-memory-only behavior (backoff lost on restart) — loud, not fatal.
fn save_follow_state(state: FollowState) {
    let Some(path) = follow_state_path() else {
        return;
    };
    let result = serde_json::to_vec(&state)
        .map_err(std::io::Error::other)
        .and_then(|bytes| crate::profile::atomic_write_600(&path, bytes));
    if let Err(e) = result {
        logline!("clauth daemon: could not persist follow state: {e}");
    }
}

impl Daemon {
    fn new(config: AppConfig, status_path: PathBuf) -> Self {
        let follow = load_follow_state();
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
            pending_switch: Arc::new(RankedMutex::new(VecDeque::new())),
            pending_switch_off: Arc::new(RankedMutex::new(false)),
            pending_config_ops: Arc::new(RankedMutex::new(Vec::new())),
            refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
            third_party_tokens,
            third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
            third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
            // Never set by the daemon: process exit IS its shutdown (launchd
            // KeepAlive restarts crashes; the singleton flock releases on
            // exit). The flag exists for `spawn_refresher`'s contract — its
            // real writer is the TUI's quit path.
            shutting_down: Arc::new(AtomicBool::new(false)),
            last_reload_fp: reload_fingerprint(),
            heartbeat: Arc::new(AtomicU64::new(0)),
            last_error: None,
            last_switch: None,
            switch_backoff: std::collections::HashMap::new(),
            follow_memo: follow.memo,
            follow_retry_at: follow.retry_at,
            dup_memo: None,
            codex_follow_memo: None,
            switch_failure_logs: 0,
            status_path,
            waker: Arc::new(waker::TickWaker::default()),
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
        self.spawn_socket();
        self.spawn_tokens_feed();
    }

    /// Launch the `~/.clauth/tokens.json` feed (TOK-3) beside `status.json`.
    /// Resolves both home-relative dirs HERE (main thread) so the detached
    /// loader/pricing/consumer threads never re-resolve `home_dir()`. A dir that
    /// fails to resolve simply skips the feed — token usage is auxiliary and must
    /// never block the scheduler/socket.
    #[cfg(not(test))]
    fn spawn_tokens_feed(&self) {
        let (Ok(clauth_dir), Ok(claude_dir)) =
            (crate::profile::clauth_dir(), crate::profile::claude_dir())
        else {
            return;
        };
        tokens_snapshot::spawn_tokens_feed(clauth_dir, claude_dir);
    }

    /// No token feed under `cfg(test)`: the detached loader threads would outlive
    /// a test's `HOME_OVERRIDE` and their atomic writes would then resolve the
    /// real `~/.clauth`/`~/.claude` (same reason the TUI skips its token/pricing
    /// wiring under test).
    #[cfg(test)]
    fn spawn_tokens_feed(&self) {}

    /// Launch the control-socket listener (`clauthd.sock`) beside `status.json`.
    #[cfg(unix)]
    fn spawn_socket(&self) {
        let sock_path = self.status_path.with_file_name(SOCK_FILE);
        socket::spawn(
            sock_path,
            self.status_path.clone(),
            socket::SocketHandles {
                config: Arc::clone(&self.config),
                pending_switch: Arc::clone(&self.pending_switch),
                pending_config_ops: Arc::clone(&self.pending_config_ops),
                refetch_queue: Arc::clone(&self.refetch_queue),
                waker: Arc::clone(&self.waker),
            },
        );
    }

    /// No control socket on non-unix targets — the daemon still refreshes usage,
    /// auto-switches, and writes `status.json`; only the interactive socket
    /// (snapshot/switch/refresh) is unavailable.
    #[cfg(not(unix))]
    fn spawn_socket(&self) {}

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

    /// Main loop. Writes an initial `status.json` immediately (so a menu bar that
    /// attaches before the first fetch has something to read), then each tick
    /// reloads external config changes, executes queued switches, and rewrites
    /// `status.json`. Runs until the process is killed.
    fn run(&mut self) {
        self.write_status();
        // Stamp the first heartbeat before the watchdog starts so it never trips
        // on the zero-heartbeat boot window, then spawn it (TECH-3).
        self.heartbeat
            .store(crate::usage::now_ms(), Ordering::Relaxed);
        self.spawn_watchdog();
        // daemon.log lives beside status.json; cap it on a ~5-min cadence (and at
        // boot, tick 0) so a pre-fix crash-loop log or a busy period can't grow it
        // unbounded (TECH-12 / #39). The check is a cheap stat that no-ops well
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
            // Wait out the tick interval, but wake the instant a socket op is
            // enqueued so switches/config edits/refreshes land in well under a tick
            // (the timeout still fires the periodic usage-refresh tick).
            self.waker.wait(TICK);
            self.tick();
            self.heartbeat
                .store(crate::usage::now_ms(), Ordering::Relaxed);
            ticks = ticks.wrapping_add(1);
        }
    }

    /// Spawn the anti-wedge watchdog (TECH-3). It observes the main loop's tick
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
        // (`docs/ccsbar/DESIGN.md`), so a refresh-fail streak must not leak into it.
        let streaks_snap: HashMap<String, u32> = self
            .poll_streaks
            .lock()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.rate_limit)).collect())
            .unwrap_or_default();
        // AUTH-2: the in-flight switch target (accepted, not yet applied) — the
        // winner the next drain will attempt (TECH-6 precedence), so the UI shows
        // in-flight truth instead of a timing heuristic. Snapshot + release before
        // the config lock, like the freshness stores above.
        let pending_snap: Option<String> = self
            .pending_switch
            .lock()
            .ok()
            .and_then(|q| select_switch_winner(&q))
            .map(|e| e.target);
        let last_error_snap = self
            .last_error
            .as_ref()
            .map(|e| (e.at_ms, e.message.as_str()));
        let live = LiveSignals {
            status: &status_snap,
            next_refresh: &next_snap,
            streaks: &streaks_snap,
            pending_switch: pending_snap.as_deref(),
            last_error: last_error_snap,
            last_switch: self.last_switch.as_ref(),
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
                if let Err(e) = atomic_write_600_fast(&self.status_path, &json) {
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
