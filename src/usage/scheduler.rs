use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crate::lockorder::{RankedMutex, rank};

use super::fetch::{FetchError, UsageInfo, fetch_raw, load_disk_cache, now_ms, write_disk_cache};

/// Scheduler wake interval. Network work only fires for profiles whose cadence has elapsed.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Fixed per-profile refresh interval — no adaptive backoff.
pub(crate) const REFRESH_INTERVAL_MS: u64 = 60_000;

/// Wall-clock instant in epoch-milliseconds. Distinct from [`IntervalMs`] so
/// instants and spans can't be confused. `#[repr(transparent)]` keeps layout
/// identical to the persisted `u64` in any `HashMap<String, u64>`.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub(crate) struct EpochMs(u64);

/// Span of time in milliseconds. Distinct from [`EpochMs`] so "instant" and
/// "span" can't be mixed up. `#[repr(transparent)]` for `u64` layout identity.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub(crate) struct IntervalMs(u64);

impl EpochMs {
    pub(crate) const fn from_millis(ms: u64) -> Self {
        Self(ms)
    }

    pub(crate) const fn as_millis(self) -> u64 {
        self.0
    }

    /// Instant `interval` after this one, saturating.
    pub(crate) const fn saturating_add(self, interval: IntervalMs) -> EpochMs {
        EpochMs(self.0.saturating_add(interval.0))
    }
}

impl IntervalMs {
    pub(crate) const fn from_millis(ms: u64) -> Self {
        Self(ms)
    }
}

pub(crate) type UsageStore = Arc<RankedMutex<HashMap<String, UsageInfo>, rank::UsageStore>>;
pub(crate) type StatusStore = Arc<RankedMutex<HashMap<String, FetchStatus>, rank::UsageStatus>>;
pub(crate) type TokenList = Arc<RankedMutex<Vec<TokenEntry>, rank::Tokens>>;

/// Per-profile epoch-ms of the last fetch attempt (cadence gating).
pub(crate) type LastFetchedAt = Arc<RankedMutex<HashMap<String, EpochMs>, rank::LastFetched>>;

/// Names pushed here after a successful token rotation bypass the cadence on the next tick.
pub(crate) type RefetchQueue = Arc<RankedMutex<HashSet<String>, rank::RefetchQueue>>;

/// Profiles needing an auto-start kick (no live 5h window). Drained by the main thread each tick.
pub(crate) type PendingAutoStart = Arc<RankedMutex<HashSet<String>, rank::PendingAutoStart>>;

/// Auto-switch targets posted by the scheduler when the active profile crosses its threshold.
/// Set (not Vec) so duplicate enqueues collapse. Drained by `on_tick`, which dispatches a switch worker.
pub(crate) type PendingSwitch = Arc<RankedMutex<HashSet<String>, rank::PendingSwitch>>;

/// Set true when wrap-off mode finds the entire chain exhausted (no sink below 100%).
/// Drained by `on_tick` to turn off all accounts. Bool because switch-off is a global act.
pub(crate) type PendingSwitchOff = Arc<RankedMutex<bool, rank::PendingSwitchOff>>;

/// Snapshot of one profile's OAuth identity used by the refresher.
#[derive(Clone)]
pub(crate) struct TokenEntry {
    pub(crate) name: String,
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
}

/// Per-profile next-fetch epoch-ms. Written after each `partition_due` run for
/// overview countdown display without re-running the partition math on the render thread.
pub(crate) type NextRefreshPerProfile = Arc<RankedMutex<HashMap<String, u64>, rank::NextRefresh>>;

/// In-flight op per profile. Overview shows a spinner instead of a countdown when non-`Idle`.
/// Map omits `Idle` entries — absent == `Idle`. Leaf-level: never held across HTTP.
pub(crate) type ActivityStore = Arc<RankedMutex<HashMap<String, ProfileActivity>, rank::Activity>>;

/// In-flight op for one profile. Non-`Idle` shows a spinner in the overview timer slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProfileActivity {
    Idle,
    /// `/usage` HTTP fetch in flight.
    Fetching,
    /// OAuth token rotation in flight.
    Refreshing,
    /// Account switch (FS relink). Latent: switch runs synchronously on the UI thread, so this is never set.
    Switching,
    /// `clauth start` launch path. Phase 1 never sets this (`start::run` runs in a separate process);
    /// Phase 2 wires it when the launch becomes a background worker.
    #[allow(dead_code)]
    Starting,
    /// Background auto-start kick — 1-token Haiku ping.
    AutoStarting,
}

