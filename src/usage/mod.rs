mod burn;
mod fetch;
mod scheduler;

pub(crate) use burn::{
    BURN_GAP_CUT_MS, BURN_LOOKBACK_MS, BURN_MIN_SAMPLES, compute_burn_rates_from_history,
    project_utilization,
};
#[allow(unused_imports)]
pub(crate) use fetch::{
    ANTHROPIC_ORIGIN, ExtraPeriod, ExtraUsage, LABEL_5H, LABEL_7D, PlanInfo, PlanTier,
    ScopedWindow, SpendInfo, UsageInfo, UsageWindow, WindowDollars, await_request_slot,
    cli_user_agent, epoch_secs_to_iso, expire_profile_ttl, fetch_account_uuid, five_hour_live,
    http_agent, humanize_duration, ideal_pace_pct, iso_to_epoch_secs, now_epoch_secs, now_ms,
    parse_retry_after, probe_subscription_type, seven_day_live, window_avg_pace_per_day,
};
pub(crate) use scheduler::{
    ActivityStore, FetchStatus, LastFetchedAt, NextRefreshPerProfile, OpResult, OpResultReceiver,
    OpResultSender, PendingSwitch, PendingSwitchOff, ProfileActivity, RateLimitStreaks,
    RefetchQueue, StartupReceiver, StartupSender, StartupSignal, StatusStore,
    SuppressedGenericStore, ThirdPartyList, ThirdPartyStatusStore, ThirdPartyUsageStore, TokenList,
    UsageStore, any_busy, bootstrap_fetch, bootstrap_third_party, clear_activity,
    collect_third_party_entries, collect_tokens, is_idle, is_stuck_rate_limited, mark_activity,
    spawn_refresher, switch_gate_in_flight,
};
// The active-cap boundary is only referenced by tests (production code reaches it
// through `is_stuck_rate_limited`); gate the re-export behind `cfg(test)` so it
// isn't a dead symbol in the shipped binary, while keeping the `stale`/distrust
// tests robust against a change to the constant's value.
#[cfg(test)]
pub(crate) use scheduler::ACTIVE_CAP_MAX_STREAK;
// Test-only: reset the per-host request-spacing slots so a real-bytes wire test
// driving a builder through `await_request_slot` doesn't sleep out the window.
#[cfg(test)]
pub(crate) use fetch::reset_request_slots;
// The `/profile` TTL decision itself, re-exported for the account-swap tests in
// `actions`: asserting through the real decision proves a swap expired BOTH
// halves of the clock (memo + durable stamp), which no fixture of it would.
#[cfg(test)]
pub(crate) use fetch::take_profile_fetch;
