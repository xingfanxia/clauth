use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crate::lockorder::{RankedMutex, rank};
use crate::providers::{Provider, ThirdPartyStats};

use super::fetch::{
    FetchError, UsageInfo, UsageWindow, epoch_secs_to_iso, fetch_raw, iso_to_epoch_secs,
    load_disk_cache, now_epoch_secs, now_ms, write_disk_cache,
};

/// Scheduler wake interval. Network work only fires for profiles whose cadence has elapsed.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Hard ceiling on a server-provided `retry-after` so a bogus huge value
/// can't starve a profile's refresh slot.
const MAX_RETRY_AFTER_MS: u64 = 15 * 60 * 1000;

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
    /// Opted into auto-start: the periodic tick opens a 5h window for this
    /// profile (kick) before fetching usage whenever its last-known window lapsed.
    pub(crate) auto_start: bool,
    /// Epoch-ms the access token expires at, when known. Gates the kick's
    /// rotate-on-429 to clock-expired tokens only.
    pub(crate) access_expires_at: Option<i64>,
}

/// Snapshot of one third-party profile identity used by the refresher.
#[derive(Clone)]
pub(crate) struct ThirdPartyEntry {
    pub(crate) name: String,
    pub(crate) provider: Provider,
    pub(crate) api_key: String,
}

/// Profile-name accessor shared by the OAuth and third-party entry types so
/// `partition_due` / `merge_forced` run identically over both.
trait NamedEntry {
    fn name(&self) -> &str;
}

impl NamedEntry for TokenEntry {
    fn name(&self) -> &str {
        &self.name
    }
}

impl NamedEntry for ThirdPartyEntry {
    fn name(&self) -> &str {
        &self.name
    }
}

pub(crate) type ThirdPartyList = Arc<RankedMutex<Vec<ThirdPartyEntry>, rank::ThirdParty>>;
pub(crate) type ThirdPartyUsageStore =
    Arc<RankedMutex<HashMap<String, ThirdPartyStats>, rank::ThirdPartyUsageStore>>;
pub(crate) type ThirdPartyStatusStore =
    Arc<RankedMutex<HashMap<String, FetchStatus>, rank::ThirdPartyStatus>>;

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
}

