use anyhow::Result;

use crate::actions::{switch_off, switch_profile};
use crate::lock::with_state_lock;
use crate::profile::{AppConfig, Profile};
use crate::usage::{
    UsageStore, UsageWindow, five_hour_live, iso_to_epoch_secs, now_epoch_secs, seven_day_live,
};

/// What the auto-switch evaluator decided when the active profile crossed its
/// threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwitchAction {
    /// Switch the active profile to this chain member.
    To(String),
    /// Turn off all accounts: clear the live credentials and unset the active
    /// profile. Emitted only in wrap-off mode when the whole chain is exhausted
    /// and no member is marked `last_resort`.
    Off,
}

/// Default 5-hour utilization threshold (percent) applied when a chain member
/// has no per-profile override. Stays below 100 as poll-lag margin: at the
/// fixed refresh cadence a window can blow past a 100% trigger between polls,
/// so the default leaves headroom to switch before the account is already
/// rate-limited.
pub(crate) const DEFAULT_THRESHOLD: f64 = 95.0;

pub(crate) fn threshold_for(profile: &Profile) -> f64 {
    profile.fallback_threshold.unwrap_or(DEFAULT_THRESHOLD)
}

/// Live 5h window for `profile`, or `None` when there's no snapshot yet or its
/// window isn't currently live (`five_hour_live`) — a lapsed or windowless
/// snapshot means the account has headroom again whatever its last-known
/// utilization says. Shared by [`is_exhausted`] and the burn-aware active
/// check ([`is_exhausted_active`]): both refuse to act on a stale/rolled-over
/// window the same way; they differ only in how they judge a live one.
fn live_five_hour(profile: &Profile) -> Option<&UsageWindow> {
    let usage = profile.usage.as_ref()?;
    if !five_hour_live(usage, now_epoch_secs()) {
        return None;
    }
    usage.five_hour.as_ref()
}

/// Weekly line for the wrap-off `Off` decision: REAL exhaustion (the API's own
/// cap), never the configurable soft line. Switching early to a sibling buys
/// headroom safety; signing every account out early buys nothing — it only
/// forfeits the tail between the soft line and the cap on the halt path.
///
/// Every surface that DESCRIBES the halted state reads the same line — the
/// all-spent banner and `soonest_resume`'s caption included — or it would claim
/// a member is spent while that member still serves requests.
pub(crate) const WEEKLY_HARD_BLOCK_PCT: f64 = 100.0;

/// Whether `info`'s live weekly window is past `weekly_pct` — treated as
/// spent until the weekly reset regardless of anything the 5h window says.
/// The line is a SOFT one below the API's 100% refusal cap, gating BOTH
/// directions of the chain walk (the active profile's switch trigger and
/// candidate acceptance, so a hop never lands on a member that would
/// re-trigger next tick).
///
/// Why not wait for 100 (the original hard cap, 2026-07-08): an account
/// riding 98%+ of its week bricks for DAYS the moment it tops out, and its
/// idle 5h window drains and then LAPSES — so waiting for the refusal means
/// dying mid-session and (before the gate) even switching INTO such a member.
/// Hopping at the line leaves the tail as slack instead (2026-07-12).
///
/// `weekly_pct` is chain-global (`AppState::weekly_switch_threshold_pct`,
/// default 98), not a per-member knob like the 5h threshold: the line
/// protects the CHAIN (a wrong hop strands days, not hours), not one member's
/// taste. Only `seven_day` gates here — a per-model `weekly_scoped` limit at
/// 100 (e.g. "7d fable" spent while 7d has room) blocks just that model, and
/// the walk cannot know which model the next session will drive. Store-side
/// twin logic inlines this same shape (see `is_exhausted_from_store`).
fn weekly_blocked_info(info: &crate::usage::UsageInfo, now_secs: i64, weekly_pct: f64) -> bool {
    seven_day_live(info, now_secs)
        && info
            .seven_day
            .as_ref()
            .is_some_and(|w| w.utilization >= weekly_pct)
}

/// [`weekly_blocked_info`] over a profile's own usage snapshot.
fn weekly_blocked(profile: &Profile, weekly_pct: f64) -> bool {
    profile
        .usage
        .as_ref()
        .is_some_and(|u| weekly_blocked_info(u, now_epoch_secs(), weekly_pct))
}

/// True when the profile's 5h utilization has crossed its own threshold. Also
/// drives the TUI's all-spent banner wording. Static regardless of burn-aware
/// mode (issue #8 follow-up b) — the target walk's candidates and
/// `soonest_resume` keep this exact behavior by design; only the ACTIVE
/// profile's own decision becomes projection-aware (see
/// [`is_exhausted_active`]).
pub(crate) fn is_exhausted(profile: &Profile, weekly_pct: f64) -> bool {
    weekly_blocked(profile, weekly_pct)
        || live_five_hour(profile).is_some_and(|w| w.utilization >= threshold_for(profile))
}

