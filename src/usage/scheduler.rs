use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crate::lockorder::{RankedMutex, rank};
use crate::logline::logline;
use crate::oauth::RefreshError;
use crate::providers::ThirdPartyStats;

use super::fetch::{
    FetchError, PlanInfo, UsageInfo, UsageWindow, await_request_slot, epoch_secs_to_iso, fetch_raw,
    five_hour_live, iso_to_epoch_secs, now_epoch_secs, now_ms,
};
use crate::profile_cache::{
    THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache, profile_cache_mtime_ms,
    write_profile_cache,
};

/// Scheduler wake interval. Network work only fires for profiles whose cadence has elapsed.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Hard ceiling on a server-provided `retry-after` so a bogus huge value
/// can't starve a profile's refresh slot.
const MAX_RETRY_AFTER_MS: u64 = 15 * 60 * 1000;

/// Widen-only poll deferral for an `auth_broken` profile. Each quarantined
/// poll spends a guaranteed-dead 401 → refresh → 400 pair against the token
/// endpoint, so the cadence stretches to the same ceiling the 429 ladder
/// converges to; the poll stays a (slow) recovery path rather than being
/// excluded outright. Applied at partition time from the live flag — never
/// baked into the `last_fetched` stamp — so a login/adopt/carry lifting the
/// flag snaps the cadence back on the very next tick.
const AUTH_BROKEN_BACKOFF_MS: u64 = MAX_RETRY_AFTER_MS;

/// Base extra backoff applied after a 429 that carries no usable `retry-after`:
/// the first such 429 lands the next slot one interval + this far out. Successive
/// 429s multiply it by [`RATE_LIMIT_BACKOFF_FACTOR`]; a server-provided
/// `retry-after` overrides the whole ladder.
const RATE_LIMIT_MIN_BACKOFF_MS: u64 = 10_000;

/// Per-consecutive-429 multiplier on [`RATE_LIMIT_MIN_BACKOFF_MS`] when the
/// server gives no usable `retry-after`: streak 1 → 10s, 2 → 30s, 3 → 90s,
/// each capped by [`MAX_RETRY_AFTER_MS`]. Stops a sustained rate limit from being
/// re-hit every cadence; the streak resets on the next live fetch.
const RATE_LIMIT_BACKOFF_FACTOR: u64 = 3;

/// Last streak level at which the ACTIVE profile's 429 ladder stays capped at
/// 2× cadence ([`next_slot_deferral`]); deeper streaks release to the full
/// drain ladder. The bound exists because the `/usage` throttle is per-account
/// on requests to `/usage` itself and counts REJECTED polls (the #30
/// learning) — a cap with no release would keep re-filling that window for as
/// long as a genuine storm lasts. At the default 90s cadence this bound buys
/// ~6 dense probes (≈3 min apart) over the storm's first quarter hour — enough
/// to re-discover a recovered endpoint fast — before conceding to the ladder.
pub(crate) const ACTIVE_CAP_MAX_STREAK: u32 = 6;

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

/// Per-profile count of consecutive 429s, driving exponential rate-limit backoff
/// in [`apply_outcome`]. Reset on the next live fetch.
pub(crate) type RateLimitStreaks = Arc<RankedMutex<HashMap<String, u32>, rank::RateLimitStreak>>;

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
    /// Persisted `auth_broken` quarantine at snapshot time; widens the poll
    /// cadence by [`AUTH_BROKEN_BACKOFF_MS`] while set.
    pub(crate) auth_broken: bool,
}

/// Snapshot of one third-party profile identity used by the refresher.
#[derive(Clone)]
pub(crate) struct ThirdPartyEntry {
    pub(crate) name: String,
    pub(crate) target: crate::providers::ThirdPartyTarget,
    pub(crate) api_key: String,
}

/// Profile-name accessor shared by the OAuth and third-party entry types so
/// `partition_due` / `merge_forced` run identically over both.
trait NamedEntry {
    fn name(&self) -> &str;
    /// Widen-only extra deferral added to the fixed cadence at partition time.
    /// Zero for everything but a quarantined OAuth profile.
    fn poll_backoff_ms(&self) -> u64 {
        0
    }
}

impl NamedEntry for TokenEntry {
    fn name(&self) -> &str {
        &self.name
    }