/// Result of one tracked operation. Drained by `on_tick`, which clears the `ActivityStore`
/// slot and surfaces any error as a toast.
#[derive(Debug)]
pub(crate) struct OpResult {
    pub(crate) name: String,
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
    /// API returned 429 (endpoint-level rate limit); numbers come from on-disk cache.
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

/// Fetch + rotate + retry for one profile. On 401: refresh the OAuth pair,
/// persist, retry once. A 429 never rotates — it's an endpoint-level rate
/// limit (`retry-after: 0`, token-independent), so a refresh can't fix it and
/// would spend the single-use refresh token every tick under a persistent
/// storm; it falls back to disk cache as `RateLimited` and the fixed cadence
/// retries. Other errors fall back to disk cache as `Cached`. Pushes `name`
/// onto `refetch` when rotation succeeded but the follow-up fetch failed.
/// Returns a [`FetchOutcome`]: the rotated pair for the caller's `TokenList`
/// sync, the `from_fetch` provenance flag, and the 429 `retry-after` hint that
/// [`apply_outcome`] turns into a deferred next-fetch slot.
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
) -> FetchOutcome {
    match fetch_raw(access_token) {
        Ok(info) => return FetchOutcome::live(name, info, None),
        // Rate-limited: bail to cache, never rotate (see the doc comment).
        Err(FetchError::RateLimited { retry_after }) => {
            return FetchOutcome::cached(name, FetchStatus::RateLimited, None, retry_after);
        }
        // Expired access token: fall through into the rotation leg.
        Err(FetchError::Status(401)) => {}
        Err(_) => return FetchOutcome::cached(name, FetchStatus::Cached, None, None),
    }

    // Single bail-out path for all rotation-leg failures.
    let bail_to_cache = |rotated: Option<RotatedTokens>| {
        FetchOutcome::cached(name, FetchStatus::Cached, rotated, None)
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
        Ok(info) => FetchOutcome::live(name, info, rotated),
        Err(FetchError::RateLimited { retry_after }) => {
            // Retry itself rate-limited. Don't push to RefetchQueue — that risks
            // a rotate→429→enqueue→rotate cycle. The retry-after deferral governs.
            FetchOutcome::cached(name, FetchStatus::RateLimited, rotated, retry_after)
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
    /// `info` is a live API body (not a disk-cache fallback). Only live bodies
    /// may overwrite the store / disk cache in [`apply_outcome`].
    from_fetch: bool,
    /// Server `retry-after` hint from a 429; [`apply_outcome`] turns it into a
    /// deferred next-fetch slot for this profile.
    retry_after: Option<Duration>,
}

impl FetchOutcome {
    /// A live API body — overwrites the store and disk cache.
    fn live(name: &str, info: UsageInfo, rotated: Option<RotatedTokens>) -> Self {
        Self {
            name: name.to_string(),
            info: Some(info),
            status: FetchStatus::Fresh,
            rotated,
            from_fetch: true,
            retry_after: None,
        }
    }

    /// A disk-cache fallback (`status` downgrades to `Failed` when no cache
    /// exists) — may only cold-fill an absent store entry.
    fn cached(
        name: &str,
        status: FetchStatus,
        rotated: Option<RotatedTokens>,
        retry_after: Option<Duration>,
    ) -> Self {
        let (info, status) = load_cached_with_status(name, status);
        Self {
            name: name.to_string(),
            info,
            status,
            rotated,
            from_fetch: false,
            retry_after,
        }
    }
}

/// True iff we hold a fetched usage entry for `name` whose 5h window is absent
/// or already past its reset — the signal to open a fresh window. An ABSENT
/// store entry (never fetched this run) returns false on purpose: fetch first,
/// kick next tick, so a cold cache never kicks blind on a window that may
/// already be live.
fn window_lapsed(store: &UsageStore, name: &str, now_secs: i64) -> bool {
    let Ok(s) = store.lock() else {
        return false;
    };
    let Some(info) = s.get(name) else {
        return false;
    };
    let live = info
        .five_hour
        .as_ref()
        .and_then(|w| w.resets_at.as_deref())
        .and_then(iso_to_epoch_secs)
        .is_some_and(|resets_at| now_secs < resets_at);
    !live
}

/// Fetch one profile's usage. When `store` is `Some` (the periodic tick) and the
/// profile opted into auto-start, open its 5h window first if the last-known
/// window lapsed — kick (rotating once on 401 OR 429), mark the window open on
/// success, then fetch with the possibly-rotated token. `fetch_all_into`
/// (bootstrap / manual refresh) passes `None` so only the steady-state tick
/// auto-starts.
fn run_fetch(
    config: &crate::profile::ConfigHandle,
    mut entry: TokenEntry,
    store: Option<&UsageStore>,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
) -> FetchOutcome {
    // Auto-start leg: open a window before fetching when this profile opted in
    // and its last-known window has lapsed. The kick may rotate the chain (401
    // OR 429 in this branch only); fold its rotated pair into both the local
    // entry (so the fetch below uses the fresh token, never re-spending) and the
    // returned outcome (so the tick syncs it into the live snapshot).
    let mut kick_rotated: Option<RotatedTokens> = None;
    if entry.auto_start
        && let Some(store) = store
    {
        let now_secs = now_epoch_secs();
        if window_lapsed(store, &entry.name, now_secs) {
            let kicked = crate::oauth::auto_start_kick(
                config,
                &entry.name,
                &entry.access_token,
                entry.refresh_token.as_deref(),
                entry.access_expires_at,
                Some(activity),
            );
            if let Some((access, refresh)) = kicked.rotated.clone() {
                entry.access_token = access;
                entry.refresh_token = refresh;
                kick_rotated = kicked.rotated;
            }
            if kicked.opened {
                mark_window_open(store, &entry.name, now_secs);
            }
        }
    }

    let mut outcome = fetch_with_rotation(
        config,
        &entry.name,
        &entry.access_token,
        entry.refresh_token.as_deref(),
        refetch,
        activity,
    );
    // The fetch's own rotation (if any) supersedes the kick's; otherwise carry
    // the kick's rotated pair back so the tick still syncs the spent chain.
    if outcome.rotated.is_none() {
        outcome.rotated = kick_rotated;
    }
    outcome
}

/// Write one outcome into the shared stores. Disk cache written on every live response.
fn apply_outcome(
    outcome: FetchOutcome,
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    interval_ms: u64,
) {
    let now = EpochMs::from_millis(now_ms());

    // Only a body that came off the live API may overwrite shared state. The
    // 429/cached fallback paths recycle the on-disk snapshot — stamping that
    // as fresh would clobber a newer store entry and re-write the disk cache
    // mtime, freezing the UI (and the auto-start scan) on stale numbers for as
    // long as the rate limit lasts. `status` still surfaces RateLimited/Cached
    // so the staleness stays visible.
    let is_fresh = outcome.from_fetch;
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

    // Server-directed deferral: a 429's `retry-after` stamps this profile's
    // slot `retry_after - interval` into the future, so `partition_due`'s
    // fixed math (due + countdown at `stamp + interval_ms`) lands
    // exactly on `now + retry_after` (capped). Not an adaptive learner — the
    // cadence stays fixed; an explicit server hint defers one profile's next
    // slot once.
    let defer = IntervalMs::from_millis(outcome.retry_after.map_or(0, |ra| {
        (ra.as_millis() as u64)
            .min(MAX_RETRY_AFTER_MS)
            .saturating_sub(interval_ms)
    }));

    // Each in its own critical section — one leaf lock at a time.
    // Ascending rank order: LAST_FETCHED(200) < USAGE_STATUS(350).
    if let Ok(mut lf) = last_fetched.lock() {
        lf.insert(outcome.name.clone(), now.saturating_add(defer));
    }
    if let Ok(mut st) = status.lock() {
        st.insert(outcome.name.clone(), outcome.status);
    }
}

/// Optimistically mark a just-kicked profile's 5h window open in the store. A
/// 200 from the kick endpoint IS the window opening, but `/usage` can
/// rate-limit for minutes afterwards — until a live body lands, the usage tab
/// and the auto-start scan would keep seeing the stale windowless snapshot and
/// re-arm a profile whose window is already running. Utilization starts at 0
/// (the kick is ~1 token); the next live fetch overwrites the synthetic entry
/// with API truth. No-op while the stored window is still live.
fn mark_window_open(store: &UsageStore, name: &str, now_secs: i64) {
    let Ok(mut s) = store.lock() else {
        return;
    };
    let info = s.entry(name.to_string()).or_default();
    let live = info
        .five_hour
        .as_ref()
        .and_then(|w| w.resets_at.as_deref())
        .and_then(iso_to_epoch_secs)
        .is_some_and(|resets_at| now_secs < resets_at);
    if live {
        return;
    }
    info.five_hour = Some(UsageWindow {
        utilization: 0.0,
        resets_at: Some(epoch_secs_to_iso(now_secs + 5 * 3600)),
    });
}

/// Force-fetch all entries in parallel, bypassing the cadence. Used by bootstrap
/// and `manual_refresh`. Blocks until all complete. Rotated tokens are dropped —
/// `reload_if_state_changed` will pick them up from `credentials.json` shortly.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fetch_all_into(
    config: &crate::profile::ConfigHandle,
    tokens: &[TokenEntry],
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
    interval_ms: u64,
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
            let h =
                std::thread::spawn(move || run_fetch(&config, entry, None, &refetch, &activity));
            (name, h)
        })
        .collect();

    for (name, h) in handles {
        match h.join() {
            Ok(outcome) => {
                clear_activity(activity, &outcome.name);
                apply_outcome(outcome, store, status, last_fetched, interval_ms);
            }
            Err(_) => {
                // Worker panicked. Clear the activity slot so the spinner doesn't
                // freeze. No `OpResult` sender here so no toast is emitted.
                clear_activity(activity, &name);
            }
        }
    }
}