/// Recent burn rate (%/h) for `name`'s 5h window: durable per-profile history
/// (`usage_history.jsonl`, a plain disk read — no shared lock, so this is safe
/// to call without touching the order in `lockorder.rs`, but never while
/// holding the `AppConfig` guard) plus the current live sample, run through
/// the same recency-weighted computation and windowing `App::active_burn_rate`
/// uses for the Overview ETA line. `None` until enough distinct samples exist.
/// Sole caller is the scheduler-side [`is_exhausted_active_from_store`] — the
/// UI-thread [`is_exhausted_active`] takes its rate as a parameter instead so
/// the render pass never triggers this disk read under the config guard.
pub(crate) fn burn_rate_for_profile(name: &str, window: &UsageWindow) -> Option<f64> {
    let history = crate::profile::load_usage_history(name);
    let pair = ("5h", window);
    crate::usage::compute_burn_rates_from_history(
        &history,
        std::slice::from_ref(&pair),
        crate::usage::BURN_LOOKBACK_MS,
        crate::usage::BURN_MIN_SAMPLES,
        crate::usage::BURN_GAP_CUT_MS,
    )
    .remove("5h")
    .flatten()
}

/// Burn-aware exhaustion test shared by [`is_exhausted_active`] and its
/// scheduler-side store variant (issue #8 follow-up b, opt-in — off by
/// default). `None` burn (no history yet, a fresh profile, or a provider with
/// none) falls back to the plain `util_pct >= threshold` check — never leaves
/// an account uncovered for lack of data. With a rate, "exhausted" means the
/// *projected* utilization at the next poll has crossed the 100% cap, not the
/// per-profile threshold: a heavy burn switches ahead of the static
/// threshold, a light one may run past it since it won't blow the cap before
/// the next poll.
fn is_exhausted_projected(
    util_pct: f64,
    threshold: f64,
    burn_pct_per_hour: Option<f64>,
    interval_ms: u64,
) -> bool {
    match burn_pct_per_hour {
        Some(rate) => crate::usage::project_utilization(util_pct, rate, interval_ms) >= 100.0,
        None => util_pct >= threshold,
    }
}

/// ACTIVE-only exhaustion check (issue #8 follow-up b). `burn_aware` off
/// reproduces [`is_exhausted`] bit for bit — mode off must never diverge from
/// today's static behavior. On, `active_burn_pct_per_hour` — the caller's
/// in-memory rate; this function never reads disk itself, see
/// [`is_exhausted_active_from_store`] for the disk-reading scheduler twin —
/// feeds [`is_exhausted_projected`]. Deliberately never applied to the target
/// walk's candidates or `soonest_resume` — see docs/internals.md's auto-switch
/// asymmetry; this only ever changes whether the ACTIVE profile itself is
/// judged exhausted.
pub(crate) fn is_exhausted_active(
    profile: &Profile,
    burn_aware: bool,
    interval_ms: u64,
    active_burn_pct_per_hour: Option<f64>,
    weekly_pct: f64,
) -> bool {
    // Weekly line first: a 7d window past it is treated as dead until its
    // weekly reset whatever the 5h window (often lapsed/idle by then) says,
    // and no burn projection applies — there is nothing left to project.
    if weekly_blocked(profile, weekly_pct) {
        return true;
    }
    let Some(window) = live_five_hour(profile) else {
        return false;
    };
    let threshold = threshold_for(profile);
    if !burn_aware {
        return window.utilization >= threshold;
    }
    is_exhausted_projected(
        window.utilization,
        threshold,
        active_burn_pct_per_hour,
        interval_ms,
    )
}

