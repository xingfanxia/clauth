use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crate::lockorder::{RankedMutex, rank};

use super::fetch::{FetchError, UsageInfo, fetch_raw, load_disk_cache, now_ms, write_disk_cache};

/// Default scheduler tick. `spawn_refresher` wakes every second and only
/// performs network work for profiles whose refresh interval has elapsed.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Fixed per-profile refresh interval. Every profile is re-fetched this long
/// after its last fetch — there is no adaptive backoff.
pub(crate) const REFRESH_INTERVAL_MS: u64 = 60_000;

/// A wall-clock instant in epoch-milliseconds (the unit produced by `now_ms`).
/// Newtype so an epoch timestamp can never be confused with a duration in the
/// cadence arithmetic. The only way to get an `EpochMs` from a bare `u64` is the
/// explicit constructor. `#[repr(transparent)]` so the on-disk `u64`
/// representation and any `HashMap<String, u64>` round-trip is layout-identical.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub(crate) struct EpochMs(u64);

/// A span of time in milliseconds — the refresh interval or any elapsed
/// duration. Distinct from [`EpochMs`] so "instant" and "span" can't be mixed
/// up. `#[repr(transparent)]` keeps it layout-identical to the persisted `u64`.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub(crate) struct IntervalMs(u64);

impl EpochMs {
    /// Wrap a raw epoch-ms count (e.g. a value loaded from disk).
    pub(crate) const fn from_millis(ms: u64) -> Self {
        Self(ms)
    }

    /// Raw epoch-ms count, for persistence and render-side countdown math.
    pub(crate) const fn as_millis(self) -> u64 {
        self.0
    }

    /// Instant `interval` after this one, saturating (instant + span = instant).
    pub(crate) const fn saturating_add(self, interval: IntervalMs) -> EpochMs {
        EpochMs(self.0.saturating_add(interval.0))
    }
}

impl IntervalMs {
    /// Wrap a raw millisecond span (a const interval or a value from disk).
    pub(crate) const fn from_millis(ms: u64) -> Self {
        Self(ms)
    }
}

pub(crate) type UsageStore = Arc<RankedMutex<HashMap<String, UsageInfo>, rank::UsageStore>>;
pub(crate) type StatusStore = Arc<RankedMutex<HashMap<String, FetchStatus>, rank::UsageStatus>>;
pub(crate) type TokenList = Arc<RankedMutex<Vec<TokenEntry>, rank::Tokens>>;

/// Per-profile epoch-ms of the last fetch attempt (cache-rule gating).
pub(crate) type LastFetchedAt = Arc<RankedMutex<HashMap<String, EpochMs>, rank::LastFetched>>;

/// Names pushed here after a successful token rotation are fetched on the very
/// next scheduler tick, bypassing the per-profile cadence.
pub(crate) type RefetchQueue = Arc<RankedMutex<HashSet<String>, rank::RefetchQueue>>;

/// Profiles that need an auto-start kick after the fetch revealed no live 5h
/// window. Main thread drains this set on every tick.
pub(crate) type PendingAutoStart = Arc<RankedMutex<HashSet<String>, rank::PendingAutoStart>>;

/// Scheduler-computed auto-switch decisions. Posted by the background scheduler
/// when it observes the active profile has crossed its fallback threshold; the
/// UI thread drains in `on_tick` and dispatches a switch worker. Set rather
/// than Vec so duplicate enqueues collapse and a slow drain can't pile up.
pub(crate) type PendingSwitch = Arc<RankedMutex<HashSet<String>, rank::PendingSwitch>>;

/// Set true by the scheduler when wrap-off mode decides the whole chain is
/// exhausted with no sink (every threshold below 100%). The UI thread drains it
/// in `on_tick` and turns off all accounts. A bool rather than a set because
/// switch-off is a single global act with no target — repeated sets collapse.
pub(crate) type PendingSwitchOff = Arc<RankedMutex<bool, rank::PendingSwitchOff>>;

/// Snapshot of one profile's OAuth identity used by the refresher.
#[derive(Clone)]
pub(crate) struct TokenEntry {
    pub(crate) name: String,
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
}