    fn poll_backoff_ms(&self) -> u64 {
        if self.auth_broken {
            AUTH_BROKEN_BACKOFF_MS
        } else {
            0
        }
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
/// Session-scoped (in-memory) set of generic profiles whose last fetch yielded
/// no data, suppressed from the timer until a manual refresh clears them. Never
/// persisted — clears when the TUI process exits. Known providers and 429s are
/// never added (429 keeps the server-directed deferral).
pub(crate) type SuppressedGenericStore = Arc<RankedMutex<HashSet<String>, rank::SuppressedGeneric>>;

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
    /// Marked due this tick but still waiting behind the per-host request throttle
    /// (`REQUEST_SPACING_MS`) — not yet firing HTTP. Flips to `Fetching` the
    /// instant its request clears the gate. Distinguishing this from `Fetching`
    /// keeps a batch of due profiles from all reading as "fetching" while only one
    /// is actually in flight (the rest are queued behind the 5s spacing).
    Queued,
    /// `/usage` HTTP fetch in flight.
    Fetching,
    /// OAuth token rotation in flight.
    Refreshing,
    /// Off-thread AUTH-1 switch gate in flight for this profile (the switch
    /// target). Doubles as the pending-switch state: cleared when the gate's
    /// answer drains on the UI thread.
    Switching,
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

/// True iff any profile's switch gate is in flight. Poisoned mutex fails safe
/// to "busy". Switch entry points refuse while one is pending: a second switch
/// spawned mid-gate could land first and be overturned by the older gate's
/// completion.
pub(crate) fn switch_gate_in_flight(store: &ActivityStore) -> bool {
    match store.lock() {
        Ok(g) => g.values().any(|a| matches!(a, ProfileActivity::Switching)),
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
    match load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE) {
        Some(info) => (Some(info), status),
        None => (None, FetchStatus::Failed),
    }
}

/// A poll-time refresh failure is terminal (the OAuth login dropped for good)
/// only for a revoked/invalid refresh token, not a transient network/5xx/parse
/// blip. Quarantining on a terminal failure surfaces "needs reauth" on this tick
/// instead of serving stale cached usage until the next switch trips
/// `ensure_installable`. Truth table pinned by the scheduler `*_terminal` tests.
fn refresh_failure_is_terminal(err: &RefreshError) -> bool {
    matches!(err, RefreshError::Invalid(_))
}

/// The benign face of a terminal 400: "refresh token not found or invalid" is
/// also the exact response after a single-use double-spend — Claude Code
/// refreshing the active profile's symlinked credentials mid-poll, or another
/// refresher that completed before this tick's guard was acquired (the
/// in-memory `TokenEntry` snapshot predates the guard). Re-read the profile's
/// on-disk credentials (call while STILL holding the rotation guard, so the
/// read is stable): a stored refresh token that DIFFERS from the one we just
/// spent means someone else advanced the chain (tokens are opaque, so this is
/// an inequality check, not an ordering — no writer rewinds the store, and a
/// wrong carry self-corrects next tick) — return that fresh pair for the
/// caller's `TokenList` sync instead of quarantining a healthy account.
/// `None` (unchanged, unreadable, or tokenless) means the 400 was a real
/// revocation.
fn fresher_disk_pair(name: &str, spent_refresh: &str) -> Option<RotatedTokens> {
    let profile = crate::profile::load_profile(name).ok()?;
    let access = profile.access_token()?.to_string();
    let refresh = profile.refresh_token()?.to_string();
    (refresh != spent_refresh).then_some((access, Some(refresh)))
}

/// The carry half of the double-spend guard: when [`fresher_disk_pair`] proves
/// someone else advanced the chain, clear any pre-existing quarantine (the
/// chain is alive, so a standing `auth_broken` is stale — without this, an
/// account recovered by an external re-login would stay excluded from the
/// fallback walk and refused by every switch gate forever), queue a refetch so
/// the next tick polls with the carried pair, and hand back the cached outcome
/// whose `rotated` syncs the caller's `TokenList`. A wrong clear self-corrects:
/// if the carried pair is itself dead, its refresh 400s next tick with the
/// store unchanged and the account re-quarantines.
fn carry_external_rotation(
    config: &crate::profile::ConfigHandle,
    name: &str,
    spent_refresh: &str,
    refetch: &RefetchQueue,
) -> Option<FetchOutcome> {
    let fresh = fresher_disk_pair(name, spent_refresh)?;
    crate::oauth::mark_auth_broken(config, name, false);
    if let Ok(mut q) = refetch.lock() {
        q.insert(name.to_string());
    }
    Some(FetchOutcome::cached(
        name,
        FetchStatus::Cached,
        Some(fresh),
        None,
    ))
}

/// Whether a 429 on the usage fetch is worth rotating for. Mirrors
/// `auth::auto_start_kick`'s 429 gate: a 429 on a still-valid token is a pure
/// endpoint rate limit a refresh can't fix, but a clock-expired token would 401
/// the moment the limit clears — so its 429 masks a token that MUST be refreshed,
/// and that refresh is exactly what surfaces `auth_broken` (AUTH-1) instead of the
/// account hiding behind `RateLimited` forever. Unknown expiry stays conservative
/// (never rotate). Truth table pinned by the scheduler `rate_limited_*` tests.
fn token_clock_expired(access_expires_at: Option<i64>, now_ms: i64) -> bool {
    access_expires_at.is_some_and(|exp| now_ms >= exp)
}

/// Status + server hint for a rotation-leg bail that couldn't complete a
/// refresh (busy guard, live session, missing refresh token, failed refresh).
/// A bail that entered the rotation leg through the clock-expired-429 unmask
/// (`unmask_429` = the 429's `retry-after`) keeps that endpoint-level context
/// — `RateLimited` plus the hint — so `apply_outcome`'s deferral and streak
/// accounting survive the failed attempt; dropping them re-polled a
/// rate-limited endpoint on the plain cadence. A 401-entered bail stays
/// `Cached`.
fn rotation_bail_context(unmask_429: Option<Option<Duration>>) -> (FetchStatus, Option<Duration>) {
    match unmask_429 {
        Some(retry_after) => (FetchStatus::RateLimited, retry_after),
        None => (FetchStatus::Cached, None),
    }
}

/// Floor (ms) for [`active_rotate_lead_ms`] — even on a very short refresh
/// interval, leave a couple of minutes of margin.
const ACTIVE_ROTATE_LEAD_FLOOR_MS: i64 = 180_000;

/// How early the poller rotates the ACTIVE, Keychain-installed profile ahead
/// of its clock expiry — only with the opt-in `preemptive_rotation` toggle
/// (rotation coherence, #1).
///
/// The invariant this maintains is NOT "beat Claude Code's refresh schedule"
/// (any fixed margin would silently lose to a future CC that refreshes
/// earlier): it is **"the Keychain never holds an expired token while the
/// poller is alive."** CC re-reads the Keychain per request and refreshes
/// only when the token it just read looks spent; keep the item fresh and CC
/// has no reason to refresh at all. Three poll intervals (with a floor)
/// guarantees multiple rotation opportunities before expiry, whatever the
/// configured cadence. And correctness never depends on winning: if CC does
/// refresh first — schedule change, clauth downtime, lost race — the poller
/// ADOPTS CC's fresher pair from the live file mirror instead of fighting
/// for the chain (`oauth::try_adopt_live_rotation`).
fn active_rotate_lead_ms(interval_ms: u64) -> i64 {
    ((interval_ms as i64).saturating_mul(3)).max(ACTIVE_ROTATE_LEAD_FLOOR_MS)
}

/// Whether this poll should rotate ahead of expiry instead of waiting for a
/// 401: only with the opt-in `preemptive_rotation` toggle (`enabled`, off by
/// default — stock behavior stays strictly lazy; adoption + mirror-on-rotate
/// carry the correctness, this is an optimization), only for the ACTIVE
/// profile, only while the Keychain mirror is live (elsewhere the live
/// credential IS the profile file via the symlink, so there is no second
/// chain to race), and only inside the lead window. An unknown expiry never
/// rotates proactively (never spend a single-use refresh on a token whose
/// expiry we can't prove).
fn proactive_rotation_due(
    enabled: bool,
    active: bool,
    keychain_live: bool,
    access_expires_at: Option<i64>,
    now_ms: i64,
    interval_ms: u64,
) -> bool {
    enabled
        && active
        && keychain_live
        && access_expires_at.is_some_and(|exp| now_ms + active_rotate_lead_ms(interval_ms) >= exp)
}

/// Whether the macOS Keychain mirror is live — `false` under `cfg(test)` and
/// on every other OS, where the symlinked profile file is the live credential.
#[cfg(target_os = "macos")]
fn keychain_live() -> bool {
    crate::keychain::enabled()
}

#[cfg(not(target_os = "macos"))]
fn keychain_live() -> bool {
    false
}

/// Fetch + rotate + retry for one profile. On 401 — or a 429 on a clock-expired
/// token (the AUTH-1 dead-login unmasking, see [`token_clock_expired`]) — refresh
/// the OAuth pair, persist, retry once. A 429 on a still-valid token bails to disk
/// cache as `RateLimited`; other errors bail as `Cached`. An unmask entry whose
/// refresh can't complete keeps the 429's status + `retry-after`
/// ([`rotation_bail_context`]). Pushes `name` onto
/// `refetch` when rotation succeeded but the follow-up fetch failed. Returns a
/// [`FetchOutcome`]: the rotated pair for the caller's `TokenList` sync, the
/// `from_fetch` provenance flag, and the 429 `retry-after` hint that
/// [`apply_outcome`] turns into a deferred next-fetch slot.
///
/// A second exception to "rotate only on a rejected token": with the opt-in
/// `preemptive_rotation` toggle, the ACTIVE, Keychain-installed profile
/// rotates ahead of expiry (see [`active_rotate_lead_ms`]) so the running
/// `claude` never spends the single-use chain.
fn fetch_with_rotation(
    config: &crate::profile::ConfigHandle,
    entry: &TokenEntry,
    prev_plan: Option<PlanInfo>,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
) -> FetchOutcome {
    let name = entry.name.as_str();
    let access_token = entry.access_token.as_str();
    let refresh_token = entry.refresh_token.as_deref();
    // Rotation coherence (#1): read the active flag, stored expiry, and the
    // preemptive toggle in one short config-lock window; the poll itself must
    // never hold the lock. Config — not the `TokenEntry` snapshot — is the
    // expiry source: a kick that rotated earlier in this tick has already
    // persisted the fresh expiry there, while the entry still carries the
    // pre-kick one, and a stale past expiry here would read as clock-expired
    // and re-spend the just-minted single-use pair.
    let (is_active, access_expires_at, interval_ms, preemptive) = config
        .lock()
        .map(|c| {
            (
                c.is_active(name),
                c.find(name).and_then(|p| p.access_token_expires_at()),
                c.state.refresh_interval_ms,
                c.state.preemptive_rotation,
            )
        })
        .unwrap_or((false, None, 90_000, false));
    let proactive = proactive_rotation_due(
        preemptive,
        is_active,
        keychain_live(),
        access_expires_at,
        now_ms() as i64,
        interval_ms,
    );
    let mut unmask_429: Option<Option<Duration>> = None;
    if !proactive {
        match fetch_raw(name, access_token, prev_plan.clone(), false, Some(activity)) {
            Ok(info) => return FetchOutcome::live(name, info, None),
            // 429 on a still-valid token: an endpoint rate limit, not a token problem —
            // bail to cache (see `token_clock_expired`).
            Err(FetchError::RateLimited { retry_after })
                if !token_clock_expired(access_expires_at, now_ms() as i64) =>
            {
                return FetchOutcome::cached(name, FetchStatus::RateLimited, None, retry_after);
            }
            // 401, or a 429 on a clock-expired token (AUTH-1): fall through to the
            // rotation leg so a dead refresh token surfaces as `auth_broken` rather
            // than staying masked as `RateLimited`. The 429's endpoint-level
            // context rides along so a failed unmask keeps the deferral + streak
            // (see `rotation_bail_context`).
            Err(FetchError::Status(401)) => {}
            Err(FetchError::RateLimited { retry_after }) => unmask_429 = Some(retry_after),
            Err(_) => return FetchOutcome::cached(name, FetchStatus::Cached, None, None),
        }
    }

    let bail_to_cache = |rotated: Option<RotatedTokens>| {
        FetchOutcome::cached(name, FetchStatus::Cached, rotated, None)
    };
    // A rotation bail BEFORE any refresh was spent: reactively the token is
    // already dead, so disk cache is all there is; proactively the token still
    // has >= the lead window of life, so run the plain fetch instead — winning
    // the refresh race must never cost a live usage poll.
    let bail_unrotated = || {
        if proactive {
            match fetch_raw(name, access_token, prev_plan.clone(), false, Some(activity)) {
                Ok(info) => FetchOutcome::live(name, info, None),
                Err(FetchError::RateLimited { retry_after }) => {
                    FetchOutcome::cached(name, FetchStatus::RateLimited, None, retry_after)
                }
                Err(_) => FetchOutcome::cached(name, FetchStatus::Cached, None, None),
            }
        } else {
            let (status, retry_after) = rotation_bail_context(unmask_429);
            FetchOutcome::cached(name, status, None, retry_after)
        }
    };

    // Per-profile rotation lock across the ENTIRE rotation leg — the adopt
    // below mutates the same stored credential fields as a refresh persist,
    // so both hold the same guard as `rotate_one_inner` (guard OUTERMOST,
    // then config mutex + state flock inside). Blocking acquire is safe: the
    // tick body holds no lock, so no deadlock risk. On acquire failure, fall
    // back rather than touching the credentials unguarded.
    let Ok(rotation_guard) = crate::runtime::RotationGuard::acquire(name) else {
        return bail_unrotated();
    };

    // Adopt before spending: when the ACTIVE Keychain profile's live file
    // mirror already holds a FRESHER same-account pair, the running claude
    // rotated first — adopt its pair (identity-guarded) instead of burning
    // OUR single-use refresh token against a family it just superseded.
    // Queue a refetch so the next tick polls with the adopted token; disk
    // cache serves this tick.
    if is_active
        && keychain_live()
        && let Some(adopted) =
            crate::oauth::try_adopt_live_rotation(config, name, &rotation_guard, &|tok| {
                crate::usage::fetch_account_uuid(tok)
            })
    {
        if let Ok(mut q) = refetch.lock() {
            q.insert(name.to_string());
        }
        // Carry the adopted pair as `rotated` so the caller syncs the
        // in-memory TokenList — otherwise the queued refetch would run on the
        // superseded entry and spend the revoked refresh token.
        return FetchOutcome::cached(name, FetchStatus::Cached, Some(adopted), None);
    }

    let Some(rt) = refresh_token else {
        return bail_unrotated();
    };
    // Re-check liveness under the guard: a live session owns the chain and will
    // refresh it itself — rotating here would double-spend the single-use token.
    // The guard makes this authoritative (winner stamped its PID file first).
    // `partition_due` excludes Refreshing/Switching but not live external sessions.
    if crate::runtime::has_live_session(name) {
        return bail_unrotated();
    }
    mark_activity(activity, name, ProfileActivity::Refreshing);
    // `refresh_result` (not `refresh`) so the RefreshError variant survives — the
    // poll needs to tell a dead token (quarantine) from a transient blip (retry).
    let rotation = crate::oauth::refresh_result(rt);
    mark_activity(activity, name, ProfileActivity::Fetching);
    let tok = match rotation {
        Ok(t) => t,
        Err(e) => {
            // A failed refresh on the ACTIVE Keychain profile usually means
            // the live claude rotated first and revoked our copy — one more
            // adopt attempt (its mirror may have JUST surfaced the fresh
            // pair) before falling back. This same path re-runs every poll,
            // so a lagging store self-heals as soon as the mirror catches up
            // (at latest CC's next launch).
            if is_active
                && keychain_live()
                && let Some(adopted) =
                    crate::oauth::try_adopt_live_rotation(config, name, &rotation_guard, &|tok| {
                        crate::usage::fetch_account_uuid(tok)
                    })
            {
                if let Ok(mut q) = refetch.lock() {
                    q.insert(name.to_string());
                }
                // Sync the adopted pair into the TokenList (see the
                // rotation-leg adopt above).
                return FetchOutcome::cached(name, FetchStatus::Cached, Some(adopted), None);
            }
            if refresh_failure_is_terminal(&e) {
                // Double-spend guard before quarantining: if the on-disk pair
                // moved past the token we just spent, another refresher
                // already rotated the chain — carry the fresh pair into the
                // TokenList (clearing any stale quarantine) and retry next
                // tick (disk cache serves this one). Only an
                // unchanged-credentials 400 is a real revocation. The adopt
                // above is the identity-guarded fast path for the macOS
                // Keychain active; this re-read catches every other racer
                // (CC through the symlink, a sibling clauth process). See
                // `carry_external_rotation`.
                if let Some(outcome) = carry_external_rotation(config, name, rt, refetch) {
                    return outcome;
                }
                // A terminal failure (dead refresh token) quarantines the
                // account on this tick; a transient one leaves the flag and
                // retries. See `refresh_failure_is_terminal`.
                crate::oauth::mark_auth_broken(config, name, true);
            }
            return bail_unrotated();
        }
    };
    // Persist under the AppConfig mutex + state lock — matches every other rotation site
    // so a concurrent `rotate_one_inner` can't interleave, and keeps in-memory AppConfig in sync.
    let access = tok.access_token.clone();
    let refresh = tok.refresh_token.clone();
    if crate::oauth::apply_rotated_tokens_locked(config, name, tok).is_err() {
        return bail_to_cache(None);
    }
    // A successful refresh + persist clears any prior auth-broken quarantine
    // (mirrors `ensure_installable`); a no-op when the flag was already clear.
    crate::oauth::mark_auth_broken(config, name, false);
    let rotated: Option<RotatedTokens> = Some((access.clone(), Some(refresh)));
    // Post-rotation retry forces a `/profile` pull: the token just changed, so
    // refresh the plan alongside it (the 401 profile-fetch trigger).
    match fetch_raw(name, &access, prev_plan, true, Some(activity)) {
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

/// Patch a just-opened live 5h window back into a Fresh body that lags it. A
/// kick opens the window before `/usage` reflects it, so a Fresh body fetched in
/// the same tick can still report the window closed; writing it verbatim would
/// re-lapse the window and re-fire the kick. When `fresh` has no live 5h window
/// but `prev` does, keep `prev`'s window; every other field takes the fresh
/// value. A genuine new window (live in `fresh`) or a still-closed `prev` is left
/// untouched.
fn preserve_live_window(
    mut fresh: UsageInfo,
    prev: Option<&UsageInfo>,
    now_secs: i64,
) -> UsageInfo {
    if !five_hour_live(&fresh, now_secs)
        && let Some(prev) = prev
        && five_hour_live(prev, now_secs)
    {
        fresh.five_hour = prev.five_hour.clone();
    }
    fresh
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
    !five_hour_live(info, now_secs)
}

/// Current consecutive-429 streak for `name` (0 when absent or poisoned). Read
/// alone and released before any higher-ranked lock — RATE_LIMIT_STREAK(220)
/// sits below USAGE_STORE(300), so it must not be held across `window_lapsed`.
fn rate_limit_streak(streaks: &RateLimitStreaks, name: &str) -> u32 {
    streaks
        .lock()
        .ok()
        .and_then(|m| m.get(name).copied())
        .unwrap_or(0)
}

/// Whether `run_fetch` should open the 5h window before fetching: the window has
/// lapsed AND we are not mid-429-streak. A streak means the endpoint is already
/// throttling us, and a kick on a still-valid access token can neither rotate
/// nor open anything (see `auto_start_kick`) — re-hitting it every due slot only
/// adds load and can prolong the limit. The `/usage` retry detects recovery;
/// once a live body resets the streak, the next lapsed tick opens cleanly.
fn should_open_window(streak: u32, window_lapsed: bool) -> bool {
    window_lapsed && streak == 0
}

/// Fetch one profile's usage on the periodic tick. When the profile opted into
/// auto-start, open its 5h window first if the last-known window lapsed AND no
/// 429 streak is in flight — kick (rotating once on 401 OR 429), mark the window
/// open on success, then fetch with the possibly-rotated token.
fn run_fetch(
    config: &crate::profile::ConfigHandle,
    mut entry: TokenEntry,
    store: &UsageStore,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
    streaks: &RateLimitStreaks,
) -> FetchOutcome {
    // Auto-start leg: open a window before fetching when this profile opted in,
    // its last-known window has lapsed, and we aren't already 429-streaking (see
    // `should_open_window`). The kick may rotate the chain (401 OR 429 in this
    // branch only); fold its rotated pair into both the local entry (so the
    // fetch below uses the fresh token, never re-spending) and the returned
    // outcome (so the tick syncs it into the live snapshot).
    let mut kick_rotated: Option<RotatedTokens> = None;
    if entry.auto_start {
        let streak = rate_limit_streak(streaks, &entry.name);
        let now_secs = now_epoch_secs();
        if should_open_window(streak, window_lapsed(store, &entry.name, now_secs)) {
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

    // Prior plan for the TTL'd `/profile` policy, read from the live store and
    // released before the fetch so no lock is held across HTTP.
    let prev_plan = store
        .lock()
        .ok()
        .and_then(|m| m.get(&entry.name).and_then(|i| i.plan.clone()));

    let mut outcome = fetch_with_rotation(config, &entry, prev_plan, refetch, activity);
    // The fetch's own rotation (if any) supersedes the kick's; otherwise carry
    // the kick's rotated pair back so the tick still syncs the spent chain.
    if outcome.rotated.is_none() {
        outcome.rotated = kick_rotated;
    }
    outcome
}

/// Extra backoff (ms) for the `streak`-th consecutive 429 with no usable hint:
/// `base * factor^(streak - 1)`, saturating. The ceiling is applied by
/// [`next_slot_deferral`].
fn rate_limit_backoff_ms(streak: u32) -> u64 {
    let exp = streak.saturating_sub(1);
    RATE_LIMIT_MIN_BACKOFF_MS.saturating_mul(RATE_LIMIT_BACKOFF_FACTOR.saturating_pow(exp))
}

/// Deferral added to a profile's `last_fetched` stamp so `partition_due`'s fixed
/// `stamp + interval` math lands the next slot correctly. On a 429 the slot is
/// `max(server retry-after, one interval + `[`rate_limit_backoff_ms`]`)` —
/// a REAL long hint is honored verbatim, but a `0` / sub-cadence hint can
/// never suppress the streak ladder. The usage endpoint answers EVERY 429
/// with `retry-after: 0` while its sliding window counts the rejected
/// requests too; taking that "retry now" at face value re-polls at cadence,
/// keeps the window pinned full, and the profile never leaves `RateLimited`
/// (observed 2026-07-11: hours of uninterrupted per-account 429s that only a
/// growing back-off can drain). Capped at [`MAX_RETRY_AFTER_MS`]. Non-429
/// outcomes: no defer.
///
/// The ACTIVE profile's ladder caps at one extra interval (2× cadence) while
/// the streak is shallow (≤ [`ACTIVE_CAP_MAX_STREAK`]): a deep slot on the row
/// the user is watching mostly buys staleness (observed 2026-07-12: the
/// endpoint recovered while the active account sat out a 14-minute slot as
/// `RateLimited`). The cap must NOT be unconditional: the `/usage` window is
/// filled only by clauth's own polls — the running claude's `/v1/messages`
/// traffic never touches it — so on a SUSTAINED storm capped ~2×-cadence
/// re-polls would keep the window pinned (the exact #30 failure); past the
/// bound the active row climbs the same drain ladder as everyone else. A REAL
/// server `retry-after` still wins (though `/usage` itself only ever sends 0).
fn next_slot_deferral(
    rate_limited: bool,
    retry_after: Option<Duration>,
    streak: u32,
    interval_ms: u64,
    active: bool,
) -> IntervalMs {
    let hint = retry_after.map(|ra| ra.as_millis() as u64);
    let target_ms = if rate_limited {
        let mut ladder = interval_ms.saturating_add(rate_limit_backoff_ms(streak));
        if active && streak <= ACTIVE_CAP_MAX_STREAK {
            ladder = ladder.min(interval_ms.saturating_mul(2));
        }
        hint.unwrap_or(0).max(ladder)
    } else {
        hint.unwrap_or(0)
    };
    IntervalMs::from_millis(
        target_ms
            .min(MAX_RETRY_AFTER_MS)
            .saturating_sub(interval_ms),
    )
}

/// Deterministic per-profile spread (phase offset + per-cycle jitter) added to a
/// live fetch's `last_fetched` stamp so distinct profiles don't fall due on the
/// same tick — avoiding a same-instant request burst against the shared host.
/// Range `[0, interval/4)`. Keyed by `(name, now)`: the name separates profiles,
/// `now` re-rolls the jitter each cycle; stable for a given stamp so the deadline
/// never moves earlier mid-wait. Only widens the gap, never shortens it.
fn deadline_spread(name: &str, now: EpochMs, interval_ms: u64) -> IntervalMs {
    use std::hash::{Hash, Hasher};
    let span = interval_ms / 4;
    if span == 0 {
        return IntervalMs::from_millis(0);
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    now.as_millis().hash(&mut h);
    IntervalMs::from_millis(h.finish() % span)
}

/// Update `name`'s consecutive-429 streak from a fetch `status`, returning the
/// post-update count. `Fresh` clears it (a live body breaks the storm),
/// `RateLimited` increments it, and a transient `Cached`/`Failed` leaves it as
/// is — a network blip mid-storm must not reset the ramp. Leaf lock, taken and
/// released before the caller writes `last_fetched`/`status`.
fn update_rate_limit_streak(streaks: &RateLimitStreaks, name: &str, status: FetchStatus) -> u32 {
    let Ok(mut m) = streaks.lock() else {
        return 0;
    };
    match status {
        FetchStatus::Fresh => {
            m.remove(name);
            0
        }
        FetchStatus::RateLimited => {
            let n = m.entry(name.to_string()).or_insert(0);
            *n = n.saturating_add(1);
            *n
        }
        FetchStatus::Cached | FetchStatus::Failed => m.get(name).copied().unwrap_or(0),
    }
}

/// Write one outcome into the shared stores; returns the stamped next-fetch base
/// (`last_fetched`) so the caller republishes this profile's countdown the instant
/// it lands. Disk cache written on every live response.
fn apply_outcome(
    outcome: FetchOutcome,
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    streaks: &RateLimitStreaks,
    interval_ms: u64,
    is_active: bool,
) -> EpochMs {
    let now = EpochMs::from_millis(now_ms());

    // Only a body that came off the live API may overwrite shared state. The
    // 429/cached fallback paths recycle the on-disk snapshot — stamping that
    // as fresh would clobber a newer store entry and re-write the disk cache
    // mtime, freezing the UI (and the auto-start scan) on stale numbers for as
    // long as the rate limit lasts. `status` still surfaces RateLimited/Cached
    // so the staleness stays visible.
    let is_fresh = outcome.from_fetch;

    // For a Fresh body, keep any just-opened live 5h window we already hold so a
    // lagging `/usage` read can't re-close it (see `preserve_live_window`). The
    // prev window is read under a short lock, released before the disk write.
    let merged: Option<UsageInfo> = outcome.info.as_ref().map(|info| {
        if !is_fresh {
            return info.clone();
        }
        let now_secs = now_epoch_secs();
        let prev = store
            .lock()
            .ok()
            .and_then(|s| s.get(&outcome.name).cloned());
        preserve_live_window(info.clone(), prev.as_ref(), now_secs)
    });

    if is_fresh && let Some(info) = &merged {
        write_profile_cache(&outcome.name, USAGE_CACHE_FILE, info);
    }

    if let Ok(mut s) = store.lock()
        && let Some(info) = &merged
    {
        // Don't clobber newer Fresh data with a Cached fallback snapshot.
        // Cached only fills the store when no entry exists (cold start).
        if is_fresh || !s.contains_key(&outcome.name) {
            s.insert(outcome.name.clone(), info.clone());
        }
    }

    // Server-directed deferral: a 429's `retry-after` lands the next slot on
    // `now + retry_after` (capped); a 429 with no hint backs off exponentially by
    // the consecutive-429 count; everything else keeps the cadence. Live fetches
    // also get a per-profile spread so two profiles don't fall due on the same tick.
    let rate_limited = matches!(outcome.status, FetchStatus::RateLimited);
    let streak = update_rate_limit_streak(streaks, &outcome.name, outcome.status);
    let defer = next_slot_deferral(
        rate_limited,
        outcome.retry_after,
        streak,
        interval_ms,
        is_active,
    );
    let spread = if outcome.from_fetch {
        deadline_spread(&outcome.name, now, interval_ms)
    } else {
        IntervalMs::from_millis(0)
    };
    let stamped = now.saturating_add(defer).saturating_add(spread);

    // Both in one critical section — ascending rank order: LAST_FETCHED(200) < USAGE_STATUS(350).
    if let Ok(mut lf) = last_fetched.lock() {
        lf.insert(outcome.name.clone(), stamped);
        if let Ok(mut st) = status.lock() {
            st.insert(outcome.name.clone(), outcome.status);
        }
    }
    stamped
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

/// Startup usage seed — never blocks on HTTP. Each profile with an on-disk cache is
/// seeded straight from disk so the UI shows last-known numbers instantly, with
/// `last_fetched` stamped at the cache mtime so the fixed cadence *resumes* across
/// the restart (see [`try_seed_cache`]) instead of resetting the countdown. A cache
/// older than one interval is seeded `Cached` and refreshed in the background on the
/// first tick; one younger is `Fresh` and left be. A profile with no cache at all is
/// left unseeded and unstamped, so the scheduler fetches it fresh on its first tick.
pub(crate) fn bootstrap_fetch(
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    tokens: &[TokenEntry],
    interval_ms: u64,
) {
    let now = now_ms();
    for entry in tokens {
        try_seed_cache(store, status, last_fetched, &entry.name, now, interval_ms);
    }
}

/// Load gate shared by both startup seed sites. Takes no locks so each caller
/// stamps its own typed store and keeps its own lock rank (LAST_FETCHED then the
/// status store). Returns the loaded value, the cache mtime, and a freshness-derived
/// [`FetchStatus`] whenever a cache file exists AND is loadable; `None` only when
/// there is no cache. The cache is seeded as a starting point regardless of age:
/// `Fresh` when younger than one refresh interval (still in the fetch window — the
/// scheduler leaves it be), `Cached` when older (shown immediately while the
/// scheduler refreshes it in the background). See [`try_seed_cache`] /
/// [`bootstrap_third_party`] for why `last_fetched` is stamped at the mtime.
fn load_cache_seed<T>(
    name: &str,
    interval_ms: u64,
    now: u64,
    mtime_fn: impl Fn(&str) -> Option<u64>,
    load_fn: impl Fn(&str) -> Option<T>,
) -> Option<(T, u64, FetchStatus)> {
    let mtime = mtime_fn(name)?;
    let value = load_fn(name)?;
    let status = if now.saturating_sub(mtime) < interval_ms {
        FetchStatus::Fresh
    } else {
        FetchStatus::Cached
    };
    Some((value, mtime, status))
}

/// Seed `name` from its on-disk cache whenever one exists, returning `true`. The
/// cache is the startup starting point regardless of age: a cache younger than one
/// interval is `Fresh` (still in the fetch window — `partition_due` won't refetch
/// it), an older one is `Cached` (shown immediately while the scheduler refreshes it
/// in the background on the first tick). The `last_fetched` slot is stamped at the
/// cache **mtime**, so `partition_due` resumes the fixed cadence from the last real
/// write — the overview countdown continues where it left off across a restart
/// rather than resetting to a full interval, and a fresh cache never falls due on
/// the first tick (no startup refresh burst). A `Cached` seed may sit on a 5h window
/// that has since rolled over, so the startup auto-switch one-shot in
/// `finish_bootstrap` acts on `Fresh` data only; stale profiles auto-switch off the
/// corrected numbers on the scheduler's first tick.
fn try_seed_cache(
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    name: &str,
    now: u64,
    interval_ms: u64,
) -> bool {
    let Some((info, mtime, fetch_status)) = load_cache_seed(
        name,
        interval_ms,
        now,
        |n| profile_cache_mtime_ms(n, USAGE_CACHE_FILE),
        |n| load_profile_cache::<UsageInfo>(n, USAGE_CACHE_FILE),
    ) else {
        return false;
    };
    if let Ok(mut s) = store.lock() {
        s.insert(name.to_string(), info);
    }
    // Ascending rank order: LAST_FETCHED(200) < USAGE_STATUS(350) — matches `apply_outcome`.
    if let Ok(mut lf) = last_fetched.lock() {
        lf.insert(name.to_string(), EpochMs::from_millis(mtime));
        if let Ok(mut st) = status.lock() {
            st.insert(name.to_string(), fetch_status);
        }
    }
    true
}

/// Startup third-party seed — the api-key/provider analogue of [`bootstrap_fetch`].
/// Each profile with a `third_party_cache.json` is seeded straight from disk
/// (`last_fetched` stamped at the cache mtime so the cadence resumes across the
/// restart) so the UI shows last-known numbers instantly: `Fresh` when younger than
/// one interval, `Cached` when older (refreshed in the background on the first tick).
/// A profile with no cache is left unstamped, so the scheduler fetches it fresh.
pub(crate) fn bootstrap_third_party(
    store: &ThirdPartyUsageStore,
    status: &ThirdPartyStatusStore,
    last_fetched: &LastFetchedAt,
    entries: &[ThirdPartyEntry],
    interval_ms: u64,
) {
    let now = now_ms();
    for entry in entries {
        let Some((stats, mtime, fetch_status)) = load_cache_seed(
            &entry.name,
            interval_ms,
            now,
            |n| profile_cache_mtime_ms(n, THIRD_PARTY_CACHE_FILE),
            |n| load_profile_cache::<ThirdPartyStats>(n, THIRD_PARTY_CACHE_FILE),
        ) else {
            continue;
        };
        if let Ok(mut s) = store.lock() {
            s.insert(entry.name.clone(), stats);
        }
        // Ascending rank order: LAST_FETCHED(200) < THIRD_PARTY_STATUS(280).
        if let Ok(mut lf) = last_fetched.lock() {
            lf.insert(entry.name.clone(), EpochMs::from_millis(mtime));
            if let Ok(mut st) = status.lock() {
                st.insert(entry.name.clone(), fetch_status);
            }
        }
    }
}

/// Collect api-key profiles for the third-party fetch leg: recognised providers
/// (typed fetch) plus unrecognised api-key endpoints (generic discovery + scan).
pub(crate) fn collect_third_party_entries(
    profiles: &[crate::profile::Profile],
) -> Vec<ThirdPartyEntry> {
    profiles
        .iter()
        .filter_map(|p| {
            let api_key = p.api_key.clone()?;
            let target = if let Some(provider) = p.provider {
                crate::providers::ThirdPartyTarget::Known(provider)
            } else {
                let base_url = p.base_url.clone()?;
                crate::providers::ThirdPartyTarget::Generic { base_url }
            };
            Some(ThirdPartyEntry {
                name: p.name.to_string(),
                target,
                api_key,
            })
        })
        .collect()
}

/// Collect the OAuth profiles' token snapshots for the refresher's `TokenList`.
/// Skips api-key/credential-less profiles (no `claudeAiOauth`). Snapshots the
/// persisted quarantine flag so the poll partition can widen a flagged
/// profile's cadence without a config lock. Shared by the TUI (`App::new` /
/// `refresh_tokens`) and the headless `daemon`.
pub(crate) fn collect_tokens(config: &crate::profile::AppConfig) -> Vec<TokenEntry> {
    config
        .profiles
        .iter()
        .filter_map(|p| {
            let oauth = p.credentials.as_ref()?.claude_ai_oauth.as_ref()?;
            Some(TokenEntry {
                name: p.name.to_string(),
                access_token: oauth.access_token.clone(),
                refresh_token: oauth.refresh_token.clone(),
                auto_start: p.auto_start,
                access_expires_at: oauth.expires_at,
                auth_broken: config.is_auth_broken(&p.name),
            })
        })
        .collect()
}

/// Remove session-suppressed generic profiles from the third-party snapshot so
/// they aren't re-fetched on the timer. The set is cloned once (it is small) so
/// no lock is held across the filter; a poisoned lock passes the snapshot through.
fn filter_suppressed(
    suppressed: &SuppressedGenericStore,
    snapshot: Vec<ThirdPartyEntry>,
) -> Vec<ThirdPartyEntry> {
    let Some(sup) = suppressed.lock().ok() else {
        return snapshot;
    };
    if sup.is_empty() {
        return snapshot;
    }
    snapshot
        .into_iter()
        .filter(|e| !sup.contains(&e.name))
        .collect()
}

/// Fetch a pre-partitioned set of due OAuth profiles and apply outcomes to the
/// usage stores. Mirrors [`fetch_third_party_due`]: partitioning + countdown
/// publishing happen in `tick`; this leg only fetches. Each worker paces against
/// the shared `api.anthropic.com` host inside `get_json`.
fn fetch_oauth_due(state: &SchedulerState, due: Vec<TokenEntry>, interval_ms: u64) {
    for entry in &due {
        // Marked `Queued`, not `Fetching`: the per-host request throttle
        // (`REQUEST_SPACING_MS`) serializes the actual HTTP, so each worker flips
        // itself to `Fetching` (in `get_json`) only when its request clears the
        // gate — the rest read as queued meanwhile.
        mark_activity(&state.activity, &entry.name, ProfileActivity::Queued);
    }

    let handles: Vec<_> = due
        .into_iter()
        .map(|entry| {
            let name = entry.name.clone();
            let config = Arc::clone(&state.config);
            let store = Arc::clone(&state.store);
            let refetch_queue = Arc::clone(&state.refetch_queue);
            let activity = Arc::clone(&state.activity);
            let streaks = Arc::clone(&state.rate_limit_streaks);
            let h = std::thread::spawn(move || {
                run_fetch(&config, entry, &store, &refetch_queue, &activity, &streaks)
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
                // The active profile's 429 ladder caps low (see
                // `next_slot_deferral`); read the flag at apply time so a
                // switch mid-flight lands the right cadence.
                let is_active = state
                    .config
                    .lock()
                    .map(|c| c.is_active(&outcome.name))
                    .unwrap_or(false);
                let stamped = apply_outcome(
                    outcome,
                    &state.store,
                    &state.status,
                    &state.last_fetched,
                    &state.rate_limit_streaks,
                    interval_ms,
                    is_active,
                );
                publish_one_countdown(&state.next_refresh_per_profile, name, stamped, interval_ms);
            }
            Err(_) => {
                // Worker panicked — clear slot so the spinner doesn't freeze.
                clear_activity(&state.activity, &name);
            }
        }
    }
}

/// Fetch a pre-partitioned set of due third-party entries and apply outcomes to
/// the third-party stores. Partitioning + countdown publishing happen in `tick`
/// so both legs share one publish window; this leg only fetches.
fn fetch_third_party_due(state: &SchedulerState, due: Vec<ThirdPartyEntry>) {
    let interval_ms = state.refresh_interval.load(Ordering::Relaxed);
    for entry in &due {
        // `Queued`, not `Fetching`: same-host accounts wait behind the per-host
        // spacing slot, so each worker flips itself to `Fetching` only once its
        // request clears the gate (mirrors the OAuth leg's `get_json` flip).
        mark_activity(&state.activity, &entry.name, ProfileActivity::Queued);
    }

    let handles: Vec<_> = due
        .into_iter()
        .map(|entry| {
            let name = entry.name.clone();
            // Only generic no-data outcomes get session-suppressed; known
            // providers keep retrying on their normal cadence.
            let is_generic = matches!(
                entry.target,
                crate::providers::ThirdPartyTarget::Generic { .. }
            );
            // Reuse the endpoint that last worked so steady state is one request.
            let hint = state
                .third_party_usage_store
                .lock()
                .ok()
                .and_then(|s| s.get(&entry.name).and_then(|st| st.endpoint.clone()));
            // Pace against this provider's host only: accounts on the same endpoint
            // serialize, distinct hosts (and the Anthropic OAuth leg) run in parallel.
            let host = entry.target.throttle_key();
            let activity = Arc::clone(&state.activity);
            let worker_name = entry.name.clone();
            let h = std::thread::spawn(move || {
                await_request_slot(&host);
                mark_activity(&activity, &worker_name, ProfileActivity::Fetching);
                crate::providers::fetch_third_party_usage(
                    &entry.target,
                    &entry.api_key,
                    hint.as_deref(),
                )
            });
            (name, is_generic, h)
        })
        .collect();

    for (name, is_generic, h) in handles {
        match h.join() {
            Ok(Ok(stats)) => {
                clear_activity(&state.activity, &name);
                write_profile_cache(&name, THIRD_PARTY_CACHE_FILE, &stats);
                if let Ok(mut store) = state.third_party_usage_store.lock() {
                    store.insert(name.clone(), stats);
                }
                if let Ok(mut st) = state.third_party_status.lock() {
                    st.insert(name.clone(), FetchStatus::Fresh);
                }
                stamp_last_fetched(
                    &state.last_fetched,
                    &state.next_refresh_per_profile,
                    name,
                    None,
                    false,
                    interval_ms,
                );
            }
            Ok(Err(err)) => {
                clear_activity(&state.activity, &name);
                // Cache cold-fills an absent entry only — never overwrites live
                // store data with disk state (same rule as the OAuth path).
                let cached = load_profile_cache::<ThirdPartyStats>(&name, THIRD_PARTY_CACHE_FILE);
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
                // A generic profile that tried and found nothing (no cache, not a
                // 429) suppresses for the rest of the session — no timer retry,
                // only a manual refresh re-admits it for one retry. 429 keeps the
                // server-directed deferral; cached/known-provider legs are unaffected.
                if is_generic
                    && matches!(status, FetchStatus::Failed)
                    && let Ok(mut sup) = state.suppressed_generic.lock()
                {
                    sup.insert(name.clone());
                }
                stamp_last_fetched(
                    &state.last_fetched,
                    &state.next_refresh_per_profile,
                    name,
                    retry_after,
                    matches!(status, FetchStatus::RateLimited),
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

/// Stamp a profile's fetch slot. Normally `now` (so the next deadline reflects
/// fetch duration, mirroring OAuth `apply_outcome`); a 429's `retry-after`
/// stamps `retry_after - interval` ahead so `partition_due`'s fixed
/// `stamp + interval_ms` math lands the next slot on `now + retry_after`
/// (capped by [`MAX_RETRY_AFTER_MS`]).
fn stamp_last_fetched(
    last_fetched: &LastFetchedAt,
    next_refresh: &NextRefreshPerProfile,
    name: String,
    retry_after: Option<Duration>,
    rate_limited: bool,
    interval_ms: u64,
) {
    // Third-party providers are independent hosts with their own limits; keep the
    // flat base backoff (streak 1) rather than the per-account exponential ramp.
    let defer = next_slot_deferral(rate_limited, retry_after, 1, interval_ms, false);
    let stamped = EpochMs::from_millis(now_ms()).saturating_add(defer);
    if let Ok(mut lf) = last_fetched.lock() {
        lf.insert(name.clone(), stamped);
    }
    publish_one_countdown(next_refresh, name, stamped, interval_ms);
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

/// Republish one profile's countdown (`stamped + interval`, mirroring
/// [`partition_due`]) the instant its fetch lands, so the timer jumps straight
/// from the fetch spinner to the real interval instead of holding the pre-fetch
/// `0s` until the whole batch finishes. Per-key insert (not the full clear+replace
/// of [`publish_countdowns`]) so it can't drop the other leg's keys. NEXT_REFRESH
/// (1100) is acquired alone, after the caller's lower-ranked locks — rank-safe.
fn publish_one_countdown(
    nrpp: &NextRefreshPerProfile,
    name: String,
    stamped: EpochMs,
    interval_ms: u64,
) {
    if let Ok(mut map) = nrpp.lock() {
        map.insert(name, stamped.as_millis().saturating_add(interval_ms));
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
    rate_limit_streaks: RateLimitStreaks,
    pending_switch: PendingSwitch,
    pending_switch_off: PendingSwitchOff,
    refetch_queue: RefetchQueue,
    third_party_tokens: ThirdPartyList,
    third_party_usage_store: ThirdPartyUsageStore,
    third_party_status: ThirdPartyStatusStore,
    suppressed_generic: SuppressedGenericStore,
    shutting_down: Arc<AtomicBool>,
    /// Dual-scheduler dedup (issue #27): probe for a live daemon each tick
    /// and stand this refresher down while one runs. `true` ONLY for the TUI —
    /// the daemon must never probe (its own held flock reads as "another
    /// daemon", a self-stand-down).
    standdown_probe: bool,
    /// Whether the previous tick stood down — transition edges get one log
    /// line each way, never a per-tick repeat.
    standdown_active: AtomicBool,
}

/// One scheduler tick: drain forced refetches, partition both legs, publish
/// countdowns, fan out fetches (OAuth + third-party) that republish each
/// profile's countdown as it lands, propagate rotated tokens, evaluate
/// auto-switch chain.
fn tick(state: &SchedulerState) {
    let interval_ms = state.refresh_interval.load(Ordering::Relaxed);

    // Dual-scheduler dedup (#27): while a live daemon owns the loop, this
    // refresher stands down and renders the daemon's work product instead of
    // competing for it (double HTTP polling drains the per-account usage
    // quota; a doubled rotation races the single-use refresh chain; a doubled
    // auto-switch scan is the switch thrash flagged on #27). The probe is
    // per-tick, so the refresher re-arms within one tick of the daemon dying
    // (flock released) or wedging (status.json stale).
    if state.standdown_probe && crate::daemon::daemon_is_live() {
        if !state.standdown_active.swap(true, Ordering::Relaxed) {
            standdown_transition_log(
                "clauth: live daemon detected — standing down the in-app refresher \
                 (rendering from its feed)",
            );
        }
        standdown_tick(state, interval_ms);
        return;
    }
    if state.standdown_active.swap(false, Ordering::Relaxed) {
        standdown_transition_log("clauth: daemon gone — re-arming the in-app refresher");
    }

    // Names pushed by rotation or manual refresh — bypass cadence this tick.
    // Drained once and handed to both legs; a forced name only matches the leg
    // whose snapshot owns it, so neither starves the other.
    let forced: HashSet<String> = state
        .refetch_queue
        .lock()
        .ok()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default();

    // A manual refresh (forced) clears session suppression so the profile
    // retries once this tick. If it still yields no data it re-suppresses when
    // the outcome lands. Done before the snapshot so the name survives the
    // suppressed-name filter below.
    if !forced.is_empty()
        && let Ok(mut sup) = state.suppressed_generic.lock()
    {
        for name in &forced {
            sup.remove(name);
        }
    }

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
    // Drop generic profiles suppressed this session (no-data on the timer) from
    // the third-party leg so they aren't re-fetched every cadence. Only a manual
    // refresh (forced, cleared above) re-admits one for a single retry.
    let tp_snapshot = filter_suppressed(&state.suppressed_generic, tp_snapshot);

    // Partition both before either fetches, then publish in one window so the
    // countdown map never shows a leg as momentarily missing (and a deleted
    // profile's stale key is dropped by the full replace).
    let now = now_ms();
    let (oauth_due, oauth_next) =
        partition_and_merge(&oauth_snapshot, &forced, state, now, interval_ms);
    let (tp_due, tp_next) = partition_and_merge(&tp_snapshot, &forced, state, now, interval_ms);
    publish_countdowns(&state.next_refresh_per_profile, oauth_next, tp_next);

    // Names actually scheduled this tick across both legs. A forced name absent
    // from both (e.g. a profile whose creds were removed between the UI `r` and
    // this tick) was marked Queued by `enqueue_refetch` but no worker owns it, so
    // the orphan sweep at the tick's end clears it — otherwise its spinner freezes.
    let scheduled: HashSet<String> = oauth_due
        .iter()
        .map(|e| e.name.clone())
        .chain(tp_due.iter().map(|e| e.name.clone()))
        .collect();

    // Both legs fan out concurrently so the third-party leg no longer waits behind
    // the OAuth join loop. Per-host pacing (`await_request_slot`) keeps accounts on
    // the same endpoint serialized while distinct hosts (the Anthropic OAuth host vs
    // each api-key provider) run in parallel. The scope joins the third-party leg
    // before the post-fetch scans below, preserving their "both legs done" ordering.
    std::thread::scope(|s| {
        let tp = (!tp_due.is_empty())
            .then(move || s.spawn(move || fetch_third_party_due(state, tp_due)));
        if !oauth_due.is_empty() {
            fetch_oauth_due(state, oauth_due, interval_ms);
        }
        if let Some(h) = tp {
            // Worker panics are already swallowed inside `fetch_third_party_due`;
            // this join only reaps the leg thread itself.
            let _ = h.join();
        }
    });

    // Orphan sweep: a forced name no leg scheduled keeps a stale Queued mark.
    clear_orphaned_forced(&state.activity, &forced, &scheduled);

    // Auto-switch: evaluate every tick (not only OAuth fetch ticks) so a
    // profile that crossed its threshold is switched immediately, without
    // waiting for the next scheduled fetch. Also checks recovery post-switch-off.
    scan_auto_switch(
        &state.config,
        &state.store,
        &state.status,
        &state.rate_limit_streaks,
        &state.activity,
        &state.pending_switch,
        &state.pending_switch_off,
    );
    scan_recovery(
        &state.config,
        &state.store,
        &state.status,
        &state.pending_switch,
    );
}

/// Log a stand-down transition — but never onto a live terminal. The probe
/// runs only inside the TUI, whose stderr IS the ratatui screen: an
/// unconditional line would paint over the accounts pane on every launch
/// beside a daemon. With stderr redirected (a file, a pipe, CI) the
/// transition is recorded as usual.
fn standdown_transition_log(msg: &str) {
    use std::io::IsTerminal as _;
    if !std::io::stderr().is_terminal() {
        logline!("{msg}");
    }
}

/// One scheduler tick while a live daemon owns the loop. The daemon
/// fetches, rotates, and decides switches; this side only re-reads its work
/// product so the UI stays current:
///   * re-seed the usage / third-party stores from the disk caches the daemon
///     keeps fresh ([`try_seed_cache`] stamps status Fresh/Cached off the cache
///     mtime, and `last_fetched` AT the mtime — so the countdowns below track
///     the daemon's real cadence);
///   * republish countdowns from those stamps (partition is reused for its
///     timing math only; the due list is deliberately discarded — nothing
///     fetches here);
///   * drain forced names (a manual `r`) and clear their Queued marks — the
///     daemon can't be asked to fetch early from here, and a stranded mark
///     would freeze the row's spinner;
///   * skip rotation and both auto-switch scans entirely.
fn standdown_tick(state: &SchedulerState, interval_ms: u64) {
    let forced: HashSet<String> = state
        .refetch_queue
        .lock()
        .ok()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default();

    let oauth_snapshot: Vec<TokenEntry> =
        state.tokens.lock().map(|t| t.clone()).unwrap_or_default();
    let tp_snapshot: Vec<ThirdPartyEntry> = state
        .third_party_tokens
        .lock()
        .map(|t| t.clone())
        .unwrap_or_default();

    hydrate_from_daemon_caches(
        &state.store,
        &state.status,
        &state.third_party_usage_store,
        &state.third_party_status,
        &state.last_fetched,
        &oauth_snapshot,
        &tp_snapshot,
        interval_ms,
    );

    let now = now_ms();
    let (_, oauth_next) = partition_due(
        &oauth_snapshot,
        now,
        &state.last_fetched,
        &state.activity,
        interval_ms,
    );
    let (_, tp_next) = partition_due(
        &tp_snapshot,
        now,
        &state.last_fetched,
        &state.activity,
        interval_ms,
    );
    publish_countdowns(&state.next_refresh_per_profile, oauth_next, tp_next);

    clear_orphaned_forced(&state.activity, &forced, &HashSet::new());
    // With no worker running, EVERY Queued mark is an orphan — not only forced
    // ones. The bootstrap pre-marks cache-due profiles Queued so the first
    // paint shows a spinner instead of a stale countdown, expecting the first
    // tick's worker to take over and clear it; standing down, nothing ever
    // does, and the row would spin forever where the daemon-fed countdown
    // belongs. Fetching/Refreshing/Switching stay — a worker from the last
    // armed tick may genuinely still be in flight and clears itself.
    if let Ok(mut a) = state.activity.lock() {
        a.retain(|_, act| !matches!(act, ProfileActivity::Queued));
    }
}

/// The store-refresh half of [`standdown_tick`], extracted store-narrow so the
/// hydrate contract is testable without a full `SchedulerState`: every profile
/// with an on-disk cache lands in its store with a freshness-derived status and
/// `last_fetched` stamped at the cache mtime; cacheless profiles are left
/// untouched (the daemon will publish them shortly).
#[allow(clippy::too_many_arguments)]
fn hydrate_from_daemon_caches(
    store: &UsageStore,
    status: &StatusStore,
    tp_store: &ThirdPartyUsageStore,
    tp_status: &ThirdPartyStatusStore,
    last_fetched: &LastFetchedAt,
    oauth: &[TokenEntry],
    third_party: &[ThirdPartyEntry],
    interval_ms: u64,
) {
    let now = now_ms();
    for entry in oauth {
        try_seed_cache(store, status, last_fetched, &entry.name, now, interval_ms);
    }
    bootstrap_third_party(tp_store, tp_status, last_fetched, third_party, interval_ms);
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
    rate_limit_streaks: RateLimitStreaks,
    pending_switch: PendingSwitch,
    pending_switch_off: PendingSwitchOff,
    refetch_queue: RefetchQueue,
    third_party_tokens: ThirdPartyList,
    third_party_usage_store: ThirdPartyUsageStore,
    third_party_status: ThirdPartyStatusStore,
    suppressed_generic: SuppressedGenericStore,
    shutting_down: Arc<AtomicBool>,
    standdown_probe: bool,
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
        rate_limit_streaks,
        pending_switch,
        pending_switch_off,
        refetch_queue,
        third_party_tokens,
        third_party_usage_store,
        third_party_status,
        suppressed_generic,
        shutting_down,
        standdown_probe,
        standdown_active: AtomicBool::new(false),
    };
    #[allow(clippy::expect_used, reason = "thread spawn failure is unrecoverable")]
    std::thread::Builder::new()
        .name("clauth-tick".into())
        .spawn(move || {
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
        })
        .expect("failed to spawn scheduler tick thread");
}

/// Evaluate the fallback chain and queue an auto-switch target.
///
/// Snapshots the chain under `config` mutex (dropped before taking `usage_store`).
/// This split is load-bearing: `App::apply_usage` takes `usage_store` then `config`,
/// so the scheduler must never hold `config` while taking `usage_store`.
/// A profile's store entry is trustworthy for an auto-switch / recovery decision
/// only when its last fetch was live (`Fresh`). A `Cached` entry may be a 5h
/// window that has since rolled over (its stale-high utilization would drive a
/// false switch-away) and a `RateLimited` one may be the synthetic just-kicked
/// 0% placeholder (which would never switch away, or switch toward a spent
/// account) — the startup one-shot gates on `Fresh` for the same reason.
fn decision_fresh(status: &StatusStore, name: &str) -> bool {
    matches!(
        status.lock().ok().and_then(|m| m.get(name).copied()),
        Some(FetchStatus::Fresh)
    )
}

/// True when `name`'s last reading is a **deep-slot stuck** `RateLimited`: the
/// status is `RateLimited` AND its consecutive-429 streak has passed
/// [`ACTIVE_CAP_MAX_STREAK`] — the boundary where the active cap stops holding
/// retries frequent. Past it, a still-`RateLimited` read is genuinely stuck (the
/// `/usage` throttle window never drained), not a transient blip.
///
/// ONE predicate, two consumers, so display and decision cannot drift:
///   * `scan_auto_switch` distrusts a stuck-RateLimited active — it bypasses the
///     [`decision_fresh`] gate exactly like an auth-broken active (AUTH-4) so the
///     walk can rotate away instead of wedging on an account that can never
///     return `Fresh`. The switch still requires the walk's own last-known
///     exhaustion gate ([`crate::fallback::next_auto_switch_target`]), so a
///     throttle artifact with real headroom stays put — only a genuinely spent
///     stuck active moves.
///   * `status.json`'s per-profile `stale` flag publishes the same judgment so a
///     menu-bar reader renders the reading as distrusted, not current truth.
pub(crate) fn is_stuck_rate_limited(status: FetchStatus, streak: u32) -> bool {
    matches!(status, FetchStatus::RateLimited) && streak > ACTIVE_CAP_MAX_STREAK
}

fn scan_auto_switch(
    config: &crate::profile::ConfigHandle,
    store: &UsageStore,
    status: &StatusStore,
    streaks: &RateLimitStreaks,
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

    // Only act on a confirmed-live read of the active profile — a stale or
    // synthetic store entry would drive a false switch (see `decision_fresh`).
    // TWO exceptions bypass the freshness gate, both because the active can
    // never come back Fresh on its own so requiring one wedges the scan on it:
    //   * an auth-broken active (AUTH-4) — its login is dead (observed
    //     2026-07-09); the walk never consults its usage.
    //   * a deep-slot stuck RateLimited active (the RateLimited analogue) — the
    //     `/usage` throttle stayed pinned past the active cap, so no Fresh read
    //     is coming. Unlike auth-broken, this one still faces the walk's
    //     last-known exhaustion gate, so a throttle artifact with headroom stays
    //     put and only a genuinely spent stuck active rotates away.
    let active_broken = snapshot.broken.iter().any(|b| b == &snapshot.active);
    let active_status = status
        .lock()
        .ok()
        .and_then(|m| m.get(&snapshot.active).copied());
    let active_stuck_rl = active_status
        .is_some_and(|s| is_stuck_rate_limited(s, rate_limit_streak(streaks, &snapshot.active)));
    if !active_broken && !active_stuck_rl && !matches!(active_status, Some(FetchStatus::Fresh)) {
        return;
    }

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
    status: &StatusStore,
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
    let (members, weekly_pct): (Vec<crate::fallback::ChainMember>, f64) = {
        let cfg = match config.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let weekly_pct = cfg.state.weekly_switch_threshold_pct();
        // Only scan for recovery after switch-off-all (no active profile).
        if cfg.state.active_profile.is_some() {
            return;
        }
        if cfg.state.fallback_chain.is_empty() {
            return;
        }
        let members = cfg
            .state
            .fallback_chain
            .iter()
            .map(|name| {
                let profile = cfg.find(name);
                crate::fallback::ChainMember {
                    name: name.to_string(),
                    threshold: profile
                        .map(crate::fallback::threshold_for)
                        .unwrap_or(crate::fallback::DEFAULT_THRESHOLD),
                    last_resort: profile.is_some_and(|p| p.last_resort),
                }
            })
            .collect();
        (members, weekly_pct)
    };

    // Relink only to a member with a confirmed-live read; a synthetic/stale 0%
    // entry would relink to an unverified placeholder (see `decision_fresh`).
    let members: Vec<crate::fallback::ChainMember> = members
        .into_iter()
        .filter(|m| decision_fresh(status, &m.name))
        .collect();

    if let Some(name) = crate::fallback::find_recovered_member(&members, store, weekly_pct)
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
/// A quarantined entry's deadline widens by its `poll_backoff_ms` — read from the
/// snapshot each partition, so the widening vanishes the tick the flag lifts.
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
        let next = last
            .saturating_add(interval)
            .saturating_add(IntervalMs::from_millis(entry.poll_backoff_ms()));
        per_profile.insert(entry.name().to_string(), next.as_millis());
        let excluded = match act.as_ref() {
            Ok(a) => matches!(
                a.get(entry.name()),
                Some(ProfileActivity::Refreshing | ProfileActivity::Switching)
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

/// Merge forced (cadence-bypassing) entries into `due`. Skips profiles that are
/// `Refreshing`/`Switching` — `rotate_one_inner` or the switch gate owns the
/// activity slot — and entries already due.
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
            .filter(|(_, v)| matches!(v, ProfileActivity::Refreshing | ProfileActivity::Switching))
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

/// Clear any forced name that no leg scheduled this tick — its profile vanished
/// from both snapshots between the UI `r` and now, leaving a `Queued` mark that no
/// worker owns and would otherwise spin forever. `Refreshing`/`Switching` names
/// are owned by a rotate / switch-gate worker, so they are left in place.
fn clear_orphaned_forced(
    activity: &ActivityStore,
    forced: &HashSet<String>,
    scheduled: &HashSet<String>,
) {
    if forced.is_empty() {
        return;
    }
    if let Ok(mut a) = activity.lock() {
        for name in forced {
            if !scheduled.contains(name)
                && !matches!(
                    a.get(name),
                    Some(ProfileActivity::Refreshing | ProfileActivity::Switching)
                )
            {
                a.remove(name);
            }
        }
    }
}

#[cfg(test)]
#[path = "../../tests/inline/scheduler.rs"]
mod tests;