/// Op kind reported through [`OpResult`]. Mirrors non-`Idle` [`ProfileActivity`] variants.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivityKind {
    Fetching,
    Refreshing,
    Switching,
    Starting,
    AutoStarting,
}

impl ActivityKind {
    #[allow(dead_code)]
    pub(crate) fn as_activity(self) -> ProfileActivity {
        match self {
            ActivityKind::Fetching => ProfileActivity::Fetching,
            ActivityKind::Refreshing => ProfileActivity::Refreshing,
            ActivityKind::Switching => ProfileActivity::Switching,
            ActivityKind::Starting => ProfileActivity::Starting,
            ActivityKind::AutoStarting => ProfileActivity::AutoStarting,
        }
    }
}

/// Result of one tracked operation. Drained by `on_tick`, which clears the `ActivityStore`
/// slot and surfaces any error as a toast.
#[derive(Debug)]
pub(crate) struct OpResult {
    pub(crate) name: String,
    pub(crate) kind: ActivityKind,
    pub(crate) outcome: anyhow::Result<()>,
}

pub(crate) type OpResultSender = Sender<OpResult>;
pub(crate) type OpResultReceiver = Receiver<OpResult>;

/// Startup phase transitions from background workers to the UI thread.
/// Drained in `on_tick` so the first paint never waits on network or FS.
#[derive(Debug)]
pub(crate) enum StartupSignal {
    /// Reconcile finished cleanly — credentials in sync or silent continuation.
    ReconcileDone,
    /// Reconcile found credentials diverged from the active profile's stored creds.
    /// UI pushes the Divergence prompt; bootstrap waits for user action.
    /// (No OAuth probe — would spend the single-use refresh token.)
    ReconcileNeedsPrompt { active: String },
    /// Bootstrap finished (refresh + initial fetch + auto-start kicks).
    /// UI rebuilds token snapshot, spawns scheduler, applies usage, runs startup auto-switch.
    BootstrapDone,
}

pub(crate) type StartupSender = Sender<StartupSignal>;
pub(crate) type StartupReceiver = Receiver<StartupSignal>;

/// Mark a profile's activity. Idempotent; pair with [`clear_activity`] on every exit path.
pub(crate) fn mark_activity(store: &ActivityStore, name: &str, activity: ProfileActivity) {
    if let Ok(mut g) = store.lock() {
        if matches!(activity, ProfileActivity::Idle) {
            g.remove(name);
        } else {
            g.insert(name.to_string(), activity);
        }
    }
}

/// Drop a profile to `Idle` (removes the entry; absent == `Idle`).
pub(crate) fn clear_activity(store: &ActivityStore, name: &str) {
    if let Ok(mut g) = store.lock() {
        g.remove(name);
    }
}

/// True iff the profile has no in-flight op. Poisoned mutex fails safe to "busy".
pub(crate) fn is_idle(store: &ActivityStore, name: &str) -> bool {
    match store.lock() {
        Ok(g) => !g.contains_key(name),
        Err(_) => false,
    }
}

/// True iff any profile has an in-flight op. Gates global actions like "rotate all".
pub(crate) fn any_busy(store: &ActivityStore) -> bool {
    match store.lock() {
        Ok(g) => !g.is_empty(),
        Err(_) => true,
    }
}

/// Outcome of the most recent fetch attempt for a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchStatus {
    /// Live API response.
    Fresh,
    /// API failed; numbers come from on-disk cache.
    Cached,
    /// API failed and no cache available.
    Failed,
    /// API returned 429 on the initial call (recorded even when the post-rotation retry succeeded).
    RateLimited,
}

/// Rotated (access, refresh) pair from an in-fetch rotation. Propagated back into
/// `TokenList` so the next tick doesn't re-401 with the stale token and double-burn the chain.
pub(crate) type RotatedTokens = (String, Option<String>);