/// Collect third-party profiles that have a recognised provider and API key.
pub(crate) fn collect_third_party_entries(
    profiles: &[crate::profile::Profile],
) -> Vec<ThirdPartyEntry> {
    profiles
        .iter()
        .filter_map(|p| {
            let provider = p.provider?;
            let api_key = p.api_key.clone()?;
            Some(ThirdPartyEntry {
                name: p.name.to_string(),
                provider,
                api_key,
            })
        })
        .collect()
}

/// Fetch a pre-partitioned set of due third-party entries and apply outcomes to
/// the third-party stores. Partitioning + countdown publishing happen in `tick`
/// so both legs share one publish window; this leg only fetches.
fn fetch_third_party_due(state: &SchedulerState, due: Vec<ThirdPartyEntry>) {
    let interval_ms = state.refresh_interval.load(Ordering::Relaxed);
    for entry in &due {
        mark_activity(&state.activity, &entry.name, ProfileActivity::Fetching);
    }

    let handles: Vec<_> = due
        .into_iter()
        .map(|entry| {
            let name = entry.name.clone();
            let h = std::thread::spawn(move || {
                crate::providers::fetch_third_party_usage(entry.provider, &entry.api_key)
            });
            (name, h)
        })
        .collect();

    for (name, h) in handles {
        match h.join() {
            Ok(Ok(stats)) => {
                clear_activity(&state.activity, &name);
                crate::providers::write_third_party_disk_cache(&name, &stats);
                if let Ok(mut store) = state.third_party_usage_store.lock() {
                    store.insert(name.clone(), stats);
                }
                if let Ok(mut st) = state.third_party_status.lock() {
                    st.insert(name.clone(), FetchStatus::Fresh);
                }
                stamp_last_fetched(&state.last_fetched, name, None, interval_ms);
            }
            Ok(Err(err)) => {
                clear_activity(&state.activity, &name);
                // Cache cold-fills an absent entry only — never overwrites live
                // store data with disk state (same rule as the OAuth path).
                let cached = crate::providers::load_third_party_disk_cache(&name);
                // A 429 carries the server's `retry-after` and defers the next
                // slot (same server-directed deferral as the OAuth 429 path);
                // any other error falls back to cache without deferring.
                let (status, retry_after) = match &err {
                    crate::providers::ThirdPartyError::RateLimited { retry_after } => {
                        (FetchStatus::RateLimited, *retry_after)
                    }
                    _ if cached.is_some() => (FetchStatus::Cached, None),
                    _ => (FetchStatus::Failed, None),
                };
                if let Some(c) = cached
                    && let Ok(mut store) = state.third_party_usage_store.lock()
                {
                    store.entry(name.clone()).or_insert(c);
                }
                if let Ok(mut st) = state.third_party_status.lock() {
                    st.insert(name.clone(), status);
                }
                stamp_last_fetched(&state.last_fetched, name, retry_after, interval_ms);
            }
            Err(_) => {
                // Worker panicked — clear slot so the spinner doesn't freeze.
                clear_activity(&state.activity, &name);
            }
        }
    }
}

