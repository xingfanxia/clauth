use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::lockorder::{RankedMutex, rank};

use super::fetch::{
    FetchError, UsageInfo, fetch_raw, iso_to_epoch_secs, load_disk_cache, now_epoch_secs, now_ms,
    write_disk_cache,
};
// Re-exported into the scheduler namespace so the inline test module (which
// references `super::UsageWindow`) resolves it through this module instead of
// reaching across the parent. Test-only because no scheduler code needs the
// type directly — it flows through `UsageInfo`.
#[cfg(test)]
#[allow(unused_imports)]
use super::fetch::UsageWindow;

/// Default scheduler tick. `spawn_refresher` wakes every second and only
/// performs network work for profiles whose per-profile interval has elapsed.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Baseline refresh interval. Used as the default when no learned value exists
/// and as the quiet-period reset target.
pub(crate) const NORMAL_INTERVAL_MS: u64 = 35_000;

/// AIMD learner bounds and step. The learned value is clamped to [FLOOR, CEILING]
/// after every bump; STEP is the additive decrease per recovery round.
pub(crate) const LEARNED_FLOOR_MS: u64 = 10_000;
pub(crate) const LEARNED_CEILING_MS: u64 = 300_000;
pub(crate) const LEARNED_STEP_MS: u64 = 5_000;

/// After this many ms without a 429, a raised learned interval resets to
/// NORMAL_INTERVAL_MS so a single spike doesn't permanently raise the floor.
pub(crate) const LEARNED_QUIET_RESET_MS: u64 = 5 * 60 * 1_000;

/// Default fallback threshold (must match `fallback::DEFAULT_THRESHOLD`).
const DEFAULT_FALLBACK_THRESHOLD: f64 = 95.0;

/// Distance below the fallback threshold at which the refresher clamps to
/// LEARNED_FLOOR_MS regardless of the learned value.
const NEAR_THRESHOLD_MARGIN: f64 = 5.0;

/// Tolerance for "same five-hour utilization" comparison when deciding a
/// Fresh response is actually a server-side cache hit. Well below display
/// precision but above any plausible f64 round-trip jitter.
const CACHE_HIT_EPSILON: f64 = 1e-9;

/// Estimated TTL of the Anthropic `/usage` server-side cache. Two consecutive
/// Fresh responses with identical five-hour utilization only count as a
/// "server cache hit" when the second one landed within this window — beyond
/// it, an unchanged value just means the user isn't burning tokens, and that
/// must not pull the learner back up toward NORMAL.
pub(crate) const SERVER_CACHE_TTL_ESTIMATE_MS: u64 = 25_000;

// Cache-hit elimination invariant: the cache-hit backoff cap (NORMAL) must
// sit strictly above the server-cache-TTL gate (SERVER_CACHE_TTL_ESTIMATE_MS)
// used by `detect_cache_hit`. If NORMAL ≤ TTL, a profile converging to
// NORMAL would still poll inside the cache window and register hits forever,
// making cache-hit elimination impossible in steady state.
const _: () = assert!(
    NORMAL_INTERVAL_MS > SERVER_CACHE_TTL_ESTIMATE_MS,
    "NORMAL_INTERVAL_MS must exceed SERVER_CACHE_TTL_ESTIMATE_MS for cache-hit elimination to terminate",
);
// Stronger invariant: two consecutive bump_downs from NORMAL must keep `learned`
// at or above the server-cache TTL gate. Without this, the learner oscillates
// between NORMAL and (NORMAL - 2*STEP), repeatedly crossing TTL and producing
// a non-zero steady-state cache-hit rate in idle conditions.
const _: () = assert!(
    NORMAL_INTERVAL_MS.saturating_sub(2 * LEARNED_STEP_MS) >= SERVER_CACHE_TTL_ESTIMATE_MS,
    "two bump_downs from NORMAL must stay at or above the server cache TTL, or the learner oscillates around TTL with a non-zero steady-state cache-hit rate",
);

pub(crate) type UsageStore = Arc<RankedMutex<HashMap<String, UsageInfo>, { rank::USAGE_STORE }>>;
pub(crate) type StatusStore =
    Arc<RankedMutex<HashMap<String, FetchStatus>, { rank::USAGE_STATUS }>>;
pub(crate) type TokenList = Arc<RankedMutex<Vec<TokenEntry>, { rank::TOKENS }>>;

/// Per-profile epoch-ms of the last fetch attempt (cache-rule gating).
pub(crate) type LastFetchedAt = Arc<RankedMutex<HashMap<String, u64>, { rank::LAST_FETCHED }>>;

/// Names pushed here after a successful token rotation are fetched on the very
/// next scheduler tick, bypassing the per-profile cadence.
pub(crate) type RefetchQueue = Arc<RankedMutex<HashSet<String>, { rank::REFETCH_QUEUE }>>;

/// Per-profile learned refresh interval in ms (AIMD cadence).
pub(crate) type LearnedIntervals = Arc<RankedMutex<HashMap<String, u64>, { rank::LEARNED }>>;

