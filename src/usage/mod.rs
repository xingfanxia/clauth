mod fetch;
mod scheduler;

pub(crate) use fetch::{
    PlanInfo, UsageInfo, UsageWindow, humanize_duration, iso_to_epoch_secs, now_epoch_secs, now_ms,
};
// Only the `#[cfg(test)]` showcase names this type directly; elsewhere it's
// reached through the `UsageInfo::extra_usage` field, so the re-export would be
// an unused import in a normal build.
#[cfg(test)]
pub(crate) use fetch::ExtraUsage;
pub(crate) use scheduler::{
    ActivityKind, ActivityStore, FetchStatus, LastFetchedAt, LastRotatedWindow,
    NextRefreshPerProfile, OpResult, OpResultReceiver, OpResultSender, PendingAutoStart,
    PendingSwitch, PendingSwitchOff, PendingWindowRotation, ProfileActivity, RefetchQueue,
    StartupReceiver, StartupSender, StartupSignal, StatusStore, TokenEntry, TokenList, UsageStore,
    any_busy, clear_activity, fetch_all_into, is_idle, mark_activity, spawn_refresher,
};