/// Stamp a profile's fetch slot. Normally `now` (so the next deadline reflects
/// fetch duration, mirroring OAuth `apply_outcome`); a 429's `retry-after`
/// stamps `retry_after - interval` ahead so `partition_due`'s fixed
/// `stamp + interval_ms` math lands the next slot on `now + retry_after`
/// (capped by [`MAX_RETRY_AFTER_MS`]).
fn stamp_last_fetched(
    last_fetched: &LastFetchedAt,
    name: String,
    retry_after: Option<Duration>,
    interval_ms: u64,
) {
    let defer = IntervalMs::from_millis(retry_after.map_or(0, |ra| {
        (ra.as_millis() as u64)
            .min(MAX_RETRY_AFTER_MS)
            .saturating_sub(interval_ms)
    }));
    if let Ok(mut lf) = last_fetched.lock() {
        lf.insert(name, EpochMs::from_millis(now_ms()).saturating_add(defer));
    }
}

/// Partition a leg's snapshot into due entries + per-profile countdowns, with
/// forced (cadence-bypassing) names merged in. Empty snapshot → no work, no
/// lock traffic. Shared by both legs so they publish in one window.
fn partition_and_merge<T: NamedEntry + Clone>(
    snapshot: &[T],
    forced: &HashSet<String>,
    state: &SchedulerState,
    now: u64,
    interval_ms: u64,
) -> (Vec<T>, HashMap<String, u64>) {
    if snapshot.is_empty() {
        return (Vec::new(), HashMap::new());
    }
    let (mut due, mut next) = partition_due(
        snapshot,
        now,
        &state.last_fetched,
        &state.activity,
        interval_ms,
    );
    merge_forced(snapshot, forced, &mut due, &mut next, &state.activity, now);
    (due, next)
}