/// Name + seconds-until-reset of the chain member that resumes soonest — the
/// all-exhausted caption's data source (issue #10: the implicit
/// resume-at-soonest-reset behavior, made explicit). Valid only when the
/// WHOLE chain is currently exhausted, covering both wrap-off's
/// switch-off-all (active cleared) and wrap mode's stalled-active equivalent
/// (`next_target` returns `None` with every member maxed). Reuses
/// [`is_exhausted`]'s wall-clock gates (`five_hour_live` and the weekly
/// `seven_day_live` block): a member exhausted in neither window
/// already has headroom — `find_recovered_member` /
/// `scan_recovery` would relink it on the very next tick — so that member's
/// presence bails the WHOLE result to `None` rather than being skipped around;
/// the caption's premise is that NOTHING in the chain is currently usable.
/// Ties on `resets_at` keep the earlier chain-order member.
///
/// Weekly-wise the premise keys on the HARD cap ([`WEEKLY_HARD_BLOCK_PCT`]),
/// not the soft line (2026-07-10 triage): a member past the line still serves
/// every request its live 5h window allows, so counting it spent captioned a
/// days-out weekly reset over a working account. The window PICK below keeps
/// the soft line — once a member is genuinely out of 5h, its soft-blocked week
/// is what `find_recovered_member` gates on, so its 7d reset (not the 5h one)
/// is when the chain can use it again.
pub(crate) fn soonest_resume(config: &AppConfig) -> Option<(String, i64)> {
    let chain = &config.state.fallback_chain;
    if chain.is_empty() {
        return None;
    }
    let now = now_epoch_secs();
    let weekly_pct = config.state.weekly_switch_threshold_pct();
    let mut best: Option<(&str, i64)> = None;
    for name in chain {
        let profile = config.find(name)?;
        if !is_exhausted(profile, WEEKLY_HARD_BLOCK_PCT) {
            return None;
        }
        // A weekly-dead member resumes at its 7d reset (its 5h window may be
        // lapsed or absent entirely); anyone else at their next 5h reset.
        let usage = profile.usage.as_ref()?;
        let window = if weekly_blocked(profile, weekly_pct) {
            usage.seven_day.as_ref()?
        } else {
            usage.five_hour.as_ref()?
        };
        let resets_at = window.resets_at.as_deref().and_then(iso_to_epoch_secs)?;
        if best.is_none_or(|(_, cur)| resets_at < cur) {
            best = Some((name.as_str(), resets_at));
        }
    }
    let (name, resets_at) = best?;
    Some((name.to_string(), (resets_at - now).max(0)))
}

/// One chain member as observed when a `ChainSnapshot` was built. Holds enough
/// to evaluate the fallback decision without re-locking `AppConfig` — caller
/// snapshots once under the config mutex then reads `UsageStore` separately,
/// avoiding the `config ↔ store` lock inversion against `App::apply_usage`
/// (which holds `usage_store` then takes `config`).
#[derive(Debug, Clone)]
pub(crate) struct ChainMember {
    pub(crate) name: String,
    pub(crate) threshold: f64,
    /// Mirrors `Profile::last_resort` — a terminal stop for the chain walk,
    /// decoupled from `threshold` (issue #8 follow-up: a threshold no longer
    /// doubles as a sink marker).
    pub(crate) last_resort: bool,
}

/// In-memory snapshot of the fields `next_auto_switch_target` needs: active
/// profile name + ordered chain with each member's resolved threshold. Built
/// under the `AppConfig` mutex by [`snapshot_chain`], then evaluated lock-free.
#[derive(Debug, Clone)]
pub(crate) struct ChainSnapshot {
    pub(crate) active: String,
    pub(crate) chain: Vec<ChainMember>,
    /// Snapshot of `AppState::wrap_off` — drives the switch-off-all decision.
    pub(crate) wrap_off: bool,
    /// Snapshot of `AppState::auth_broken` — members whose OAuth refresh is
    /// revoked/invalid (AUTH-1). Excluded from every walk pass so a dead token
    /// is never installed unattended.
    pub(crate) broken: Vec<String>,
    /// Snapshot of `AppState::burn_aware_switching` (issue #8 follow-up b) —
    /// gates whether the ACTIVE-side check in `next_auto_switch_target`
    /// projects ahead of the next poll instead of using the static threshold.
    pub(crate) burn_aware: bool,
    /// Snapshot of `AppState::refresh_interval_ms` — the projection's poll
    /// interval. Read through `config.state` here (this snapshot is already
    /// built once under the config lock per tick) rather than the scheduler's
    /// hot-path `Arc<AtomicU64>`, mirroring exactly how `wrap_off` reaches
    /// this struct.
    pub(crate) interval_ms: u64,
    /// Snapshot of `AppState::weekly_switch_threshold_pct()` — the chain-wide
    /// weekly (7d) exhaustion line both walk directions gate on.
    pub(crate) weekly_pct: f64,
    /// Members whose 5h auto-start kick the messages limiter is REJECTING
    /// (switch-grade `KickBlock`s: the limiter's own `rejected` verdict, ≥2
    /// consecutive kicks, advertised ceiling still ahead). Not config state —
    /// [`snapshot_chain`] leaves it empty and the scheduler's scan fills it
    /// from the live kick-block map. A rejected member can't serve inference
    /// even with idle-looking usage, so it is walked around like `broken`, and
    /// a rejected ACTIVE bypasses the exhaustion gate the same way.
    pub(crate) kick_rejected: Vec<String>,
}