/// Per-profile epoch-ms of the next scheduled fetch. Written by the scheduler
/// after each `partition_due` run so the overview rows can show a countdown
/// without re-running the partition math on the render thread.
pub(crate) type NextRefreshPerProfile = Arc<RankedMutex<HashMap<String, u64>, rank::NextRefresh>>;

/// In-flight blocking operation per profile. The overview row shows a spinner
/// in the timer slot instead of a countdown whenever a profile's slot is
/// anything other than `Idle`. The map omits `Idle` entries — absent and
/// `Idle` are equivalent.
///
/// Mutex is leaf-level: never hold across HTTP. Snapshot or per-name
/// read/write only so the UI render thread isn't blocked by a worker.
pub(crate) type ActivityStore = Arc<RankedMutex<HashMap<String, ProfileActivity>, rank::Activity>>;

/// What's currently happening to one profile. `Idle` means no in-flight work;
/// every other variant is a blocking op the overview row should visualize
/// with a spinner in the per-profile timer slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProfileActivity {
    Idle,
    /// `/usage` HTTP fetch in flight.
    Fetching,
    /// OAuth refresh (rotate access + refresh tokens) in flight.
    Refreshing,
    /// CLI/TUI account switch in flight (relink + pre-switch refresh).
    Switching,
    /// One-shot `clauth start` launch path. Phase 1 doesn't drive this from
    /// any path (`start::run` runs in a separate process); Phase 2 wires it
    /// when the launch becomes a background worker.
    #[allow(dead_code)]
    Starting,
    /// Background auto-start kick — token refresh + 1-token Haiku ping.
    AutoStarting,
}

/// Kind of operation reported through an [`OpResult`]. Mirrors the non-`Idle`
/// variants of [`ProfileActivity`] one-for-one. Phase 1 keeps every refresh
/// path synchronous on the main thread, so the channel stays mostly empty;
/// Phase 2 hands each variant to a real worker.
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
    /// Lift a kind to the matching activity variant.
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

/// Result of one async (or for now: synchronous-but-tracked) operation. The
/// main thread drains the receiver inside `on_tick`, clears the profile's
/// `ActivityStore` slot back to `Idle`, and surfaces any error as a toast.
#[derive(Debug)]
pub(crate) struct OpResult {
    pub(crate) name: String,
    pub(crate) kind: ActivityKind,
    pub(crate) outcome: anyhow::Result<()>,
}

pub(crate) type OpResultSender = Sender<OpResult>;
pub(crate) type OpResultReceiver = Receiver<OpResult>;

/// Control-flow signal from the two startup background workers to the UI
/// thread. Unlike [`OpResult`] (per-profile op completion), these carry the
/// startup phase transitions the event loop sequences on: the reconcile
/// worker's verdict and the bootstrap worker's completion. Drained in
/// `on_tick` so the first paint never waits on the network or an FS walk.
#[derive(Debug)]
pub(crate) enum StartupSignal {
    /// Reconcile worker finished without needing user input — the live
    /// credentials were either in sync or a silent continuation. The UI may
    /// now proceed to bootstrap.
    ReconcileDone,
    /// Reconcile found the live credentials diverged from the active profile's
    /// stored creds. We don't probe the stored chain's liveness (an OAuth
    /// refresh would spend its single-use token), so the UI pushes the
    /// Divergence prompt for `active` and only proceeds to bootstrap once the
    /// user picks an action.
    ReconcileNeedsPrompt { active: String },
    /// Bootstrap worker finished its HTTP work (refresh + initial fetch +
    /// auto-start kicks). The UI rebuilds the token snapshot, spawns the
    /// scheduler, applies usage, and runs the one-shot startup auto-switch.
    /// Per-profile toasts (rotations, auto-starts) and the post-auto-start
    /// re-fetch ride the standard `OpResult` drain, same as the synchronous
    /// predecessor did.
    BootstrapDone,
}

pub(crate) type StartupSender = Sender<StartupSignal>;
pub(crate) type StartupReceiver = Receiver<StartupSignal>;