/// Load disk cache as `(Some, status)` or `(None, Failed)` for the rotation bail-out path.
fn load_cached_with_status(name: &str, status: FetchStatus) -> (Option<UsageInfo>, FetchStatus) {
    match load_disk_cache(name) {
        Some(info) => (Some(info), status),
        None => (None, FetchStatus::Failed),
    }
}

/// Fetch + rotate + retry for one profile. On 401/429: refresh the OAuth pair,
/// persist, retry once. A 429 on the initial call always sets `RateLimited` even
/// when the retry succeeds. Other errors fall back to disk cache. Pushes `name`
/// onto `refetch` when rotation succeeded but the follow-up fetch failed.
/// Returns the rotated pair so the caller can update `TokenList`.
///
/// Flips `activity[name]` to `Refreshing` during `oauth::refresh`, then back to
/// `Fetching` for the retry. Caller owns the initial `Fetching` mark and final `Idle` clear.
fn fetch_with_rotation(
    config: &crate::profile::ConfigHandle,
    name: &str,
    access_token: &str,
    refresh_token: Option<&str>,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
) -> (Option<UsageInfo>, FetchStatus, Option<RotatedTokens>) {
    let saw_429 = match fetch_raw(access_token) {
        Ok(info) => return (Some(info), FetchStatus::Fresh, None),
        Err(FetchError::Status(429)) => true,
        Err(FetchError::Status(401)) => false,
        Err(_) => {
            let (info, status) = load_cached_with_status(name, FetchStatus::Cached);
            return (info, status, None);
        }
    };

    let fallback_status = if saw_429 {
        FetchStatus::RateLimited
    } else {
        FetchStatus::Cached
    };
    // Single bail-out path for all rotation-leg failures. `fallback_status`
    // is computed once above so all abort branches stay consistent.
    let bail_to_cache = |rotated: Option<RotatedTokens>| {
        let (info, status) = load_cached_with_status(name, fallback_status);
        (info, status, rotated)
    };

    let Some(rt) = refresh_token else {
        return bail_to_cache(None);
    };
    // Per-profile rotation lock across the refresh HTTP window — prevents a
    // concurrent `clauth start <name>` from double-spending this single-use token.
    // Blocking acquire is safe: the tick body holds no lock, so no deadlock risk.
    // On acquire failure, fall back to cache rather than refreshing unguarded.
    let Ok(_rotation_guard) = crate::runtime::RotationGuard::acquire(name) else {
        return bail_to_cache(None);
    };
    // Re-check liveness under the guard: a live session owns the chain and will
    // refresh it itself — rotating here would double-spend the single-use token.
    // The guard makes this authoritative (winner stamped its PID file first).
    // `partition_due` excludes Refreshing/Switching but not live external sessions.
    if crate::runtime::has_live_session(name) {
        return bail_to_cache(None);
    }
    // Show refresh spinner during the network round trip, then back to Fetching for the retry.
    mark_activity(activity, name, ProfileActivity::Refreshing);
    let refresh_result = crate::oauth::refresh(rt);
    mark_activity(activity, name, ProfileActivity::Fetching);
    let tok = match refresh_result {
        Ok(t) => t,
        Err(_) => return bail_to_cache(None),
    };
    // Persist under the AppConfig mutex + state lock — matches every other rotation site
    // so a concurrent `rotate_one_inner` can't interleave, and keeps in-memory AppConfig in sync.
    let access = tok.access_token.clone();
    let refresh = tok.refresh_token.clone();
    if crate::oauth::apply_rotated_tokens_locked(config, name, tok).is_err() {
        return bail_to_cache(None);
    }
    let rotated: Option<RotatedTokens> = Some((access.clone(), Some(refresh)));
    match fetch_raw(&access) {
        Ok(info) => {
            // 429 was observed this tick even though we recovered — keep RateLimited.
            let status = if saw_429 {
                FetchStatus::RateLimited
            } else {
                FetchStatus::Fresh
            };
            (Some(info), status, rotated)
        }
        Err(FetchError::Status(429)) => {
            // Retry itself rate-limited. Don't push to RefetchQueue — that risks
            // a rotate→429→enqueue→rotate cycle. Let the fixed cadence govern.
            let (info, _) = load_cached_with_status(name, FetchStatus::RateLimited);
            (info, FetchStatus::RateLimited, rotated)
        }
        Err(_) => {
            // Rotation succeeded but a transient error stopped the retry.
            // Push to RefetchQueue so we retry with the new token next tick
            // rather than waiting the full refresh interval.
            if let Ok(mut q) = refetch.lock() {
                q.insert(name.to_string());
            }
            bail_to_cache(rotated)
        }
    }
}

