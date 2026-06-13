//! Pure profile/usage → display-string formatters. No UI dependencies, so
//! both the TUI and CLI subcommands (e.g. `clauth which`) can share them.

use crate::profile::Profile;
use crate::usage::PlanInfo;

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
    let sub = profile
        .credentials
        .as_ref()
        .and_then(|c| c.claude_ai_oauth.as_ref())
        .and_then(|o| o.subscription_type.as_deref())
        .unwrap_or("pro");
    format!("Claude {}", titlecase(sub))
}

pub(crate) fn plan_label(plan: &PlanInfo) -> String {
    let org = plan.organization_type.as_deref().unwrap_or("");
    let base = match org {
        "claude_max" => "Max".to_string(),
        "claude_pro" => "Pro".to_string(),
        "claude_team" | "claude_teams" => "Team".to_string(),
        "claude_enterprise" => "Enterprise".to_string(),
        "claude_free" | "free" => "Free".to_string(),
        "" => {
            if plan.has_max {
                "Max".to_string()
            } else if plan.has_pro {
                "Pro".to_string()
            } else {
                return "Claude".to_string();
            }
        }
        other => titlecase(other.strip_prefix("claude_").unwrap_or(other)),
    };

    if base == "Max"
        && let Some(tier) = plan.rate_limit_tier.as_deref()
        && let Some(mult) = max_multiplier(tier)
    {
        return format!("Claude Max {mult}x");
    }
    format!("Claude {base}")
}

fn max_multiplier(tier: &str) -> Option<&str> {
    let last = tier.rsplit('_').next()?;
    last.strip_suffix('x')
        .filter(|m| m.chars().all(|c| c.is_ascii_digit()))
}

pub(crate) fn titlecase(s: &str) -> String {
    s.split(['_', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