/// Mark a profile as performing `activity` in the shared store. Idempotent;
/// caller should pair with [`clear_activity`] in every exit path.
pub(crate) fn mark_activity(store: &ActivityStore, name: &str, activity: ProfileActivity) {
    if let Ok(mut g) = store.lock() {
        if matches!(activity, ProfileActivity::Idle) {
            g.remove(name);
        } else {
            g.insert(name.to_string(), activity);
        }
    }
}

/// Drop a profile back to `Idle` (removes the entry entirely; readers treat
/// missing and `Idle` identically).
pub(crate) fn clear_activity(store: &ActivityStore, name: &str) {
    if let Ok(mut g) = store.lock() {
        g.remove(name);
    }
}

/// True iff the profile currently has no in-flight op. Used by input handlers
/// and tick-time enqueue paths to avoid double-triggering an op on a profile
/// that already has one running.
pub(crate) fn is_idle(store: &ActivityStore, name: &str) -> bool {
    match store.lock() {
        Ok(g) => !g.contains_key(name),
        // Poisoned — fail-safe to "busy" so we don't fire a duplicate op.
        Err(_) => false,
    }
}

/// True iff any profile currently has any in-flight op. Used to gate global
/// actions like "rotate all" against concurrent per-profile work.
pub(crate) fn any_busy(store: &ActivityStore) -> bool {
    match store.lock() {
        Ok(g) => !g.is_empty(),
        Err(_) => true,
    }
}

/// Outcome of the most recent usage fetch attempt for a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchStatus {
    /// Live response from the Anthropic API this tick.
    Fresh,
    /// API request failed; the numbers shown come from the on-disk cache.
    Cached,
    /// API request failed and no cache was available — no data to show.
    Failed,
    /// The API returned 429 on the initial call (the signal was observed this
    /// tick regardless of whether the retry after token rotation succeeded).
    /// Surfaced to the overview row as a distinct rate-limited status.
    RateLimited,
}

/// New access + refresh token returned by a successful in-fetch rotation.
/// The scheduler propagates this back into the live `TokenList` so the next
/// tick uses the rotated pair instead of re-401'ing with the stale access
/// token and burning the refresh-token chain by rotating again.
pub(crate) type RotatedTokens = (String, Option<String>);

/// Resolve `load_disk_cache` into the `(Option<UsageInfo>, FetchStatus)` pair
/// that the rotation path expects: `(Some, status)` when the cache has bytes,
/// `(None, Failed)` when it doesn't.
fn load_cached_with_status(name: &str, status: FetchStatus) -> (Option<UsageInfo>, FetchStatus) {
    match load_disk_cache(name) {
        Some(info) => (Some(info), status),
        None => (None, FetchStatus::Failed),
    }
}