/// Full-replace publish of both legs' countdowns in one lock window. `clear`
/// before `extend` drops any deleted profile's stale key and avoids the
/// mid-tick window where one leg's countdowns are momentarily missing.
fn publish_countdowns(
    nrpp: &NextRefreshPerProfile,
    oauth: HashMap<String, u64>,
    third_party: HashMap<String, u64>,
) {
    if let Ok(mut map) = nrpp.lock() {
        map.clear();
        map.extend(oauth);
        map.extend(third_party);
    }
}

/// Background scheduler state. Holds **cloned `Arc`s only** — no live lock guards —
/// so the struct carries no lock rank. `tick` acquires individual mutexes in rank order.
pub(crate) struct SchedulerState {
    config: crate::profile::ConfigHandle,
    tokens: TokenList,
    store: UsageStore,
    status: StatusStore,
    refresh_interval: Arc<AtomicU64>,
    next_refresh_per_profile: NextRefreshPerProfile,
    activity: ActivityStore,
    last_fetched: LastFetchedAt,
    pending_switch: PendingSwitch,
    pending_switch_off: PendingSwitchOff,
    refetch_queue: RefetchQueue,
    third_party_tokens: ThirdPartyList,
    third_party_usage_store: ThirdPartyUsageStore,
    third_party_status: ThirdPartyStatusStore,
    shutting_down: Arc<AtomicBool>,
}