/// Snapshot active profile + chain + per-member thresholds out of `AppConfig`.
/// Returns `None` when there's no active profile, the active isn't a chain
/// member, or the chain is empty — every case where `next_auto_switch_target`
/// short-circuits anyway, so callers can skip evaluation on `None`.
pub(crate) fn snapshot_chain(config: &AppConfig) -> Option<ChainSnapshot> {
    let active = config.state.active_profile.as_deref()?.to_string();
    let chain = &config.state.fallback_chain;
    if !chain.iter().any(|n| n == &active) {
        return None;
    }
    let chain = chain
        .iter()
        .filter(|name| {
            // CDX-1 T1b tolerance: a stray codex member (hand-edited into an
            // existing profiles.toml — the edit surfaces reject new ones) must
            // never become a walk candidate. Silent here (this runs every
            // tick); the rejection with a message lives in `fallback_config`.
            !config.find(name).is_some_and(|p| p.is_codex())
        })
        .map(|name| {
            let profile = config.find(name);
            ChainMember {
                name: name.to_string(),
                threshold: profile.map(threshold_for).unwrap_or(DEFAULT_THRESHOLD),
                last_resort: profile.is_some_and(|p| p.last_resort),
            }
        })
        .collect();
    Some(ChainSnapshot {
        active,
        chain,
        wrap_off: config.state.wrap_off,
        broken: config
            .state
            .auth_broken
            .iter()
            .map(|n| n.as_str().to_string())
            .collect(),
        burn_aware: config.state.burn_aware_switching,
        interval_ms: config.state.refresh_interval_ms,
        weekly_pct: config.state.weekly_switch_threshold_pct(),
        kick_rejected: Vec::new(),
    })
}

/// Scheduler-side [`is_exhausted`]: reads 5h utilization from the shared
/// `UsageStore` rather than `Profile.usage` (which only the UI thread writes via
/// `apply_usage`). A poisoned store lock fails safe to "not exhausted" so a
/// momentarily wedged mutex can't trigger a switch.
fn is_exhausted_from_store(
    name: &str,
    threshold: f64,
    store: &UsageStore,
    weekly_pct: f64,
) -> bool {
    let now = now_epoch_secs();
    match store.lock() {
        Ok(s) => s.get(name).is_some_and(|info| {
            weekly_blocked_info(info, now, weekly_pct)
                || (five_hour_live(info, now)
                    && info
                        .five_hour
                        .as_ref()
                        .is_some_and(|w| w.utilization >= threshold))
        }),
        Err(_) => false,
    }
}

/// Scheduler-side [`is_exhausted_active`]: reads the 5h window from
/// `UsageStore` instead of `Profile.usage`, so the scheduler's periodic scan
/// agrees with the UI-thread one-shot (`auto_switch_if_needed`) on the ACTIVE
/// decision. The store lock is dropped before the (disk-only, unlocked) burn
/// rate lookup — never held across that I/O. A poisoned store lock fails safe
/// to "not exhausted".
fn is_exhausted_active_from_store(
    name: &str,
    threshold: f64,
    burn_aware: bool,
    interval_ms: u64,
    store: &UsageStore,
    weekly_pct: f64,
) -> bool {
    let now = now_epoch_secs();
    // One lock window for both reads; the weekly line trumps projection
    // (mirrors `is_exhausted_active`).
    let (blocked, window): (bool, Option<UsageWindow>) = match store.lock() {
        Ok(s) => match s.get(name) {
            Some(info) => (
                weekly_blocked_info(info, now, weekly_pct),
                if five_hour_live(info, now) {
                    info.five_hour.clone()
                } else {
                    None
                },
            ),
            None => (false, None),
        },
        Err(_) => (false, None),
    };
    if blocked {
        return true;
    }
    let Some(window) = window else {
        return false;
    };
    if !burn_aware {
        return window.utilization >= threshold;
    }
    let rate = burn_rate_for_profile(name, &window);
    is_exhausted_projected(window.utilization, threshold, rate, interval_ms)
}

/// Chain walk shared by [`next_target`] and [`next_auto_switch_target`]. Scans
/// every other slot starting one after `idx` and wrapping. `skip_pred(i)` skips
/// slots (active profile, or a member with no resolvable profile);
/// `accept_pred(i)` selects the first matching slot, whose index is returned.
fn walk_chain(
    idx: usize,
    len: usize,
    skip_pred: &dyn Fn(usize) -> bool,
    accept_pred: &dyn Fn(usize) -> bool,
) -> Option<usize> {
    for offset in 1..=len {
        let i = (idx + offset) % len;
        if skip_pred(i) {
            continue;
        }
        if accept_pred(i) {
            return Some(i);
        }
    }
    None
}