/// One profile's fetch + rotate + retry path. On 401/429 we refresh the OAuth
/// pair, persist it, and retry once. A 429 on the initial call sets
/// `RateLimited`; a successful retry still records it because the rate-limit
/// signal was observed this tick. Any other error falls back to the on-disk
/// cache. Pushes `name` onto `refetch` when
/// rotation succeeds but the follow-up fetch failed, so the next scheduler
/// tick re-fetches with the new token. Returns the rotated pair on success
/// so the caller can update the live `TokenList`.
///
/// The inline 401 refresh leg flips `activity[name]` to `Refreshing` for the
/// duration of `oauth::refresh` so the overview row shows a refresh spinner
/// instead of a fetch spinner during the rotation, then back to `Fetching`
/// for the retry. The caller is responsible for the initial `Fetching` mark
/// and the final `Idle` clear.
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
    // Single early-return path for every rotation-leg bail-out: resolve the
    // on-disk cache against `fallback_status` and carry the (already-rotated or
    // not) token pair back. `fallback_status` is computed once above; this keeps
    // the eight branches that abort to cache from drifting apart.
    let bail_to_cache = |rotated: Option<RotatedTokens>| {
        let (info, status) = load_cached_with_status(name, fallback_status);
        (info, status, rotated)
    };

    let Some(rt) = refresh_token else {
        return bail_to_cache(None);
    };
    // Per-profile rotation lock across the refresh HTTP window so an external
    // `clauth start <name>` cannot double-spend this single-use token while our
    // 401-recovery rotation is in flight (same template as `rotate_one`).
    // Blocking acquire is safe in the scheduler thread: the tick body holds no
    // lock (leaf-mutex discipline), so this cannot deadlock against anything
    // the scheduler already holds, and the rotation holder window is short.
    // On acquire failure, fall back to cache rather than refreshing unguarded.
    let Ok(_rotation_guard) = crate::runtime::RotationGuard::acquire(name) else {
        return bail_to_cache(None);
    };
    // Re-check liveness under the guard: a live session holds the chain and
    // will refresh it itself; rotating here would spend the same single-use
    // token and 401 one of the two actors, burning the chain. The guard makes
    // this read authoritative (a session that won the acquire race stamped its
    // PID file before releasing). `partition_due` already excludes
    // `Refreshing`/`Switching`, but nothing excludes a *live external session*.
    if crate::runtime::has_live_session(name) {
        return bail_to_cache(None);
    }
    // Refresh leg: surface a refresh spinner during the network round trip,
    // then drop back to Fetching for the retry. `Idle` is only reached on the
    // scheduler-side clear after this function returns.
    mark_activity(activity, name, ProfileActivity::Refreshing);
    let refresh_result = crate::oauth::refresh(rt);
    mark_activity(activity, name, ProfileActivity::Fetching);
    let tok = match refresh_result {
        Ok(t) => t,
        Err(_) => return bail_to_cache(None),
    };
    // Persist under the AppConfig mutex AND state lock together — matches
    // every other rotation site so a concurrent `rotate_one` writer can't
    // interleave between read and write. Using the shared helper also keeps
    // the in-memory AppConfig in step with the on-disk credentials.
    let access = tok.access_token.clone();
    let refresh = tok.refresh_token.clone();
    if crate::oauth::apply_rotated_tokens_locked(config, name, tok).is_err() {
        return bail_to_cache(None);
    }
    let rotated: Option<RotatedTokens> = Some((access.clone(), Some(refresh)));
    match fetch_raw(&access) {
        Ok(info) => {
            // Token rotated and fresh numbers are in hand. A 429 was still
            // observed this tick, so report RateLimited even though we recovered.
            let status = if saw_429 {
                FetchStatus::RateLimited
            } else {
                FetchStatus::Fresh
            };
            (Some(info), status, rotated)
        }
        Err(FetchError::Status(429)) => {
            // Rotation succeeded, but the retry itself got rate-limited. Don't
            // force a refetch on the next tick: pushing to RefetchQueue here would
            // schedule an immediate re-fetch that risks cycling on a persistent
            // 429 (rotate→retry-429→enqueue→rotate). The fixed cadence governs the
            // next poll instead.
            let (info, _) = load_cached_with_status(name, FetchStatus::RateLimited);
            (info, FetchStatus::RateLimited, rotated)
        }
        Err(_) => {
            // Rotation succeeded but a non-429 transient error stopped the retry.
            // Force a re-fetch on the next tick so we pick up with the new token
            // as soon as possible without waiting the full refresh interval.
            if let Ok(mut q) = refetch.lock() {
                q.insert(name.to_string());
            }
            bail_to_cache(rotated)
        }
    }
}

/// Outcome of one profile's fetch step inside the scheduler tick. Holds the
/// data the scheduler needs to update shared state on the main loop side of
/// the spawned thread.
struct FetchOutcome {
    name: String,
    info: Option<UsageInfo>,
    status: FetchStatus,
    /// New `(access_token, refresh_token)` pair when the fetch path rotated
    /// the OAuth tokens. The scheduler propagates this into the live
    /// `TokenList` so the next tick uses the fresh pair.
    rotated: Option<RotatedTokens>,
}

/// Run a single fetch for one entry.
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

/// Write one outcome into the shared stores. Disk cache is updated on every
/// successful API response so a later process restart can still surface
/// numbers without a fresh API call.
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
        // Don't clobber newer Fresh data with older Cached snapshots loaded
        // from disk by `fetch_with_rotation`'s fallback path. Cached only
        // fills the store when no entry exists (cold start without network).
        if is_fresh || !s.contains_key(&outcome.name) {
            s.insert(outcome.name.clone(), info.clone());
        }
    }

    // Stamp last_fetched and status, each in its own short critical section so
    // only one leaf lock is held at a time (never two leaves at once). Acquired
    // in ascending rank order LAST_FETCHED(200) < USAGE_STATUS(350).
    if let Ok(mut lf) = last_fetched.lock() {
        lf.insert(outcome.name.clone(), now);
    }
    if let Ok(mut st) = status.lock() {
        st.insert(outcome.name.clone(), outcome.status);
    }
}