/// One profile's fetch result, carried back to update shared state.
struct FetchOutcome {
    name: String,
    info: Option<UsageInfo>,
    status: FetchStatus,
    /// Rotated token pair when the fetch path rotated OAuth; propagated into `TokenList`.
    rotated: Option<RotatedTokens>,
}

fn run_fetch(
    config: &crate::profile::ConfigHandle,
    entry: TokenEntry,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
) -> FetchOutcome {
    let (info, status, rotated) = fetch_with_rotation(
        config,
        &entry.name,
        &entry.access_token,
        entry.refresh_token.as_deref(),
        refetch,
        activity,
    );

    FetchOutcome {
        name: entry.name,
        info,
        status,
        rotated,
    }
}

/// Write one outcome into the shared stores. Disk cache written on every fresh response.
fn apply_outcome(
    outcome: FetchOutcome,
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
) {
    let now = EpochMs::from_millis(now_ms());

    let is_fresh = matches!(
        outcome.status,
        FetchStatus::Fresh | FetchStatus::RateLimited
    );
    if is_fresh && let Some(info) = &outcome.info {
        write_disk_cache(&outcome.name, info);
    }

    if let Ok(mut s) = store.lock()
        && let Some(info) = &outcome.info
    {
        // Don't clobber newer Fresh data with a Cached fallback snapshot.
        // Cached only fills the store when no entry exists (cold start).
        if is_fresh || !s.contains_key(&outcome.name) {
            s.insert(outcome.name.clone(), info.clone());
        }
    }

    // Each in its own critical section — one leaf lock at a time.
    // Ascending rank order: LAST_FETCHED(200) < USAGE_STATUS(350).
    if let Ok(mut lf) = last_fetched.lock() {
        lf.insert(outcome.name.clone(), now);
    }
    if let Ok(mut st) = status.lock() {
        st.insert(outcome.name.clone(), outcome.status);
    }
}

/// Force-fetch all entries in parallel, bypassing the cadence. Used by bootstrap
/// and `manual_refresh`. Blocks until all complete. Rotated tokens are dropped —
/// `reload_if_state_changed` will pick them up from `credentials.json` shortly.
pub(crate) fn fetch_all_into(
    config: &crate::profile::ConfigHandle,
    tokens: &[TokenEntry],
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
) {
    if tokens.is_empty() {
        return;
    }

    // Mark all as Fetching before fan-out so the spinner covers the full window.
    // Cleared per-name as each thread joins.
    for entry in tokens {
        mark_activity(activity, &entry.name, ProfileActivity::Fetching);
    }

    let handles: Vec<_> = tokens
        .iter()
        .cloned()
        .map(|entry| {
            let name = entry.name.clone();
            let config = Arc::clone(config);
            let refetch = Arc::clone(refetch);
            let activity = Arc::clone(activity);
            let h = std::thread::spawn(move || run_fetch(&config, entry, &refetch, &activity));
            (name, h)
        })
        .collect();

    for (name, h) in handles {
        match h.join() {
            Ok(outcome) => {
                clear_activity(activity, &outcome.name);
                apply_outcome(outcome, store, status, last_fetched);
            }
            Err(_) => {
                // Worker panicked. Clear the activity slot so the spinner doesn't
                // freeze. No `OpResult` sender here so no toast is emitted.
                clear_activity(activity, &name);
            }
        }
    }
}

/// Background scheduler state. Holds **cloned `Arc`s only** — no live lock guards —
/// so the struct carries no lock rank. `tick` acquires individual mutexes in rank order.
pub(crate) struct SchedulerState {
    config: crate::profile::ConfigHandle,
    tokens: TokenList,
    store: UsageStore,
    status: StatusStore,
    next_refresh_per_profile: NextRefreshPerProfile,
    activity: ActivityStore,
    last_fetched: LastFetchedAt,
    pending_switch: PendingSwitch,
    pending_switch_off: PendingSwitchOff,
    refetch_queue: RefetchQueue,
}

