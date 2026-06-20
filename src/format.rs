//! Pure profile/usage → display-string formatters. No UI dependencies, so
//! both the TUI and CLI subcommands (e.g. `clauth which`) can share them.

use crate::profile::Profile;
use crate::usage::{PlanInfo, PlanTier};

/// Trailing-ellipsis truncation to `max` chars (counts `char`s, not bytes).
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub(crate) fn endpoint_label(profile: &Profile) -> String {
    if let Some(url) = &profile.base_url {
        return url.clone();
    }
    if let Some(plan) = profile.usage.as_ref().and_then(|u| u.plan.as_ref()) {
        return plan_label(plan);
    }
    // No fetched plan yet — fall back to the OAuth token's subscription_type.
    let sub = profile
        .credentials
        .as_ref()
        .and_then(|c| c.claude_ai_oauth.as_ref())
        .and_then(|o| o.subscription_type.as_deref());
    PlanTier::from_subscription_type(sub).display()
}

pub(crate) fn plan_label(plan: &PlanInfo) -> String {
    plan.tier.display()
}