/// Picks the next chain member to switch to, starting one slot after the active
/// profile and wrapping. Returns None when nothing is viable.
///
///   1. Any member with real headroom (5h utilization below threshold, or no
///      usage data fetched yet).
///   2. Last resort: a member marked `last_resort`, accepted even while
///      exhausted. Claude Code shows its own "out of 5h limit" message on
///      arrival. `last_resort` is independent of `threshold` — a member can
///      still switch away at, say, 80% utilization and remain the chain's
///      last resort once nothing else has headroom.
///   3. Wrap-off only: no headroom, no `last_resort` member anywhere, and the
///      active profile itself exhausted → [`SwitchAction::Off`] to halt usage.
///
/// `active_burn_pct_per_hour` is the caller's in-memory 5h burn rate for the
/// active profile, forwarded to [`is_exhausted_active`] for step 3's
/// burn-aware projection; ignored unless `burn_aware_switching` is on. This
/// function never reads disk — callers must supply the rate themselves.
pub(crate) fn next_target(
    config: &AppConfig,
    active_burn_pct_per_hour: Option<f64>,
) -> Option<SwitchAction> {
    let active = config.state.active_profile.as_deref()?;
    let chain = &config.state.fallback_chain;
    let active_idx = chain.iter().position(|n| n == active)?;
    let len = chain.len();
    let weekly_pct = config.state.weekly_switch_threshold_pct();

    // Skip the active slot, any member with no resolvable profile, and any
    // auth-broken member (AUTH-1: never rotate into a revoked/dead token) — the
    // last applies to the headroom pass AND the 100%-sink pass below.
    let skip = |i: usize| {
        chain[i] == active || config.find(&chain[i]).is_none() || config.is_auth_broken(&chain[i])
    };
    let walk = |accept: &dyn Fn(&Profile) -> bool| -> Option<String> {
        let pick = walk_chain(active_idx, len, &skip, &|i| {
            config.find(&chain[i]).is_some_and(&accept)
        });
        pick.map(|i| chain[i].to_string())
    };

    if let Some(name) = walk(&|p| !is_exhausted(p, weekly_pct)) {
        return Some(SwitchAction::To(name));
    }

    // Only fall back to a `last_resort` member when the active profile is NOT
    // itself marked `last_resort`. Two last-resort members switching to each
    // other indefinitely gains nothing — one migration is fine, but the next
    // tick must stay put.
    let active_is_last_resort = config.find(active).is_some_and(|p| p.last_resort);
    if active_is_last_resort {
        return None;
    }
    if let Some(name) = walk(&|p| p.last_resort) {
        return Some(SwitchAction::To(name));
    }

    // No headroom, no `last_resort` member anywhere. In wrap-off mode, turn off
    // all accounts — but only when the active profile is itself exhausted,
    // since this picker is also exercised on a healthy active. The ACTIVE
    // check is burn-aware (issue #8 follow-up b) so this agrees with
    // `next_auto_switch_target`'s scheduler-side gate; the candidate walk
    // above stays on the static `is_exhausted`. Weekly-wise `Off` keys on the
    // HARD cap (`WEEKLY_HARD_BLOCK_PCT`), not the soft line: a soft-blocked
    // active with real weekly room left stays put instead of signing every
    // running claude out over the tail it could still spend.
    if config.state.wrap_off
        && config.find(active).is_some_and(|p| {
            is_exhausted_active(
                p,
                config.state.burn_aware_switching,
                config.state.refresh_interval_ms,
                active_burn_pct_per_hour,
                WEEKLY_HARD_BLOCK_PCT,
            )
        })
    {
        return Some(SwitchAction::Off);
    }
    None
}