/// How many consecutive non-429 fetches each profile has seen since the last backoff.
pub(crate) type ConsecutiveOk = Arc<RankedMutex<HashMap<String, u32>, { rank::OK_COUNT }>>;

/// How many consecutive Fresh fetches with unchanged utilization each profile
/// has seen. Used to detect server-side cache hits and back off when polling
/// faster than the server invalidates. In-memory only; not persisted.
pub(crate) type ConsecutiveCacheHit = Arc<RankedMutex<HashMap<String, u32>, { rank::CACHE_HIT }>>;

/// Epoch-ms of the most recent 429 per profile. Used for quiet-period resets.
pub(crate) type Last429At = Arc<RankedMutex<HashMap<String, u64>, { rank::LAST_429 }>>;

/// Profiles that need an auto-start kick after the fetch revealed no live 5h
/// window. Main thread drains this set on every tick.
pub(crate) type PendingAutoStart = Arc<RankedMutex<HashSet<String>, { rank::PENDING_AUTO_START }>>;

/// Profiles whose 5h window has just expired and need a token rotation.
/// Value: the `resets_at` epoch-secs pinned at detection time so the drain
/// stamps `LastRotatedWindow` with the exact window it acted on, not whatever
/// the store holds when the drain runs (which may already be a newer window).
pub(crate) type PendingWindowRotation =
    Arc<RankedMutex<HashMap<String, i64>, { rank::PENDING_WINDOW_ROTATION }>>;

/// Per-profile `resets_at` epoch-secs we already rotated on, so each expiry
/// fires exactly once.
pub(crate) type LastRotatedWindow =
    Arc<RankedMutex<HashMap<String, i64>, { rank::LAST_ROTATED_WINDOW }>>;

/// Scheduler-computed auto-switch decisions. Posted by the background scheduler
/// when it observes the active profile has crossed its fallback threshold; the
/// UI thread drains in `on_tick` and dispatches a switch worker. Set rather
/// than Vec so duplicate enqueues collapse and a slow drain can't pile up.
pub(crate) type PendingSwitch = Arc<RankedMutex<HashSet<String>, { rank::PENDING_SWITCH }>>;

/// Set true by the scheduler when wrap-off mode decides the whole chain is
/// exhausted with no sink (every threshold below 100%). The UI thread drains it
/// in `on_tick` and turns off all accounts. A bool rather than a set because
/// switch-off is a single global act with no target — repeated sets collapse.
pub(crate) type PendingSwitchOff = Arc<RankedMutex<bool, { rank::PENDING_SWITCH_OFF }>>;

/// Snapshot of one profile's OAuth identity used by the refresher.
#[derive(Clone)]
pub(crate) struct TokenEntry {
    pub(crate) name: String,
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    pub(crate) fallback_threshold: f64,
}

/// Per-profile epoch-ms of the next scheduled fetch. Written by the scheduler
/// after each `partition_due` run so the overview rows can show a countdown
/// without re-running the partition math on the render thread.
pub(crate) type NextRefreshPerProfile =
    Arc<RankedMutex<HashMap<String, u64>, { rank::NEXT_REFRESH }>>;

/// In-flight blocking operation per profile. The overview row shows a spinner
/// in the timer slot instead of a countdown whenever a profile's slot is
/// anything other than `Idle`. The map omits `Idle` entries — absent and
/// `Idle` are equivalent.
///
/// Mutex is leaf-level: never hold across HTTP. Snapshot or per-name
/// read/write only so the UI render thread isn't blocked by a worker.
pub(crate) type ActivityStore =
    Arc<RankedMutex<HashMap<String, ProfileActivity>, { rank::ACTIVITY }>>;

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
    /// The AIMD learner uses this to bump the per-profile interval up.
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
/// `RateLimited` so the AIMD learner can back off; a successful retry still
/// records it because the rate-limit signal was observed this tick. Any other
/// error falls back to the on-disk cache. Pushes `name` onto `refetch` when
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
    if crate::oauth::apply_rotated_tokens_locked(config, name, tok, None).is_err() {
        return bail_to_cache(None);
    }
    let rotated: Option<RotatedTokens> = Some((access.clone(), Some(refresh)));
    match fetch_raw(&access) {
        Ok(info) => {
            // Token rotated and fresh numbers are in hand. A 429 was still
            // observed this tick, so report RateLimited so the learner backs
            // off even though we recovered.
            let status = if saw_429 {
                FetchStatus::RateLimited
            } else {
                FetchStatus::Fresh
            };
            (Some(info), status, rotated)
        }
        Err(FetchError::Status(429)) => {
            // Rotation succeeded, but the retry itself got rate-limited. Let the
            // AIMD learner's RateLimited→bump_up path govern the next poll cadence;
            // pushing to RefetchQueue here would schedule a 1s-floor re-fetch that
            // fights the backoff and risks cycling: rotate→retry-429→enqueue→rotate.
            // The learner's backoff must win — no forced refetch on a 429 retry.
            let (info, _) = load_cached_with_status(name, FetchStatus::RateLimited);
            (info, FetchStatus::RateLimited, rotated)
        }
        Err(_) => {
            // Rotation succeeded but a non-429 transient error stopped the retry.
            // Force a re-fetch on the next tick so we pick up with the new token
            // as soon as possible without waiting the full learned interval.
            if let Ok(mut q) = refetch.lock() {
                q.insert(name.to_string());
            }
            bail_to_cache(rotated)
        }
    }
}