/// One scheduler tick: drain forced refetches, partition due set, fan out fetches,
/// propagate rotated tokens, evaluate auto-switch chain.
fn tick(state: &SchedulerState) {
    let snapshot: Vec<TokenEntry> = match state.tokens.lock() {
        Ok(t) => t.clone(),
        Err(_) => return,
    };
    if snapshot.is_empty() {
        return;
    }

    // Names pushed by rotation paths — bypass cadence and fetch this tick.
    let forced: HashSet<String> = state
        .refetch_queue
        .lock()
        .ok()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default();

    let now = now_ms();
    let (mut due, mut per_profile_next) =
        partition_due(&snapshot, now, &state.last_fetched, &state.activity);

    // Merge forced entries not already due. Still skip Switching/Refreshing —
    // the switch worker owns the TokenList write window; rotate_one_inner owns the refresh token.
    if !forced.is_empty() {
        let switching: HashSet<String> = match state.activity.lock() {
            Ok(a) => a
                .iter()
                .filter(|(_, v)| {
                    matches!(v, ProfileActivity::Switching | ProfileActivity::Refreshing)
                })
                .map(|(n, _)| n.clone())
                .collect(),
            Err(_) => snapshot.iter().map(|e| e.name.clone()).collect(),
        };
        let mut extras: Vec<TokenEntry> = Vec::with_capacity(forced.len());
        for entry in snapshot.iter().filter(|e| {
            forced.contains(&e.name)
                && !switching.contains(&e.name)
                && !due.iter().any(|d| d.name == e.name)
        }) {
            per_profile_next.insert(entry.name.clone(), now);
            extras.push(entry.clone());
        }
        due.extend(extras);
    }

    // Publish after forced merge so the UI doesn't show a countdown for a profile
    // that is actually fetching this tick.
    if let Ok(mut nrpp) = state.next_refresh_per_profile.lock() {
        nrpp.clone_from(&per_profile_next);
    }

    if due.is_empty() {
        return;
    }

    for entry in &due {
        mark_activity(&state.activity, &entry.name, ProfileActivity::Fetching);
    }

    let handles: Vec<_> = due
        .into_iter()
        .map(|entry| {
            let name = entry.name.clone();
            let config = Arc::clone(&state.config);
            let refetch_queue = Arc::clone(&state.refetch_queue);
            let activity = Arc::clone(&state.activity);
            let h =
                std::thread::spawn(move || run_fetch(&config, entry, &refetch_queue, &activity));
            (name, h)
        })
        .collect();
    for (name, h) in handles {
        match h.join() {
            Ok(outcome) => {
                clear_activity(&state.activity, &outcome.name);
                // Propagate rotated tokens back into the live snapshot — otherwise
                // tick N+1 reuses the stale access token, 401s, and double-burns the chain.
                if let Some((new_access, new_refresh)) = &outcome.rotated
                    && let Ok(mut t) = state.tokens.lock()
                    && let Some(entry) = t.iter_mut().find(|e| e.name == outcome.name)
                {
                    entry.access_token = new_access.clone();
                    entry.refresh_token = new_refresh.clone();
                }
                apply_outcome(outcome, &state.store, &state.status, &state.last_fetched);
            }
            Err(_) => {
                // Worker panicked — clear slot so the spinner doesn't freeze.
                clear_activity(&state.activity, &name);
            }
        }
    }

    // Recompute after fetches so countdowns reflect fresh deadlines.
    // Passing `activity` ensures a mid-tick Switching profile is excluded here too.
    let (_, per_profile_after) =
        partition_due(&snapshot, now_ms(), &state.last_fetched, &state.activity);
    if let Ok(mut nrpp) = state.next_refresh_per_profile.lock() {
        nrpp.clone_from(&per_profile_after);
    }

    // Auto-switch: read chain under config mutex only (not across HTTP/state lock).
    // Actual relink is deferred to the UI thread via `pending_switch`.
    scan_auto_switch(
        &state.config,
        &state.store,
        &state.activity,
        &state.pending_switch,
        &state.pending_switch_off,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_refresher(
    config: crate::profile::ConfigHandle,
    tokens: TokenList,
    store: UsageStore,
    status: StatusStore,
    next_refresh_per_profile: NextRefreshPerProfile,
    activity: ActivityStore,
    last_fetched: LastFetchedAt,
    pending_switch: PendingSwitch,
    pending_switch_off: PendingSwitchOff,
    refetch_queue: RefetchQueue,
) {
    let state = SchedulerState {
        config,
        tokens,
        store,
        status,
        next_refresh_per_profile,
        activity,
        last_fetched,
        pending_switch,
        pending_switch_off,
        refetch_queue,
    };
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(TICK_INTERVAL);
            tick(&state);
        }
    });
}

