//! Profile → JSON view helpers shared by the `mcp` server, the `daemon`
//! status writer, and `clauth status --json`. Every reader sources usage from
//! the same on-disk `usage_cache.json` (written by the scheduler), so these
//! functions are process-independent: they return the last-persisted numbers
//! whether or not a scheduler is live. One home for the shape keeps the three
//! surfaces from drifting.

use crate::profile::Profile;
use crate::profile_cache::{USAGE_CACHE_FILE, load_profile_cache};
use crate::usage::{PlanTier, UsageInfo};

/// Display provider for a profile: a recognised third-party name, else
/// `anthropic` for an OAuth profile.
pub(crate) fn provider_label(profile: &Profile) -> String {
    profile
        .provider
        .map(|p| p.display_name().to_string())
        .unwrap_or_else(|| "anthropic".to_string())
}

/// Human account-tier label for an OAuth profile, preferring the fetched plan
/// tier (carries the Max multiplier, e.g. `Max 5x`) over the bare OAuth
/// `subscription_type` token (`max`). `None` for third-party/api-key profiles
/// and when neither a fetched plan nor a token hint is on disk.
pub(crate) fn tier_label(profile: &Profile) -> Option<String> {
    if profile.is_third_party() {
        return None;
    }
    let fetched = load_profile_cache::<UsageInfo>(profile.name.as_str(), USAGE_CACHE_FILE)
        .and_then(|u| u.plan)
        .filter(|p| p.tier != PlanTier::Unknown);
    match fetched {
        Some(plan) => {
            let short = plan.tier.short_label()?;
            Some(if plan.is_canceled() {
                format!("{short} · canceled")
            } else {
                short
            })
        }
        None => {
            let sub = profile
                .credentials
                .as_ref()?
                .claude_ai_oauth
                .as_ref()?
                .subscription_type
                .as_deref()?;
            PlanTier::from_subscription_type(Some(sub)).short_label()
        }
    }
}

/// The profile's usage windows as a JSON array of `{label, utilization_pct,
/// resets_at}` — 5h, 7d, then one entry per weekly model window (`7d <model>`) —
/// read fresh from the disk cache. Empty array when no cache yet.
pub(crate) fn windows_json(name: &str) -> serde_json::Value {
    let Some(usage) = load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE) else {
        return serde_json::Value::Array(Vec::new());
    };
    let windows: Vec<serde_json::Value> = usage
        .windows()
        .into_iter()
        .map(|(label, w)| {
            serde_json::json!({
                "label": label,
                "utilization_pct": w.utilization,
                "resets_at": w.resets_at,
            })
        })
        .collect();
    serde_json::Value::Array(windows)
}