fn five_hour_utilization(info: &UsageInfo) -> Option<f64> {
    info.five_hour.as_ref().map(|w| w.utilization)
}

/// Process-wide counter mixed into the bump-up jitter seed so two profiles
/// bumping in the same tick get distinct jitter — `subsec_nanos` alone often
/// collides under burst load, defeating the whole point of jittering.
static JITTER_COUNTER: AtomicU64 = AtomicU64::new(0);

fn jitter_seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = JITTER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mixed = nanos ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    mixed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407)
}

/// Multiplicative-increase: multiply current by 1.5 and add ±10% jitter,
/// clamped to ceiling. At saturation the result pins to CEILING with no
/// jitter — applying jitter after the clamp would let continued 429s drift
/// the effective ceiling down by half the jitter range.
///
/// The saturation guard fires when `raised + margin >= CEILING` (equivalently
/// `raised >= CEILING * 10 / 11`), not just when `raised >= CEILING`. This
/// ensures jitter is only applied when the full ±10% window fits under the
/// ceiling — without this, upward jitter gets clipped by `.min()` while
/// downward jitter passes through, shifting the mean below `raised`.
pub(crate) fn bump_up(current: u64) -> u64 {
    let raised = current.saturating_mul(3) / 2;
    let margin = raised / 10;
    // Pin to CEILING (no jitter) when the full ±margin window would straddle
    // the ceiling. `LEARNED_CEILING_MS * 10 / 11` is the equivalent threshold.
    if raised.saturating_add(margin) >= LEARNED_CEILING_MS {
        return LEARNED_CEILING_MS;
    }
    let jitter = if margin == 0 {
        0i64
    } else {
        i64::try_from(jitter_seed() % (margin * 2 + 1)).unwrap_or(0)
            - i64::try_from(margin).unwrap_or(0)
    };
    ((raised as i64 + jitter).max(LEARNED_FLOOR_MS as i64) as u64).min(LEARNED_CEILING_MS)
}

/// Additive-decrease: subtract LEARNED_STEP_MS, clamp to floor.
pub(crate) fn bump_down(current: u64) -> u64 {
    current
        .saturating_sub(LEARNED_STEP_MS)
        .max(LEARNED_FLOOR_MS)
}

/// True when an unchanged five-hour utilization should be attributed to the
/// server-side /usage cache rather than to genuine idle. Three conditions:
/// status must be Fresh (other statuses can't carry a "same as last" signal);
/// the poll must have landed inside the server cache window (slower polls
/// would see invalidated cache, so unchanged values mean the user just wasn't
/// burning tokens); both prev and new utilizations must be present and equal
/// within `CACHE_HIT_EPSILON`.
fn detect_cache_hit(
    status: FetchStatus,
    elapsed_ms: u64,
    prev_util: Option<f64>,
    new_util: Option<f64>,
) -> bool {
    if !matches!(status, FetchStatus::Fresh) {
        return false;
    }
    // `>=` is correct: at exactly the TTL boundary the server has had time to
    // invalidate its cache, so equal values mean idle, not a cache hit. This
    // boundary is what makes cache-hit elimination terminate — once the learned
    // interval reaches SERVER_CACHE_TTL_ESTIMATE_MS, polls stop registering as
    // cache hits and the backoff arm stops firing.
    if elapsed_ms >= SERVER_CACHE_TTL_ESTIMATE_MS {
        return false;
    }
    match (prev_util, new_util) {
        (Some(a), Some(b)) => (a - b).abs() < CACHE_HIT_EPSILON,
        _ => false,
    }
}

/// Resolve the effective refresh interval for one profile. Near-threshold always
/// wins with FLOOR (highest urgency). Otherwise returns the learned interval,
/// defaulting to NORMAL when no learned value exists.
///
/// The near-threshold override pins FLOOR (10s), which is below the server
/// cache TTL (25s), so cache hits are intentionally NOT eliminated when a
/// profile is within NEAR_THRESHOLD_MARGIN of its configured fallback threshold.
/// This is the one accepted exception to the "eliminate cache hits in steady
/// state" goal — traded for responsiveness as a profile approaches its fallback
/// limit. It only fires for profiles with a genuinely-configured threshold
/// (> NEAR_THRESHOLD_MARGIN); a zero/unset threshold (0.0) does not trigger it.
fn interval_for(
    entry: &TokenEntry,
    last_5h: Option<f64>,
    learned_intervals: &LearnedIntervals,
) -> u64 {
    // Guard: only apply the near-threshold override when the threshold is
    // genuinely configured (> NEAR_THRESHOLD_MARGIN). Without this guard,
    // a zero/unset threshold (0.0) produces a negative RHS, making the
    // comparison true for any utilization — pinning every such profile to
    // FLOOR (10s) forever and defeating the AIMD learner entirely.
    let near = entry.fallback_threshold > NEAR_THRESHOLD_MARGIN
        && matches!(last_5h, Some(u) if u >= entry.fallback_threshold - NEAR_THRESHOLD_MARGIN);
    if near {
        return LEARNED_FLOOR_MS;
    }
    learned_intervals
        .lock()
        .ok()
        .and_then(|m| m.get(&entry.name).copied())
        .unwrap_or(NORMAL_INTERVAL_MS)
}