/// One scheduler tick: drain forced refetches, partition both legs, publish
/// countdowns once, fan out fetches (OAuth + third-party), propagate rotated
/// tokens, evaluate auto-switch chain.
fn tick(state: &SchedulerState) {
    let interval_ms = state.refresh_interval.load(Ordering::Relaxed);

    // Names pushed by rotation or manual refresh — bypass cadence this tick.
    // Drained once and handed to both legs; a forced name only matches the leg
    // whose snapshot owns it, so neither starves the other.
    let forced: HashSet<String> = state
        .refetch_queue
        .lock()
        .ok()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default();

    // Snapshot both legs. A poisoned OAuth lock yields an empty snapshot rather
    // than an early `return` — that would starve the third-party leg and drop
    // its already-drained forced names.
    let oauth_snapshot: Vec<TokenEntry> =
        state.tokens.lock().map(|t| t.clone()).unwrap_or_default();
    let tp_snapshot: Vec<ThirdPartyEntry> = state
        .third_party_tokens
        .lock()
        .map(|t| t.clone())
        .unwrap_or_default();

    // Partition both before either fetches, then publish in one window so the
    // countdown map never shows a leg as momentarily missing (and a deleted
    // profile's stale key is dropped by the full replace).
    let now = now_ms();
    let (oauth_due, oauth_next) =
        partition_and_merge(&oauth_snapshot, &forced, state, now, interval_ms);
    let (tp_due, tp_next) = partition_and_merge(&tp_snapshot, &forced, state, now, interval_ms);
    publish_countdowns(&state.next_refresh_per_profile, oauth_next, tp_next);

    let fetched = !oauth_due.is_empty() || !tp_due.is_empty();

    if !oauth_due.is_empty() {
        for entry in &oauth_due {
            mark_activity(&state.activity, &entry.name, ProfileActivity::Fetching);
        }

        let handles: Vec<_> = oauth_due
            .into_iter()
            .map(|entry| {
                let name = entry.name.clone();
                let config = Arc::clone(&state.config);
                let store = Arc::clone(&state.store);
                let refetch_queue = Arc::clone(&state.refetch_queue);
                let activity = Arc::clone(&state.activity);
                let h = std::thread::spawn(move || {
                    run_fetch(&config, entry, Some(&store), &refetch_queue, &activity)
                });
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
                    apply_outcome(
                        outcome,
                        &state.store,
                        &state.status,
                        &state.last_fetched,
                        interval_ms,
                    );
                }
                Err(_) => {
                    // Worker panicked — clear slot so the spinner doesn't freeze.
                    clear_activity(&state.activity, &name);
                }
            }
        }
    }

    // Third-party providers — same cadence, separate stores. Owns the forced
    // names the OAuth leg didn't consume.
    if !tp_due.is_empty() {
        fetch_third_party_due(state, tp_due);
    }

    // Recompute deadlines after fetches so countdowns reflect fresh stamps —
    // one publish, both legs. Skipped on a fully-idle tick (nothing changed, so
    // the pre-fetch publish already holds).
    if fetched {
        let now = now_ms();
        let (_, oauth_after) = partition_due(
            &oauth_snapshot,
            now,
            &state.last_fetched,
            &state.activity,
            interval_ms,
        );
        let (_, tp_after) = partition_due(
            &tp_snapshot,
            now,
            &state.last_fetched,
            &state.activity,
            interval_ms,
        );
        publish_countdowns(&state.next_refresh_per_profile, oauth_after, tp_after);
    }

    // Auto-switch: evaluate every tick (not only OAuth fetch ticks) so a
    // profile that crossed its threshold is switched immediately, without
    // waiting for the next scheduled fetch. Also checks recovery post-switch-off.
    scan_auto_switch(
        &state.config,
        &state.store,
        &state.activity,
        &state.pending_switch,
        &state.pending_switch_off,
    );
    scan_recovery(&state.config, &state.store, &state.pending_switch);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_refresher(
    config: crate::profile::ConfigHandle,
    tokens: TokenList,
    store: UsageStore,
    status: StatusStore,
    refresh_interval: Arc<AtomicU64>,
    next_refresh_per_profile: NextRefreshPerProfile,
    activity: ActivityStore,
    last_fetched: LastFetchedAt,
    pending_switch: PendingSwitch,
    pending_switch_off: PendingSwitchOff,
    refetch_queue: RefetchQueue,
    third_party_tokens: ThirdPartyList,
    third_party_usage_store: ThirdPartyUsageStore,
    third_party_status: ThirdPartyStatusStore,
    shutting_down: Arc<AtomicBool>,
) {
    let state = SchedulerState {
        config,
        tokens,
        store,
        status,
        refresh_interval,
        next_refresh_per_profile,
        activity,
        last_fetched,
        pending_switch,
        pending_switch_off,
        refetch_queue,
        third_party_tokens,
        third_party_usage_store,
        third_party_status,
        shutting_down,
    };
    std::thread::spawn(move || {
        loop {
            if state.shutting_down.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(TICK_INTERVAL);
            if state.shutting_down.load(Ordering::SeqCst) {
                break;
            }
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
    _activity: &ActivityStore,
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

/// Evaluate recovery after switch-off-all: when no active profile is set,
/// scan the fallback chain for any member whose utilization has dropped
/// below its threshold and queue a switch to the first one found.
///
/// Lock-safe: acquires `config` (rank 400) then drops before `store` (300)
/// and `pending_switch` (1500) — never two tracked locks at once.
fn scan_recovery(
    config: &crate::profile::ConfigHandle,
    store: &UsageStore,
    pending_switch: &PendingSwitch,
) {
    // Skip when a previous switch is still pending.
    if let Ok(p) = pending_switch.lock()
        && !p.is_empty()
    {
        return;
    }

    // Build chain-member snapshot under config lock, then drop before
    // touching store (avoids the config↔store inversion that
    // `next_auto_switch_target` avoids via ChainSnapshot).
    let members: Vec<crate::fallback::ChainMember> = {
        let cfg = match config.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        // Only scan for recovery after switch-off-all (no active profile).
        if cfg.state.active_profile.is_some() {
            return;
        }
        if cfg.state.fallback_chain.is_empty() {
            return;
        }
        cfg.state
            .fallback_chain
            .iter()
            .map(|name| crate::fallback::ChainMember {
                name: name.to_string(),
                threshold: cfg
                    .find(name)
                    .map(crate::fallback::threshold_for)
                    .unwrap_or(crate::fallback::DEFAULT_THRESHOLD),
            })
            .collect()
    };

    if let Some(name) = crate::fallback::find_recovered_member(&members, store)
        && let Ok(mut p) = pending_switch.lock()
    {
        p.insert(name);
    }
}

/// Split `snapshot` into the due set and a per-profile next-fetch map.
///
/// Poisoned `last_fetched` returns empty rather than `last=0` (which would mark
/// all profiles due — fetch storm). Profiles currently `Switching` or `Refreshing`
/// are excluded to avoid racing the switch worker on `TokenList` or `rotate_one_inner`
/// on the single-use refresh token. Poisoned activity mutex fails safe to excluded.
fn partition_due<T: NamedEntry + Clone>(
    snapshot: &[T],
    now: u64,
    last_fetched: &LastFetchedAt,
    activity: &ActivityStore,
    interval_ms: u64,
) -> (Vec<T>, HashMap<String, u64>) {
    let now = EpochMs::from_millis(now);
    let Ok(lf) = last_fetched.lock() else {
        return (Vec::new(), HashMap::new());
    };
    let act = activity.lock();

    let interval = IntervalMs::from_millis(interval_ms);
    let mut due = Vec::new();
    let mut per_profile = HashMap::with_capacity(snapshot.len());
    for entry in snapshot {
        let last = lf
            .get(entry.name())
            .copied()
            .unwrap_or(EpochMs::from_millis(0));
        let next = last.saturating_add(interval);
        per_profile.insert(entry.name().to_string(), next.as_millis());
        // Countdown still publishes for excluded profiles — UI shows when they become eligible.
        let excluded = match act.as_ref() {
            Ok(a) => matches!(a.get(entry.name()), Some(ProfileActivity::Refreshing)),
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

/// Merge forced (cadence-bypassing) entries into `due`. Skips profiles that are
/// `Refreshing` — `rotate_one_inner` owns the refresh token — and entries already due.
fn merge_forced<T: NamedEntry + Clone>(
    snapshot: &[T],
    forced: &HashSet<String>,
    due: &mut Vec<T>,
    per_profile_next: &mut HashMap<String, u64>,
    activity: &ActivityStore,
    now: u64,
) {
    if forced.is_empty() {
        return;
    }
    let switching: HashSet<String> = match activity.lock() {
        Ok(a) => a
            .iter()
            .filter(|(_, v)| matches!(v, ProfileActivity::Refreshing))
            .map(|(n, _)| n.clone())
            .collect(),
        Err(_) => snapshot.iter().map(|e| e.name().to_string()).collect(),
    };
    let mut extras: Vec<T> = Vec::with_capacity(forced.len());
    for entry in snapshot.iter().filter(|e| {
        forced.contains(e.name())
            && !switching.contains(e.name())
            && !due.iter().any(|d| d.name() == e.name())
    }) {
        per_profile_next.insert(entry.name().to_string(), now);
        extras.push(entry.clone());
    }
    due.extend(extras);
}

#[cfg(test)]
#[path = "../../tests/inline/scheduler.rs"]
mod tests;