/// Force-fetch every entry right now in parallel and write the results into
/// the shared stores. Bypasses the cache rule — used by the startup bootstrap
/// worker and `manual_refresh`. Blocks until all fetches complete. One-shot, so any
/// `rotated` tokens are dropped — the main thread's `reload_if_state_changed`
/// will pick them up from the persisted `credentials.json` shortly.
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

    // Mark every entry as Fetching before the thread fan-out so the UI shows
    // a spinner for the full window, then clear on join (or on fetch-with-
    // rotation flipping back through Refreshing). Cleared per-name as each
    // thread joins.
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
                // freeze and `any_busy` can resolve. The panic message is lost here;
                // there is no `OpResult` sender in this path so no toast is emitted.
                clear_activity(activity, &name);
            }
        }
    }
}

/// Background scheduler. Wakes every second and fans out parallel fetches for
/// every profile whose fixed `REFRESH_INTERVAL_MS` cadence has elapsed.
///
/// Also evaluates the fallback chain at the end of every tick. When the active
/// profile has crossed its threshold and a viable target exists, the name is
/// posted to `pending_switch` for the UI thread to dispatch a switch worker.
/// Computing the decision here keeps the 100 ms UI tick free of FS access
/// (`with_state_lock` was previously taken on every `on_tick`).
/// Shared handles the background scheduler thread reads on every tick. Holds
/// **cloned `Arc`s only** — never a live lock guard — so the struct itself
/// carries no lock rank and constructing or passing it can't violate the
/// lock-order discipline in `lockorder.rs`. `tick` borrows it and acquires the
/// individual mutexes in the same order the inline loop did.
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

/// Run one scheduler tick: drain forced refetches, partition the due set, fan
/// out parallel fetches, propagate rotated tokens, and evaluate the auto-switch
/// chain. Returns at each early-bail point (the inline loop used `continue` for
/// the same effect). Lock acquisition order and the `RotationGuard` boundary
/// (inside `run_fetch` → `fetch_with_rotation`) are unchanged from the inline
/// version — this only relocates the body.
fn tick(state: &SchedulerState) {
    let snapshot: Vec<TokenEntry> = match state.tokens.lock() {
        Ok(t) => t.clone(),
        Err(_) => return,
    };
    if snapshot.is_empty() {
        return;
    }

    // Drain names pushed by rotation paths so they bypass the cadence
    // and get fresh numbers on this tick instead of waiting the full interval.
    let forced: HashSet<String> = state
        .refetch_queue
        .lock()
        .ok()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default();

    // Decide which entries are due this tick. Every profile uses the same
    // fixed `REFRESH_INTERVAL_MS` cadence.
    let now = now_ms();
    let (mut due, mut per_profile_next) =
        partition_due(&snapshot, now, &state.last_fetched, &state.activity);

    // Merge forced entries that aren't already scheduled this tick and
    // reflect them in the published map as "due now" (zero countdown).
    // Forced entries still skip Switching and Refreshing profiles — the
    // switch worker owns the TokenList write window, and a concurrent
    // rotate_one holds the single-use refresh token.
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

    // Publish the per-profile next times AFTER the forced merge so the
    // UI countdown doesn't show "in Xs" for a profile that is in fact
    // fetching this very tick.
    if let Ok(mut nrpp) = state.next_refresh_per_profile.lock() {
        nrpp.clone_from(&per_profile_next);
    }

    if due.is_empty() {
        return;
    }

    // Mark profiles as in-flight so the overview row shows a spinner.
    // Per-name leaf write — the lock is never held across the HTTP
    // round trips below.
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
                // Clear the in-flight marker before writing results so the
                // overview row transitions from spinner → fresh countdown atomically
                // from the render thread's perspective (it reads both under separate
                // locks, but a brief flicker to "no spinner + stale timer" is acceptable).
                clear_activity(&state.activity, &outcome.name);
                // Propagate any rotated OAuth pair back into the live snapshot
                // before the next tick — otherwise tick N+1 reuses the stale
                // access token, 401s, rotates again, and burns the refresh-token
                // chain while waiting for the mtime watch to reload AppConfig.
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
                // Worker panicked. Clear the activity slot so the spinner
                // doesn't freeze permanently and `any_busy` can resolve.
                clear_activity(&state.activity, &name);
            }
        }
    }

    // Recompute per-profile next times AFTER fetches have updated
    // `last_fetched` so the overview countdowns reflect fresh deadlines.
    // `activity` is passed so a profile that became Switching mid-tick
    // gets the same exclusion treatment here as in the pre-fetch call —
    // its countdown is recomputed from current `last_fetched` rather
    // than carrying a stale pre-fetch value for one extra tick.
    let (_, per_profile_after) =
        partition_due(&snapshot, now_ms(), &state.last_fetched, &state.activity);
    if let Ok(mut nrpp) = state.next_refresh_per_profile.lock() {
        nrpp.clone_from(&per_profile_after);
    }

    // Auto-switch decision: read the live chain + thresholds under
    // the config mutex (NOT across HTTP, NOT across with_state_lock)
    // and consult `store` for utilization. Posting the target to
    // `pending_switch` defers the actual relink to the UI thread,
    // which dispatches a switch worker. A Switching profile here
    // means the previous decision is still in flight — skip until
    // the worker completes.
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