/// Which AIMD transition a fetch outcome maps to, computed from
/// `(status, cache_hit, util_changed)` with no locks held. Resolving this
/// before locking keeps the critical section in `update_learner` minimal and
/// makes the five transitions auditable in one place.
///
/// Each variant corresponds 1:1 to an old `match status` arm; the locked
/// region only reads/writes the shared maps, it no longer re-derives the
/// branch from the inputs.
enum LearnerSignal {
    /// `RateLimited`: multiplicative bump-up, reset both counters, stamp 429.
    RateLimit,
    /// `Fresh` + server-cache hit: accumulate cache-hit count, jump above TTL
    /// on the second consecutive hit.
    CacheHit,
    /// `Fresh` + genuinely-changed utilization: the recovery signal; accumulate
    /// ok-count, `bump_down` on the second consecutive change.
    Recovery,
    /// Neutral: idle Fresh at >= TTL, Cached, or Failed. Leave every map alone.
    Neutral,
}

impl LearnerSignal {
    fn classify(status: FetchStatus, cache_hit: bool, util_changed: bool) -> Self {
        match status {
            FetchStatus::RateLimited => Self::RateLimit,
            FetchStatus::Fresh if cache_hit => Self::CacheHit,
            FetchStatus::Fresh if util_changed => Self::Recovery,
            FetchStatus::Fresh => Self::Neutral,
            FetchStatus::Cached | FetchStatus::Failed => Self::Neutral,
        }
    }
}