/// Scheduler-side [`auto_switch_if_needed`]: same logic over an in-memory
/// [`ChainSnapshot`] taken under the config mutex, reading utilization from the
/// shared `UsageStore`. Returns the member to switch to, or `None`.
///
/// The store/config lock split is load-bearing: `App::apply_usage` locks
/// `usage_store` then `config`, so the scheduler must never hold `config` while
/// taking `usage_store`. Caller builds the snapshot under `config.lock()`, drops
/// the guard, then calls this.
pub(crate) fn next_auto_switch_target(
    snapshot: &ChainSnapshot,
    store: &UsageStore,
) -> Option<SwitchAction> {
    let active_idx = snapshot
        .chain
        .iter()
        .position(|m| m.name == snapshot.active)?;
    let len = snapshot.chain.len();

    let active = &snapshot.chain[active_idx];
    // AUTH-4: an auth-broken active bypasses the exhaustion gate. Its fetches
    // can never succeed again, so its store entry is frozen at the last read —
    // usually a lapsed 5h window that reads as idle headroom — and requiring
    // exhaustion here wedged the daemon on the dead account while a viable
    // sibling idled (observed 2026-07-09). The flag is terminal-confirmed (set
    // only after a rejected refresh AND a failed live-mirror adopt), and the
    // walk below never consults the broken active's own usage.
    let active_broken = snapshot.broken.iter().any(|b| b == &active.name);
    // A kick-rejected active is broken's messages-limiter analogue: its usage
    // can read as idle headroom (`/usage` stays 200 through the outage) while
    // every inference request is rejected, so exhaustion can't be a
    // precondition for leaving it either. Unlike `broken` it is transient —
    // the block clears itself once a kick lands — so only the switch-grade
    // form (limiter-confirmed `rejected`, ≥2 kicks, ceiling ahead) reaches
    // this snapshot at all.
    let active_kick_rejected = snapshot.kick_rejected.iter().any(|k| k == &active.name);
    let active_exhausted = is_exhausted_active_from_store(
        &active.name,
        active.threshold,
        snapshot.burn_aware,
        snapshot.interval_ms,
        store,
        snapshot.weekly_pct,
    );
    if !active_broken && !active_kick_rejected && !active_exhausted {
        return None;
    }

    // Skip the active slot and any auth-broken member (AUTH-1) — the broken
    // exclusion covers the headroom pass AND the 100%-sink pass below.
    let skip = |i: usize| {
        snapshot.chain[i].name == active.name
            || snapshot.broken.iter().any(|b| b == &snapshot.chain[i].name)
            || snapshot
                .kick_rejected
                .iter()
                .any(|k| k == &snapshot.chain[i].name)
    };
    let walk = |accept: &dyn Fn(&ChainMember) -> bool| -> Option<String> {
        let pick = walk_chain(active_idx, len, &skip, &|i| accept(&snapshot.chain[i]));
        pick.map(|i| snapshot.chain[i].name.clone())
    };

    if let Some(name) =
        walk(&|m| !is_exhausted_from_store(&m.name, m.threshold, store, snapshot.weekly_pct))
    {
        return Some(SwitchAction::To(name));
    }

    let active_is_last_resort = active.last_resort;
    if active_is_last_resort {
        return None;
    }
    if let Some(name) = walk(&|m| m.last_resort) {
        return Some(SwitchAction::To(name));
    }

    // No headroom, no `last_resort` member anywhere. In wrap-off mode, halt
    // all usage instead of staying on the spent profile — keyed on REAL
    // exhaustion only: a broken-but-unspent active stays put (AUTH-4), since
    // the live session's own Keychain chain may still be healthy and switching
    // off would log it out over a flag, not over spent quota. The switch
    // trigger above honored the soft weekly line; `Off` re-checks at the HARD
    // cap (`WEEKLY_HARD_BLOCK_PCT`) so a merely soft-blocked active with real
    // weekly room left never signs every running claude out early.
    if snapshot.wrap_off
        && is_exhausted_active_from_store(
            &active.name,
            active.threshold,
            snapshot.burn_aware,
            snapshot.interval_ms,
            store,
            WEEKLY_HARD_BLOCK_PCT,
        )
    {
        return Some(SwitchAction::Off);
    }
    None
}

/// Find the first chain member whose utilization is below its threshold
/// (has recovered headroom after switch-off-all). Returns the member name.
/// Safe to call without holding the config lock — reads from [`UsageStore`].
/// `kick_rejected` members are never "recovered": their idle-looking usage is
/// exactly the shape a messages-limiter rejection freezes them in.
pub(crate) fn find_recovered_member(
    chain: &[ChainMember],
    store: &UsageStore,
    weekly_pct: f64,
    kick_rejected: &[String],
) -> Option<String> {
    let now = now_epoch_secs();
    for member in chain {
        if kick_rejected.iter().any(|k| k == &member.name) {
            continue;
        }
        // A fetched entry whose 5h window is absent or past its reset is idle
        // headroom; a live window recovers only below the member's threshold.
        // An absent entry (never fetched) stays undecidable. A weekly-dead
        // member NEVER recovers — its 5h window lapsing every few hours is
        // exactly what made it look reborn while the 7d cap still blocks it.
        let recovered = match store.lock() {
            Ok(s) => s.get(&member.name).map(|info| {
                !weekly_blocked_info(info, now, weekly_pct)
                    && (!five_hour_live(info, now)
                        || info
                            .five_hour
                            .as_ref()
                            .is_none_or(|w| w.utilization < member.threshold))
            }),
            Err(_) => None,
        };
        if recovered == Some(true) {
            return Some(member.name.clone());
        }
    }
    None
}

