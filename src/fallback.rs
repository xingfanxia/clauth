use anyhow::Result;

use crate::actions::{switch_off, switch_profile};
use crate::lock::with_state_lock;
use crate::profile::{AppConfig, Profile};
use crate::usage::{
    FetchStatus, UsageStore, UsageWindow, five_hour_live, iso_to_epoch_secs, now_epoch_secs,
    seven_day_live,
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

/// Fraction of the binding spend cap a member may reach and still be picked for
/// spend reasons. The 10% slack is poll drift insurance: utilization is sampled
/// on a cadence, not streamed, so a member picked AT its cap would overshoot it
/// before the next read lands. Real money, so the margin errs low.
const SPEND_ARM_FRACTION: f64 = 0.90;

/// Dollars this member may still spend automatically, or `None` when it may not
/// spend at all. Both halves of the opt-in must be set (`budget_on` chain-wide,
/// a `ceiling > 0` on the member) AND the account must actually be billing
/// pay-as-you-go.
///
/// `enabled` is the honest field here, NOT [`crate::usage::SpendInfo::is_visible`]:
/// that one answers "is a spend bar worth rendering" and is true for an account
/// carrying a stale limit with billing switched OFF (observed live 2026-07-17).
/// Arming on it would hop the chain into an account that refuses the request,
/// with money attached.
///
/// The cap is whichever binds first — the account's own limit or the member's
/// ceiling. An account with billing on but no declared limit is bounded by the
/// ceiling alone; that is the point of the ceiling.
fn spend_room(spend: &crate::usage::SpendInfo, ceiling: f64) -> Option<f64> {
    // `is_finite` is load-bearing, not decoration: `max_auto_spend = inf` is
    // valid TOML, and an infinite ceiling with no account cap yields infinite
    // room — unlimited unattended spending. NaN would slip a plain `<= 0.0`
    // (every NaN comparison is false) and then `f64::min(NaN, cap)` returns the
    // cap, arming at the account's full limit. The load boundary normalizes
    // both away; this refuses them again for any other construction path.
    if !spend.enabled || !ceiling.is_finite() || ceiling <= 0.0 {
        return None;
    }
    let cap = match spend.limit {
        Some(limit) => limit.min(ceiling),
        None => ceiling,
    };
    // Unknown spend REFUSES rather than reading as $0 spent. `used` is the only
    // input bounding what gets spent, and `RawMoney::to_dollars` returns `None`
    // whenever `amount_minor` is absent or renamed — so defaulting it to 0 hands
    // back the full cap on any wire drift, which is the most permissive possible
    // answer to "how much has this already cost?". Fail closed on money.
    let used = spend.used?;
    let room = SPEND_ARM_FRACTION * cap - used;
    (room > 0.0).then_some(room)
}

/// True when the ACTIVE profile is billing pay-as-you-go and has spent the
/// budget the operator allowed it — the moment `max_auto_spend` has to stop
/// being an entry gate and start being a cap.
///
/// Deliberately narrow, because acting on it HALTS accounts: it needs the
/// chain-wide opt-in, a real ceiling on this member, and the account to actually
/// be billing. Anything else (a normal subscription account, a $0 ceiling, the
/// toggle off) is `false`, so the halt path stays bit-identical to pre-2026-07-17
/// for every chain that never opted in.
///
/// Only meaningful once the member's subscription windows are spent — an account
/// with quota left is not billing, so callers check exhaustion first (an
/// unexhausted member never reaches the halt decision anyway).
fn budget_spent(usage: Option<&crate::usage::UsageInfo>, budget_on: bool, ceiling: f64) -> bool {
    usage.and_then(|u| u.spend.as_ref()).is_some_and(|spend| {
        budget_on
            && spend.enabled
            && ceiling.is_finite()
            && ceiling > 0.0
            && spend_room(spend, ceiling).is_none()
    })
}

/// Whether this member's live config can bill with nothing stopping it: armed to
/// spend, but told to stay on the account once the budget runs out, and no free
/// parking spot to be sent to instead. `max_auto_spend` then bounds only when
/// the chain STARTS paying, never when it stops.
///
/// ONE predicate, two consumers, so the warning and the behavior cannot drift:
/// the Fallback card renders it as a DANGER tooltip under the ceiling it
/// describes, and the daemon says it at boot for the headless case where nobody
/// is watching that card. Policy, not presentation — which is why it lives here
/// beside the rules it is reading, not in `tui::render`.
pub(crate) fn spend_is_uncapped(config: &AppConfig, ceiling: f64) -> bool {
    config.state.spend_budget_switching
        && ceiling > 0.0
        && !config.state.switch_off_when_budget_spent
        && !config.profiles.iter().any(|p| p.last_resort)
}

/// [`spend_room`] over a member's usage snapshot: true when the chain may pick
/// it purely to spend money. Never consulted until every subscription member
/// with free quota has been passed over — see [`next_target`].
fn spend_armed(usage: Option<&crate::usage::UsageInfo>, budget_on: bool, ceiling: f64) -> bool {
    budget_on
        && usage
            .and_then(|u| u.spend.as_ref())
            .and_then(|s| spend_room(s, ceiling))
            .is_some()
}

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
fn burn_rate_for_profile(name: &str, window: &UsageWindow) -> Option<f64> {
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
    /// Mirrors `Profile::max_auto_spend` in dollars, `0` when unset — the
    /// member's own ceiling on unattended pay-as-you-go spending.
    pub(crate) max_spend: f64,
}

/// In-memory snapshot of the fields `next_auto_switch_target` needs: active
/// profile name + ordered chain with each member's resolved threshold. Built
/// under the `AppConfig` mutex by [`snapshot_chain`], then evaluated lock-free.
#[derive(Debug, Clone)]
pub(crate) struct ChainSnapshot {
    pub(crate) active: String,
    pub(crate) chain: Vec<ChainMember>,
    /// Snapshot of `AppState::switch_off_when_spent` — drives the switch-off-all decision.
    pub(crate) switch_off_when_spent: bool,
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
    /// hot-path `Arc<AtomicU64>`, mirroring exactly how `switch_off_when_spent` reaches
    /// this struct.
    pub(crate) interval_ms: u64,
    /// Snapshot of `AppState::weekly_switch_threshold_pct()` — the chain-wide
    /// weekly (7d) exhaustion line both walk directions gate on.
    pub(crate) weekly_pct: f64,
    /// Snapshot of `AppState::spend_budget_switching` — the master half of the
    /// real-money opt-in. Off (the default) makes the spend pass inert whatever
    /// any member's ceiling says.
    pub(crate) spend_budget: bool,
    /// Snapshot of `AppState::switch_off_when_budget_spent` — `switch_off_when_spent`'s twin for the case
    /// where what ran out is money rather than quota.
    pub(crate) switch_off_when_budget_spent: bool,
    /// Members whose 5h auto-start kick the messages limiter is REJECTING
    /// (switch-grade `KickBlock`s: the limiter's own `rejected` verdict, ≥2
    /// consecutive kicks, advertised ceiling still ahead). Not config state —
    /// [`snapshot_chain`] leaves it empty and the scheduler's scan fills it
    /// from the live kick-block map. A rejected member can't serve inference
    /// even with idle-looking usage, so it is walked around like `broken`, and
    /// a rejected ACTIVE bypasses the exhaustion gate the same way.
    pub(crate) kick_rejected: Vec<String>,
    /// Members whose last store read was live (`FetchStatus::Fresh`) — the same
    /// freshness `decision_fresh` gates the ACTIVE on. Not config state:
    /// [`snapshot_chain`] leaves it empty and the scheduler's scan fills it from
    /// the `StatusStore` (like `kick_rejected`). Drives the headroom walk's
    /// fresh-PREFERENCE first pass — a PREFERENCE, never a gate: when no fresh
    /// member has headroom the walk still accepts a stale-but-unexhausted one,
    /// so an exhausted active never loses its escape (2026-06-28 target
    /// asymmetry: the walk gates only the ACTIVE, never the target).
    pub(crate) fresh: Vec<String>,
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
        .map(|name| {
            let profile = config.find(name);
            ChainMember {
                name: name.to_string(),
                threshold: profile.map(threshold_for).unwrap_or(DEFAULT_THRESHOLD),
                last_resort: profile.is_some_and(|p| p.last_resort),
                max_spend: profile.and_then(|p| p.max_auto_spend).unwrap_or(0.0),
            }
        })
        .collect();
    Some(ChainSnapshot {
        active,
        chain,
        switch_off_when_spent: config.state.switch_off_when_spent,
        broken: config
            .state
            .auth_broken
            .iter()
            .map(|n| n.as_str().to_string())
            .collect(),
        burn_aware: config.state.burn_aware_switching,
        interval_ms: config.state.refresh_interval_ms,
        weekly_pct: config.state.weekly_switch_threshold_pct(),
        spend_budget: config.state.spend_budget_switching,
        switch_off_when_budget_spent: config.state.switch_off_when_budget_spent,
        kick_rejected: Vec::new(),
        fresh: Vec::new(),
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

    let skip = |i: usize| {
        chain[i] == active || config.find(&chain[i]).is_none() || config.is_auth_broken(&chain[i])
    };
    let walk = |accept: &dyn Fn(&Profile) -> bool| -> Option<String> {
        let pick = walk_chain(active_idx, len, &skip, &|i| {
            config.find(&chain[i]).is_some_and(&accept)
        });
        pick.map(|i| chain[i].to_string())
    };

    // Two-pass headroom PREFERENCE (not a gate): pass 1a prefers a candidate
    // whose usage read we TRUST (`fetch_status == Fresh`); pass 1b, run only
    // when 1a finds nothing, is the verbatim old accept — any-freshness
    // headroom. Pass 1b MUST always run so an exhausted active still escapes to
    // a stale-but-viable member: freshness is a preference here, never a gate
    // (2026-06-28 asymmetry — the target walk is never gated, only the ACTIVE).
    let is_fresh = |p: &Profile| p.fetch_status == Some(FetchStatus::Fresh);
    if let Some(name) = walk(&|p| !is_exhausted(p, weekly_pct) && is_fresh(p)) {
        return Some(SwitchAction::To(name));
    }
    if let Some(name) = walk(&|p| !is_exhausted(p, weekly_pct)) {
        return Some(SwitchAction::To(name));
    }

    // Spend-armed members rank PRE-last-resort: every member above still had
    // free subscription quota, so reaching here means paying is the only way
    // forward. Ranked above `last_resort` parking and wrap-off because both of
    // those stop work outright, and an operator who set a ceiling asked to keep
    // working. Opt-in twice over (see `spend_armed`), so stock chains skip this.
    let spend_budget = config.state.spend_budget_switching;

    // An active that is ITSELF still within budget stays put — the same loop
    // guard `last_resort` needs, for the same reason: hopping between two paying
    // members buys nothing and relinks live credentials every tick. Everything
    // with free quota was already passed over above, so the choice here is only
    // "which account to pay", and the one already installed wins. An OVER-budget
    // active isn't armed, so it falls through to the halt gate below instead of
    // parking here.
    let active_is_spend_armed = config.find(active).is_some_and(|p| {
        spend_armed(
            p.usage.as_ref(),
            spend_budget,
            p.max_auto_spend.unwrap_or(0.0),
        )
    });
    if active_is_spend_armed {
        return None;
    }
    if let Some(name) = walk(&|p| {
        spend_armed(
            p.usage.as_ref(),
            spend_budget,
            p.max_auto_spend.unwrap_or(0.0),
        )
    }) {
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
    // above stays on the static `is_exhausted`. Weekly-wise Off keys on the
    // HARD cap ([`WEEKLY_HARD_BLOCK_PCT`]), not the soft line: a soft-blocked
    // active with real weekly room left stays put instead of signing every
    // running claude out over the tail it could still spend.
    //
    // Which flag answers that depends on WHAT ran out. An active that spent its
    // pay-as-you-go budget reads `switch_off_when_budget_spent` instead of `switch_off_when_spent`: `wrap
    // off` means "the chain is out of free quota", where staying costs nothing
    // but rate-limit errors, and this is the case where staying IS the spending.
    // Note the `last_resort` walk above already ran, so an over-budget active
    // parks on a sink when one exists — stopping the billing without signing
    // anyone out — and only reaches this halt with nowhere free to go.
    let halt_flag = if budget_spent(
        config.find(active).and_then(|p| p.usage.as_ref()),
        config.state.spend_budget_switching,
        config
            .find(active)
            .and_then(|p| p.max_auto_spend)
            .unwrap_or(0.0),
    ) {
        config.state.switch_off_when_budget_spent
    } else {
        config.state.switch_off_when_spent
    };
    if halt_flag
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

    // Two-pass headroom PREFERENCE, lockstep with [`next_target`] (not a gate):
    // pass 1a prefers a member whose last store read was live — carried on
    // `snapshot.fresh`, since the `UsageStore` value (`UsageInfo`) holds no
    // status; the freshness comes from the same `StatusStore` `decision_fresh`
    // gates the ACTIVE on. Pass 1b, run only when 1a is empty, is today's
    // any-freshness accept verbatim, so an exhausted active keeps its
    // stale-but-viable escape (2026-06-28 target asymmetry).
    let is_fresh = |m: &ChainMember| snapshot.fresh.iter().any(|n| n == &m.name);
    if let Some(name) = walk(&|m| {
        !is_exhausted_from_store(&m.name, m.threshold, store, snapshot.weekly_pct) && is_fresh(m)
    }) {
        return Some(SwitchAction::To(name));
    }
    if let Some(name) =
        walk(&|m| !is_exhausted_from_store(&m.name, m.threshold, store, snapshot.weekly_pct))
    {
        return Some(SwitchAction::To(name));
    }

    // Spend-armed pass, lockstep with [`next_target`]: pre-last-resort, and the
    // spend block rides the store's `UsageInfo` exactly like utilization does.
    // Same loop guard — an active still within budget stays put rather than
    // ping-ponging between two paying members.
    let active_is_spend_armed = store
        .lock()
        .ok()
        .is_some_and(|s| spend_armed(s.get(&active.name), snapshot.spend_budget, active.max_spend));
    if active_is_spend_armed {
        return None;
    }
    if let Some(name) = walk(&|m| {
        store
            .lock()
            .ok()
            .is_some_and(|s| spend_armed(s.get(&m.name), snapshot.spend_budget, m.max_spend))
    }) {
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
    // off would log it out over a flag, not over spent quota. Same principle
    // weekly-wise: `active_exhausted` above (the switch trigger) honors the
    // soft line, but Off re-checks at the HARD cap
    // ([`WEEKLY_HARD_BLOCK_PCT`]) — a soft-blocked active with real weekly
    // room left stays put rather than signing everything out over the tail.
    // Budget-aware halt flag, lockstep with [`next_target`]: an active that
    // spent its pay-as-you-go budget reads `switch_off_when_budget_spent`, since staying on
    // it keeps costing money rather than merely erroring.
    let active_budget_spent = store.lock().ok().is_some_and(|s| {
        budget_spent(s.get(&active.name), snapshot.spend_budget, active.max_spend)
    });
    let halt_flag = if active_budget_spent {
        snapshot.switch_off_when_budget_spent
    } else {
        snapshot.switch_off_when_spent
    };
    if halt_flag
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

#[cfg(test)]
#[path = "../tests/inline/fallback.rs"]
mod tests;