/// Evaluate the fallback chain and queue an auto-switch target when the active
/// profile has crossed its threshold. Snapshots the chain out of `AppConfig`
/// under its mutex (and drops the guard), then reads utilization from the
/// shared `UsageStore`. The split is load-bearing: `App::apply_usage` takes
/// `usage_store` then `config`, so the scheduler must never hold `config`
/// while taking `usage_store`. Skips when the active profile is already
/// mid-switch.
fn scan_auto_switch(
    config: &crate::profile::ConfigHandle,
    store: &UsageStore,
    activity: &ActivityStore,
    pending_switch: &PendingSwitch,
    pending_switch_off: &PendingSwitchOff,
) {
    // Skip when an auto-switch decision is still pending the UI drain or
    // when any profile is currently Switching — either way the previous
    // decision hasn't landed yet and a duplicate enqueue would be a no-op
    // anyway (set semantics), but checking here avoids the config lock.
    // Each lock is acquired and dropped before the next to avoid holding two
    // leaf mutexes simultaneously.
    {
        let Ok(p) = pending_switch.lock() else { return };
        if !p.is_empty() {
            return;
        }
    }
    {
        // A pending switch-off hasn't been applied yet — re-deciding would just
        // re-set the same bool. Skip until the UI drains it.
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

    // Snapshot under `config` only — no `usage_store` lock here.
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

    // `config` guard is dropped. Safe to lock `usage_store` now.
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

/// Split `snapshot` into the subset due this tick and a per-profile map of
/// next fetch epoch-ms. A poisoned `last_fetched` returns empty rather than
/// falling back to `last=0` (which would mark every profile due → fetch storm).
///
/// `activity` is consulted to exclude profiles currently `Switching`: a
/// concurrent switch worker may be rotating their tokens (`refresh_all` or
/// the relink leg), and the scheduler's 401-rotated path would otherwise race
/// it on the same `TokenList` write site. A poisoned activity mutex fails
/// safe to "treat as Switching" so a duplicate fetch never fires.
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
        // Profiles mid-switch or mid-refresh are excluded from this tick's due
        // set so the scheduler can't race the switch worker on the same
        // TokenList write, and can't race `rotate_one` / `fetch_with_rotation`'s
        // inline refresh leg on the same single-use refresh token.
        // The countdown still publishes — the UI shows when the profile will
        // become eligible once Switching/Refreshing clears.
        let excluded = match act.as_ref() {
            Ok(a) => matches!(
                a.get(&entry.name),
                Some(ProfileActivity::Switching | ProfileActivity::Refreshing)
            ),
            // Poisoned mutex: fail safe to "excluded" so no duplicate fetch.
            Err(_) => true,
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