/// If the active profile is a chain member past its threshold, switch to the
/// next viable member — or, in wrap-off mode when the whole chain is spent and
/// no sink exists, turn off all accounts. Returns the action taken, or None.
///
/// `active_burn_pct_per_hour` is the caller's in-memory burn rate for the
/// active profile (ignored unless burn-aware mode is on) — same contract as
/// [`next_target`], which this forwards it to.
pub(crate) fn auto_switch_if_needed(
    config: &mut AppConfig,
    active_burn_pct_per_hour: Option<f64>,
) -> Result<Option<SwitchAction>> {
    with_state_lock(|| {
        let Some(active_name) = config.state.active_profile.as_deref() else {
            return Ok(None);
        };
        if !config.state.fallback_chain.iter().any(|n| n == active_name) {
            return Ok(None);
        }
        let Some(active) = config.find(active_name) else {
            return Ok(None);
        };
        // AUTH-4 parity with the scheduler-side walk: an auth-broken active's
        // usage is frozen-stale (its fetches can't succeed), so exhaustion
        // cannot be a precondition for leaving it.
        if !config.is_auth_broken(active_name)
            && !is_exhausted_active(
                active,
                config.state.burn_aware_switching,
                config.state.refresh_interval_ms,
                active_burn_pct_per_hour,
                config.state.weekly_switch_threshold_pct(),
            )
        {
            return Ok(None);
        }

        let Some(action) = next_target(config, active_burn_pct_per_hour) else {
            return Ok(None);
        };

        match &action {
            SwitchAction::To(target) => switch_profile(config, target)?,
            SwitchAction::Off => switch_off(config)?,
        }
        Ok(Some(action))
    })
}

// ---------------------------------------------------------------------------
// CDX-4: the codex chain walk (PLAN.md §0.15) — session-boundary auto-switch
// off the passive signal. Reuses the claude walk's shapes where they map
// (ChainMember, wrapped scan order, the store-side exhaustion predicate,
// last_resort's one-migration rule) and drops the gates that don't: no
// decision_fresh (passive data self-invalidates via resets_at, and
// used_percent is monotone within a window so a stale read only
// UNDER-reports), no kick_rejected (no kicks), no `Off` action (logging the
// live codex out serves nothing — an exhausted account just errors).
// ---------------------------------------------------------------------------

/// Snapshot for the codex scan, built under the config lock by
/// [`snapshot_codex_chain`]; `leased`/`loginless` are IO-derived and filled by
/// the caller AFTER the lock drops (the `kick_rejected` pattern).
#[derive(Debug, Clone)]
pub(crate) struct CodexChainSnapshot {
    pub(crate) active: String,
    pub(crate) chain: Vec<ChainMember>,
    pub(crate) broken: Vec<String>,
    pub(crate) weekly_pct: f64,
    /// Members with a live isolated `clauth start` session — their chain
    /// lives in that session's CODEX_HOME (§0.14); installing the store
    /// snapshot would fork it. Walked around like `broken`.
    pub(crate) leased: Vec<String>,
    /// Members with no stored codex login — nothing to install.
    pub(crate) loginless: Vec<String>,
}

/// Snapshot the codex chain out of `AppConfig`. `None` for every degenerate
/// marker the scan must not act on: no codex active, an active that names a
/// deleted or claude profile (hand-edited state), an active outside the
/// chain, or an empty chain. Stray non-codex chain members are silently
/// skipped, mirroring `snapshot_chain`'s T1b tolerance.
pub(crate) fn snapshot_codex_chain(config: &AppConfig) -> Option<CodexChainSnapshot> {
    let active = config.state.active_codex_profile.as_deref()?.to_string();
    if !config.find(&active).is_some_and(|p| p.is_codex()) {
        return None;
    }
    let chain = &config.state.codex_fallback_chain;
    if !chain.iter().any(|n| n.as_str() == active) {
        return None;
    }
    let chain: Vec<ChainMember> = chain
        .iter()
        .filter(|name| config.find(name).is_some_and(|p| p.is_codex()))
        .map(|name| {
            let profile = config.find(name);
            ChainMember {
                name: name.to_string(),
                threshold: profile.map(threshold_for).unwrap_or(DEFAULT_THRESHOLD),
                last_resort: profile.is_some_and(|p| p.last_resort),
            }
        })
        .collect();
    Some(CodexChainSnapshot {
        active,
        chain,
        broken: config
            .state
            .auth_broken
            .iter()
            .map(|n| n.as_str().to_string())
            .collect(),
        weekly_pct: config.state.weekly_switch_threshold_pct(),
        leased: Vec::new(),
        loginless: Vec::new(),
    })
}