/// Evaluate the fallback chain and queue an auto-switch target.
///
/// Snapshots the chain under `config` mutex (dropped before taking `usage_store`).
/// This split is load-bearing: `App::apply_usage` takes `usage_store` then `config`,
/// so the scheduler must never hold `config` while taking `usage_store`.
fn scan_auto_switch(
    config: &crate::profile::ConfigHandle,
    store: &UsageStore,
    activity: &ActivityStore,
    pending_switch: &PendingSwitch,
    pending_switch_off: &PendingSwitchOff,
) {
    // Skip when a previous decision is still pending. Each lock is acquired
    // and dropped before the next — never two leaf mutexes at once.
    {
        let Ok(p) = pending_switch.lock() else { return };
        if !p.is_empty() {
            return;
        }
    }
    {
        // Pending switch-off not yet applied — skip until UI drains it.
        let Ok(off) = pending_switch_off.lock() else {
            return;
        };
        if *off {
            return;
        }
    }
    {
        let Ok(a) = activity.lock() else { return };
        if a.values().any(|v| matches!(v, ProfileActivity::Switching)) {
            return;
        }
    }

    // Snapshot under `config` only — drop guard before taking `usage_store`.
    let snapshot = {
        let cfg = match config.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        crate::fallback::snapshot_chain(&cfg)
    };
    let Some(snapshot) = snapshot else {
        return;
    };

    match crate::fallback::next_auto_switch_target(&snapshot, store) {
        Some(crate::fallback::SwitchAction::To(name)) => {
            if let Ok(mut p) = pending_switch.lock() {
                p.insert(name);
            }
        }
        Some(crate::fallback::SwitchAction::Off) => {
            if let Ok(mut off) = pending_switch_off.lock() {
                *off = true;
            }
        }
        None => {}
    }
}

/// Split `snapshot` into the due set and a per-profile next-fetch map.
///
/// Poisoned `last_fetched` returns empty rather than `last=0` (which would mark
/// all profiles due — fetch storm). Profiles currently `Switching` or `Refreshing`
/// are excluded to avoid racing the switch worker on `TokenList` or `rotate_one_inner`
/// on the single-use refresh token. Poisoned activity mutex fails safe to excluded.
fn partition_due(
    snapshot: &[TokenEntry],
    now: u64,
    last_fetched: &LastFetchedAt,
    activity: &ActivityStore,
) -> (Vec<TokenEntry>, HashMap<String, u64>) {
    let now = EpochMs::from_millis(now);
    let Ok(lf) = last_fetched.lock() else {
        return (Vec::new(), HashMap::new());
    };
    let act = activity.lock();

    let interval = IntervalMs::from_millis(REFRESH_INTERVAL_MS);
    let mut due = Vec::new();
    let mut per_profile = HashMap::with_capacity(snapshot.len());
    for entry in snapshot {
        let last = lf
            .get(&entry.name)
            .copied()
            .unwrap_or(EpochMs::from_millis(0));
        let next = last.saturating_add(interval);
        per_profile.insert(entry.name.clone(), next.as_millis());
        // Countdown still publishes for excluded profiles — UI shows when they become eligible.
        let excluded = match act.as_ref() {
            Ok(a) => matches!(
                a.get(&entry.name),
                Some(ProfileActivity::Switching | ProfileActivity::Refreshing)
            ),
            Err(_) => true, // Poisoned: fail safe to excluded.
        };
        if excluded {
            continue;
        }
        if now >= next {
            due.push(entry.clone());
        }
    }
    (due, per_profile)
}

#[cfg(test)]
#[path = "../../tests/inline/scheduler.rs"]
mod tests;