/// Update the AIMD learner maps for one profile based on the fetch outcome.
/// Called from the scheduler thread; all four maps are shared with the main
/// thread via `Arc<Mutex<...>>` and persisted to `AppState` on shutdown
/// (except `cache_hit_count`, which is in-memory only).
///
/// `cache_hit` is true when a Fresh response carried the same five-hour
/// utilization as the previously stored value AND the poll landed inside the
/// server cache window — the Anthropic usage API has a ~30s server-side cache,
/// so unchanged numbers at FLOOR (10s) mean we're polling faster than the
/// server invalidates, not that the API is healthy.
///
/// `util_changed` is true when this Fresh response carried a five-hour
/// utilization that genuinely differs from the previously stored value (beyond
/// `CACHE_HIT_EPSILON`). It is the recovery signal: only a Fresh whose data
/// actually moved counts as evidence the API is healthy and the interval can
/// be tightened. A Fresh with unchanged utilization at elapsed >= TTL (an idle
/// profile burning no tokens) is NOT a cache hit AND NOT a recovery signal — it
/// is neutral, so the learner leaves it untouched. Without this gate the bare
/// recovery arm fired `bump_down` on every idle poll above TTL, walking the
/// interval down until it dropped below TTL where the cache-hit arm snapped it
/// back up — a permanent oscillation for any genuinely idle profile.
#[allow(clippy::too_many_arguments)]
fn update_learner(
    name: &str,
    status: FetchStatus,
    cache_hit: bool,
    util_changed: bool,
    learned: &LearnedIntervals,
    ok_count: &ConsecutiveOk,
    cache_hit_count: &ConsecutiveCacheHit,
    last_429: &Last429At,
) {
    let now = now_ms();

    // Resolve the transition before taking any lock so the critical section
    // below only touches the shared maps. Pure function of the fetch outcome.
    let signal = LearnerSignal::classify(status, cache_hit, util_changed);

    let (Ok(mut learned_g), Ok(mut ok_g), Ok(mut ch_g), Ok(mut l429_g)) = (
        learned.lock(),
        ok_count.lock(),
        cache_hit_count.lock(),
        last_429.lock(),
    ) else {
        return;
    };

    // Quiet-period reset only fires on a confirmed Fresh: a 5-minute network
    // outage shouldn't undo legitimate backoff from prior 429s. Always remove
    // the stale 429 stamp when the quiet window has elapsed, even if the
    // learner is already at or below NORMAL — otherwise the entry lingers
    // forever on disk.
    //
    // `t429 != 0` guards against a zero sentinel from `now_ms()` returning 0
    // on a badly-skewed clock — a stored 0 would satisfy the elapsed check and
    // spuriously wipe legitimate 429 backoff.
    if matches!(status, FetchStatus::Fresh)
        && let Some(&t429) = l429_g.get(name)
        && t429 != 0
        && now.saturating_sub(t429) >= LEARNED_QUIET_RESET_MS
    {
        let current = learned_g.get(name).copied().unwrap_or(NORMAL_INTERVAL_MS);
        if current > NORMAL_INTERVAL_MS {
            learned_g.insert(name.to_string(), NORMAL_INTERVAL_MS);
        }
        l429_g.remove(name);
    }

    match signal {
        LearnerSignal::RateLimit => {
            let current = learned_g.get(name).copied().unwrap_or(NORMAL_INTERVAL_MS);
            learned_g.insert(name.to_string(), bump_up(current));
            ok_g.insert(name.to_string(), 0);
            ch_g.insert(name.to_string(), 0);
            l429_g.insert(name.to_string(), now);
        }
        // A Fresh response with the same utilization is a server-side cache hit:
        // jump the interval above SERVER_CACHE_TTL_ESTIMATE_MS so the very next
        // poll lands outside the cache window and cache-hit detection stops firing.
        // Convergence guarantee: from FLOOR, at most two consecutive cache-hit polls
        // drive `learned` strictly above SERVER_CACHE_TTL_ESTIMATE_MS; once there,
        // `detect_cache_hit` returns false (elapsed >= TTL) and the arm stops firing.
        // Ceiling is NORMAL, not LEARNED_CEILING — cache hits mean "polling too fast",
        // not "server overloaded". Only ever raise `learned` here; if we're already
        // above the target (e.g. post-429 backoff held above NORMAL), leave it alone.
        // `ConsecutiveOk` is intentionally NOT reset here — cache hits are neutral
        // evidence about API health, so zeroing ok_count would discard post-429
        // recovery progress and require an extra bump_down cycle.
        LearnerSignal::CacheHit => {
            let hits = ch_g.get(name).copied().unwrap_or(0) + 1;
            if hits >= 2 {
                let current = learned_g.get(name).copied().unwrap_or(NORMAL_INTERVAL_MS);
                // Jump strictly above the server-cache TTL so the next poll's elapsed
                // >= TTL and detect_cache_hit returns false. Clamp to NORMAL so cache
                // hits never push above the baseline. Never lower the interval here —
                // a 429-backed raised value above NORMAL must be preserved.
                let target =
                    (SERVER_CACHE_TTL_ESTIMATE_MS + LEARNED_STEP_MS).min(NORMAL_INTERVAL_MS);
                let bumped = current.max(target);
                learned_g.insert(name.to_string(), bumped);
                ch_g.insert(name.to_string(), 0);
                // `ok_g` is deliberately left unchanged — only `RateLimited` resets
                // ConsecutiveOk. Cache hits carry no information about API health.
            } else {
                ch_g.insert(name.to_string(), hits);
            }
        }
        // A Fresh response that carried genuinely changed utilization is the
        // only recovery signal: the API answered AND the user is actively
        // burning tokens, so the interval can be tightened. Two consecutive
        // change-events fire `bump_down`. A real change also wipes the cache-hit
        // accumulator so it can't bridge across an intervening change.
        LearnerSignal::Recovery => {
            let count = ok_g.get(name).copied().unwrap_or(0) + 1;
            if count >= 2 {
                let current = learned_g.get(name).copied().unwrap_or(NORMAL_INTERVAL_MS);
                learned_g.insert(name.to_string(), bump_down(current));
                ok_g.insert(name.to_string(), 0);
            } else {
                ok_g.insert(name.to_string(), count);
            }
            ch_g.insert(name.to_string(), 0);
        }
        // Neutral evidence, leave every counter and the learned interval
        // untouched. Three origins collapse here:
        // - Fresh, not a cache hit, unchanged utilization: an idle poll at
        //   elapsed >= TTL (the user simply isn't burning tokens). The interval
        //   settles at its current value instead of oscillating. `ConsecutiveOk`
        //   is preserved (a real change later still counts toward recovery); `ch`
        //   is left alone (cache hits only accumulate at <TTL, never reached here).
        // - Cached / Failed: network failures and cache fallbacks neither confirm
        //   nor refute API health, so a single blip doesn't wipe legitimate
        //   recovery progress accumulated from prior Fresh responses.
        LearnerSignal::Neutral => {}
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
#[allow(clippy::too_many_arguments)]
fn apply_outcome(
    outcome: FetchOutcome,
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    learned: &LearnedIntervals,
    ok_count: &ConsecutiveOk,
    cache_hit_count: &ConsecutiveCacheHit,
    last_429: &Last429At,
) {
    // Single clock snapshot at function entry, before any I/O. Using one
    // `now_ms()` call here ensures `elapsed_ms` measures the true inter-poll
    // interval rather than inter-poll-plus-disk-write, preventing
    // non-deterministic misclassification near the TTL boundary on slow or
    // variable-latency storage. Also prevents the double-read race under
    // concurrent `fetch_all_into` fan-out: if two `apply_outcome` calls for
    // the same profile interleave, the second would otherwise read the
    // timestamp just written by the first → near-zero `elapsed_ms` → false
    // cache-hit → spurious upward bump.
    let now = now_ms();

    let is_fresh = matches!(
        outcome.status,
        FetchStatus::Fresh | FetchStatus::RateLimited
    );
    if is_fresh && let Some(info) = &outcome.info {
        write_disk_cache(&outcome.name, info);
    }

    // Read prev_util and write the new entry in a single lock acquisition so
    // no concurrent apply_outcome for the same name can slip a write between
    // the two operations and produce a stale prev_util for detect_cache_hit.
    let (prev_util, new_util): (Option<f64>, Option<f64>) = if let Ok(mut s) = store.lock() {
        let prev = s.get(&outcome.name).and_then(five_hour_utilization);
        let new_u = outcome.info.as_ref().and_then(five_hour_utilization);
        if let Some(info) = &outcome.info {
            // Don't clobber newer Fresh data with older Cached snapshots loaded
            // from disk by `fetch_with_rotation`'s fallback path. Cached only
            // fills the store when no entry exists (cold start without network).
            if is_fresh || !s.contains_key(&outcome.name) {
                s.insert(outcome.name.clone(), info.clone());
            }
        }
        (prev, new_u)
    } else {
        (None, outcome.info.as_ref().and_then(five_hour_utilization))
    };

    // Snapshot the previous `last_fetched` value BEFORE any write so
    // `elapsed_ms` is measured against the prior fetch, not the just-written
    // timestamp from an earlier call in the same concurrent batch.
    let elapsed_ms: u64 = last_fetched
        .lock()
        .ok()
        .and_then(|m| m.get(&outcome.name).copied())
        .map(|prev| now.saturating_sub(prev))
        .unwrap_or(u64::MAX);

    let cache_hit = detect_cache_hit(outcome.status, elapsed_ms, prev_util, new_util);

    // Recovery signal: the five-hour utilization genuinely moved since the last
    // stored value (beyond `CACHE_HIT_EPSILON`). Requires both a prior and a new
    // value — a cold start (no prior) carries no change evidence, so it is not a
    // recovery signal. A Fresh with unchanged util is neutral, not recovery; this
    // is what keeps an idle profile from walking its interval down forever.
    let util_changed = matches!(
        (prev_util, new_util),
        (Some(a), Some(b)) if (a - b).abs() >= CACHE_HIT_EPSILON
    );

    // Stamp last_fetched and status together, each in its own short critical
    // section so only one leaf lock is held at a time (never two leaves at
    // once). Acquired in ascending rank order LAST_FETCHED(200) <
    // USAGE_STATUS(350) so the sequence stays consistent with the global order
    // even if a future edit nests them. Both are released before
    // `update_learner` takes LEARNED.
    {
        if let Ok(mut lf) = last_fetched.lock() {
            lf.insert(outcome.name.clone(), now);
        }
        if let Ok(mut st) = status.lock() {
            st.insert(outcome.name.clone(), outcome.status);
        }
    }
    update_learner(
        &outcome.name,
        outcome.status,
        cache_hit,
        util_changed,
        learned,
        ok_count,
        cache_hit_count,
        last_429,
    );
}

/// Force-fetch every entry right now in parallel and write the results into
/// the shared stores. Bypasses the cache rule — used by the startup bootstrap
/// worker and `manual_refresh`. Blocks until all fetches complete. One-shot, so any
/// `rotated` tokens are dropped — the main thread's `reload_if_state_changed`
/// will pick them up from the persisted `credentials.json` shortly.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fetch_all_into(
    config: &crate::profile::ConfigHandle,
    tokens: &[TokenEntry],
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
    learned: &LearnedIntervals,
    ok_count: &ConsecutiveOk,
    cache_hit_count: &ConsecutiveCacheHit,
    last_429: &Last429At,
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
                apply_outcome(
                    outcome,
                    store,
                    status,
                    last_fetched,
                    learned,
                    ok_count,
                    cache_hit_count,
                    last_429,
                );
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
/// every profile whose per-profile interval has elapsed. The effective interval
/// per profile comes from the AIMD learner (stored in `learned`): FLOOR when
/// near the configured fallback threshold, the learned value otherwise, falling
/// back to NORMAL when no learned value exists.
///
/// Also evaluates the fallback chain at the end of every tick. When the active
/// profile has crossed its threshold and a viable target exists, the name is
/// posted to `pending_switch` for the UI thread to dispatch a switch worker.
/// Computing the decision here keeps the 100 ms UI tick free of FS access
/// (`with_state_lock` was previously taken on every `on_tick`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_refresher(
    config: crate::profile::ConfigHandle,
    tokens: TokenList,
    store: UsageStore,
    status: StatusStore,
    next_refresh_per_profile: NextRefreshPerProfile,
    activity: ActivityStore,
    last_fetched: LastFetchedAt,
    pending_window_rotation: PendingWindowRotation,
    last_rotated_window: LastRotatedWindow,
    pending_switch: PendingSwitch,
    pending_switch_off: PendingSwitchOff,
    refetch_queue: RefetchQueue,
    learned: LearnedIntervals,
    ok_count: ConsecutiveOk,
    cache_hit_count: ConsecutiveCacheHit,
    last_429: Last429At,
) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(TICK_INTERVAL);

            let snapshot: Vec<TokenEntry> = match tokens.lock() {
                Ok(t) => t.clone(),
                Err(_) => continue,
            };
            if snapshot.is_empty() {
                continue;
            }

            // Drain names pushed by rotation paths so they bypass the cadence
            // and get fresh numbers on this tick instead of waiting up to 45s.
            let forced: HashSet<String> = refetch_queue
                .lock()
                .ok()
                .map(|mut q| std::mem::take(&mut *q))
                .unwrap_or_default();

            // Decide which entries are due this tick. Per-profile intervals
            // come from the AIMD learner held in `learned`.
            let now = now_ms();
            let (mut due, mut per_profile_next) =
                partition_due(&snapshot, now, &store, &last_fetched, &learned, &activity);

            // Merge forced entries that aren't already scheduled this tick and
            // reflect them in the published map as "due now" (zero countdown).
            // Forced entries still skip Switching and Refreshing profiles — the
            // switch worker owns the TokenList write window, and a concurrent
            // rotate_one holds the single-use refresh token.
            if !forced.is_empty() {
                let switching: HashSet<String> = match activity.lock() {
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
            if let Ok(mut nrpp) = next_refresh_per_profile.lock() {
                nrpp.clone_from(&per_profile_next);
            }

            // Snapshot each profile's 5h `resets_at` as it stands BEFORE this
            // tick's fetches run. A fetch that crosses the boundary returns a
            // fresh window (resets_at ~5h out); reading the store AFTER the fetch
            // would overwrite the expired value and never observe the expiry.
            // Scanning here — above the `due.is_empty()` bail — also covers idle
            // ticks that cross a boundary no fetch would otherwise report.
            let pre_fetch_resets: HashMap<String, i64> = match store.lock() {
                Ok(st) => snapshot
                    .iter()
                    .filter_map(|entry| {
                        let resets_at_str = st
                            .get(&entry.name)
                            .and_then(|u| u.five_hour.as_ref())
                            .and_then(|w| w.resets_at.as_deref())?;
                        Some((entry.name.clone(), iso_to_epoch_secs(resets_at_str)?))
                    })
                    .collect(),
                Err(_) => HashMap::new(),
            };
            scan_expired_windows(
                &pre_fetch_resets,
                &activity,
                &last_rotated_window,
                &pending_window_rotation,
            );

            if due.is_empty() {
                continue;
            }

            // Mark profiles as in-flight so the overview row shows a spinner.
            // Per-name leaf write — the lock is never held across the HTTP
            // round trips below.
            for entry in &due {
                mark_activity(&activity, &entry.name, ProfileActivity::Fetching);
            }

            let handles: Vec<_> = due
                .into_iter()
                .map(|entry| {
                    let name = entry.name.clone();
                    let config = Arc::clone(&config);
                    let refetch_queue = Arc::clone(&refetch_queue);
                    let activity = Arc::clone(&activity);
                    let h = std::thread::spawn(move || {
                        run_fetch(&config, entry, &refetch_queue, &activity)
                    });
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
                        clear_activity(&activity, &outcome.name);
                        // Propagate any rotated OAuth pair back into the live snapshot
                        // before the next tick — otherwise tick N+1 reuses the stale
                        // access token, 401s, rotates again, and burns the refresh-token
                        // chain while waiting for the mtime watch to reload AppConfig.
                        if let Some((new_access, new_refresh)) = &outcome.rotated
                            && let Ok(mut t) = tokens.lock()
                            && let Some(entry) = t.iter_mut().find(|e| e.name == outcome.name)
                        {
                            entry.access_token = new_access.clone();
                            entry.refresh_token = new_refresh.clone();
                        }
                        apply_outcome(
                            outcome,
                            &store,
                            &status,
                            &last_fetched,
                            &learned,
                            &ok_count,
                            &cache_hit_count,
                            &last_429,
                        );
                    }
                    Err(_) => {
                        // Worker panicked. Clear the activity slot so the spinner
                        // doesn't freeze permanently and `any_busy` can resolve.
                        clear_activity(&activity, &name);
                    }
                }
            }

            // Recompute per-profile next times AFTER fetches have updated
            // `last_fetched` so the overview countdowns reflect fresh deadlines.
            // `activity` is passed so a profile that became Switching mid-tick
            // gets the same exclusion treatment here as in the pre-fetch call —
            // its countdown is recomputed from current `last_fetched` rather
            // than carrying a stale pre-fetch value for one extra tick.
            let (_, per_profile_after) = partition_due(
                &snapshot,
                now_ms(),
                &store,
                &last_fetched,
                &learned,
                &activity,
            );
            if let Ok(mut nrpp) = next_refresh_per_profile.lock() {
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
                &config,
                &store,
                &activity,
                &pending_switch,
                &pending_switch_off,
            );
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

/// For each profile in `pre_fetch_resets` (name → 5h `resets_at` epoch-secs
/// captured BEFORE this tick's fetches), check whether the window has expired
/// (current time is at least 5s past `resets_at`) and we haven't already
/// queued a rotation for that specific `resets_at` epoch. Qualifying profiles
/// are pushed into `pending_window_rotation` for the main thread to drain.
///
/// The snapshot is taken pre-fetch on purpose: a fetch crossing the boundary
/// returns a fresh window (resets_at ~5h out), so reading the live store here
/// would overwrite the expired value and the expiry would never be observed.
///
/// Profiles currently `Switching` or `Refreshing` are skipped (same exclusion
/// `partition_due` applies): a rotate/switch worker already holds that
/// profile's single-use refresh token over HTTP, and re-posting it here would
/// let `on_tick` dispatch a second window-rotation worker that double-spends
/// the chain. The pin/stamp semantics are unchanged — a still-expired profile
/// is simply re-enqueued on a later tick once it returns to `Idle`. A poisoned
/// activity mutex fails safe to "all busy" so no entry is posted.
fn scan_expired_windows(
    pre_fetch_resets: &HashMap<String, i64>,
    activity: &ActivityStore,
    last_rotated_window: &LastRotatedWindow,
    pending: &PendingWindowRotation,
) {
    let now = now_epoch_secs();

    // Required acquisition order: activity → (drop) → last_rotated_window →
    // (drop) → pending. Never two leaf mutexes at once.
    let candidates: Vec<(String, i64)> = pre_fetch_resets
        .iter()
        .filter_map(|(name, &resets_at)| {
            // 5s past the window boundary to avoid acting on a window
            // that hasn't fully closed yet.
            if now < resets_at + 5 {
                return None;
            }
            Some((name.clone(), resets_at))
        })
        .collect();

    if candidates.is_empty() {
        return;
    }

    // Drop profiles whose activity slot is Switching/Refreshing — a worker is
    // already holding their refresh token. Scoped on its own so the activity
    // guard drops before `last_rotated_window`. Poisoned mutex => fail safe to
    // "all busy" and post nothing this tick.
    let candidates: Vec<(String, i64)> = {
        let Ok(act) = activity.lock() else { return };
        candidates
            .into_iter()
            .filter(|(name, _)| {
                !matches!(
                    act.get(name),
                    Some(ProfileActivity::Switching | ProfileActivity::Refreshing)
                )
            })
            .collect()
    }; // activity guard drops here

    if candidates.is_empty() {
        return;
    }

    // Filter out already-acted-on windows using `last_rotated_window`, then
    // drop it before acquiring `pending`. Never hold two leaf mutexes at once.
    let to_enqueue: Vec<(String, i64)> = {
        let Ok(lrw) = last_rotated_window.lock() else {
            return;
        };
        candidates
            .into_iter()
            .filter(|(name, resets_at)| lrw.get(name).copied().unwrap_or(0) != *resets_at)
            .collect()
    }; // lrw dropped here

    if to_enqueue.is_empty() {
        return;
    }

    let Ok(mut pend) = pending.lock() else {
        return;
    };
    for (name, resets_at) in to_enqueue {
        // Pin the epoch at detection time. The drain uses this value to stamp
        // `LastRotatedWindow` so it deduplicates the window it actually saw,
        // not a potentially newer one the store holds by the time the drain runs.
        pend.insert(name, resets_at);
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
    store: &UsageStore,
    last_fetched: &LastFetchedAt,
    learned: &LearnedIntervals,
    activity: &ActivityStore,
) -> (Vec<TokenEntry>, HashMap<String, u64>) {
    let Ok(lf) = last_fetched.lock() else {
        return (Vec::new(), HashMap::new());
    };
    // Poisoned store: bail the same way as poisoned last_fetched. Proceeding
    // with no utilization data silently disables the near-threshold FLOOR
    // override — profiles near the limit would poll at NORMAL instead of FLOOR.
    let Ok(st) = store.lock() else {
        return (Vec::new(), HashMap::new());
    };
    let act = activity.lock();

    let mut due = Vec::new();
    let mut per_profile = HashMap::with_capacity(snapshot.len());
    for entry in snapshot {
        let last = lf.get(&entry.name).copied().unwrap_or(0);
        let last_5h = st.get(&entry.name).and_then(five_hour_utilization);
        let interval = interval_for(entry, last_5h, learned);
        let next = last.saturating_add(interval);
        per_profile.insert(entry.name.clone(), next);
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

/// Default fallback threshold used when a profile leaves it unset. Public so
/// `App::collect_tokens` can resolve once at snapshot time instead of every
/// scheduler tick.
pub(crate) const fn default_fallback_threshold() -> f64 {
    DEFAULT_FALLBACK_THRESHOLD
}

#[cfg(test)]
#[path = "../../tests/inline/learned_cadence.rs"]
mod tests;