/// Codex's own limiter verdict, cross-checked against the named window's
/// `resets_at` (§0.16): `rate_limit_reached_type` says WHICH window rejected
/// the last request, and the verdict stands only while that window hasn't
/// reset. An unrecognized window name is checked against both windows —
/// trust the verdict while either could still be in force, self-clearing at
/// the later reset (never a permanent wedge).
fn codex_limiter_blocked(info: &crate::usage::UsageInfo, now_secs: i64) -> bool {
    match info.codex_rate_limit_reached.as_deref() {
        Some("primary") => crate::usage::five_hour_live(info, now_secs),
        Some("secondary") => crate::usage::seven_day_live(info, now_secs),
        Some(_) => {
            crate::usage::five_hour_live(info, now_secs)
                || crate::usage::seven_day_live(info, now_secs)
        }
        None => false,
    }
}

/// Whether one codex `UsageInfo` snapshot reads as exhausted at `threshold`:
/// the shared percent shape (weekly line, or live 5h window over threshold)
/// OR codex's own limiter verdict. The pure core shared by the store walk and
/// the CDX-5 proxy's per-account cache check.
pub(crate) fn codex_info_exhausted_at(
    info: &crate::usage::UsageInfo,
    now_secs: i64,
    threshold: f64,
    weekly_pct: f64,
) -> bool {
    codex_limiter_blocked(info, now_secs)
        || weekly_blocked_info(info, now_secs, weekly_pct)
        || (crate::usage::five_hour_live(info, now_secs)
            && info
                .five_hour
                .as_ref()
                .is_some_and(|w| w.utilization >= threshold))
}

/// [`codex_info_exhausted_at`] at the DEFAULT threshold — the CDX-5 proxy's
/// availability check (it has no per-member threshold snapshot).
pub(crate) fn codex_info_exhausted(
    info: &crate::usage::UsageInfo,
    now_secs: i64,
    weekly_pct: f64,
) -> bool {
    codex_info_exhausted_at(info, now_secs, DEFAULT_THRESHOLD, weekly_pct)
}

/// Store-side exhaustion for one codex profile. A poisoned store lock fails
/// safe to "not exhausted", same as the claude side.
fn codex_exhausted_from_store(
    name: &str,
    threshold: f64,
    store: &UsageStore,
    weekly_pct: f64,
) -> bool {
    let now = now_epoch_secs();
    match store.lock() {
        Ok(s) => s
            .get(name)
            .is_some_and(|info| codex_info_exhausted_at(info, now, threshold, weekly_pct)),
        Err(_) => false,
    }
}

/// The codex scan's decision: the next chain member to install, or `None`.
/// Fires only when the ACTIVE codex profile is exhausted (percent shape or
/// limiter verdict); candidates must be installable (not broken / leased /
/// loginless) and not exhausted themselves — a member with no data, or whose
/// cached windows have lapsed (`resets_at` passed ⇒ reset), is viable. Then
/// the `last_resort` pass under the same one-migration rule as the claude
/// walk. No `Off` arm by design (module note above).
pub(crate) fn next_codex_auto_switch_target(
    snapshot: &CodexChainSnapshot,
    store: &UsageStore,
) -> Option<String> {
    let active_idx = snapshot
        .chain
        .iter()
        .position(|m| m.name == snapshot.active)?;
    let len = snapshot.chain.len();

    let active = &snapshot.chain[active_idx];
    let active_broken = snapshot.broken.iter().any(|b| b == &active.name);
    if !active_broken
        && !codex_exhausted_from_store(&active.name, active.threshold, store, snapshot.weekly_pct)
    {
        return None;
    }

    let skip = |i: usize| {
        let m = &snapshot.chain[i];
        m.name == snapshot.active
            || snapshot.broken.iter().any(|b| b == &m.name)
            || snapshot.leased.iter().any(|l| l == &m.name)
            || snapshot.loginless.iter().any(|l| l == &m.name)
    };
    let pick = walk_chain(active_idx, len, &skip, &|i| {
        let m = &snapshot.chain[i];
        !codex_exhausted_from_store(&m.name, m.threshold, store, snapshot.weekly_pct)
    });
    if let Some(i) = pick {
        return Some(snapshot.chain[i].name.clone());
    }

    // Last resort: accepted even while exhausted — but never from a
    // last-resort active (two sinks migrating to each other gains nothing).
    if active.last_resort {
        return None;
    }
    walk_chain(active_idx, len, &skip, &|i| snapshot.chain[i].last_resort)
        .map(|i| snapshot.chain[i].name.clone())
}

#[cfg(test)]
#[path = "../tests/inline/fallback.rs"]
mod tests;
