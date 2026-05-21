//! Shared formatters and style helpers used across multiple screens.
//!
//! Anything that maps profile/usage state to a `Span`, `Style`, or display
//! string and is referenced by more than one screen lives here. Screen-only
//! helpers (e.g. fallback flow widget, overview column math) stay on their
//! screen module.

use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::Span;

use super::super::theme;
use crate::profile::Profile;
use crate::usage::{FetchStatus, UsageWindow, iso_to_epoch_secs, now_epoch_secs};

pub(super) fn fixed(value: &str, width: usize) -> String {
    let mut out = String::new();
    for (i, ch) in value.chars().enumerate() {
        if i >= width {
            break;
        }
        out.push(ch);
    }
    if value.chars().count() > width && width > 0 {
        out.pop();
        out.push('…');
    }
    let len = out.chars().count();
    if len < width {
        out.push_str(&" ".repeat(width - len));
    }
    out
}

pub(super) fn name_style(profile: &Profile) -> Style {
    let base = Style::default().fg(theme::TEXT).bold();
    if !profile.is_oauth() {
        return base;
    }
    match profile.fetch_status {
        Some(FetchStatus::Cached) => base
            .underline_color(theme::WARNING)
            .add_modifier(Modifier::UNDERLINED),
        Some(FetchStatus::Failed) => base
            .underline_color(theme::DANGER)
            .add_modifier(Modifier::UNDERLINED),
        _ => base,
    }
}

pub(super) fn account_type_label(profile: &Profile) -> String {
    if !profile.is_oauth() {
        return "API".to_string();
    }
    let label = endpoint_label(profile);
    label
        .strip_prefix("Claude ")
        .unwrap_or(label.as_str())
        .to_string()
}

pub(super) fn account_type_style(profile: &Profile) -> Style {
    if !profile.is_oauth() {
        theme::accent()
    } else {
        Style::default().fg(theme::ACCENT_2)
    }
}

pub(super) fn endpoint_label(profile: &Profile) -> String {
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

pub(super) fn plan_label(plan: &crate::usage::PlanInfo) -> String {
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

pub(super) fn titlecase(s: &str) -> String {
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

pub(super) fn bar_string_with_cells(pct: f64, cells: usize) -> String {
    let pct = pct.clamp(0.0, 100.0);
    let filled = ((pct / 100.0) * cells as f64).round() as usize;
    let filled = filled.min(cells);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(cells - filled))
}

pub(super) fn format_reset(window: &UsageWindow) -> Option<String> {
    let resets_at = window.resets_at.as_deref()?;
    let target = iso_to_epoch_secs(resets_at)?;
    let secs = target - now_epoch_secs();
    Some(crate::usage::humanize_duration(secs))
}

pub(super) fn window_summary_span(
    window: Option<&UsageWindow>,
    width: usize,
    include_bar: bool,
) -> Span<'static> {
    let Some(window) = window else {
        return Span::styled(fixed("—", width), theme::faint());
    };
    let pct = window.utilization.clamp(0.0, 100.0);
    let color = theme::util_color(pct);
    let text = if include_bar && width >= 26 {
        let reset = format_reset(window)
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        format!("{} {:>3.0}%{reset}", bar_string_with_cells(pct, 10), pct)
    } else if include_bar && width >= 17 {
        format!("{} {:>3.0}%", bar_string_with_cells(pct, 10), pct)
    } else if include_bar && width >= 12 {
        let bar_cells = width.saturating_sub(5).clamp(3, 7);
        format!("{}{:>3.0}", bar_string_with_cells(pct, bar_cells), pct)
    } else {
        format!("{pct:>3.0}%")
    };
    Span::styled(fixed(&text, width), Style::default().fg(color))
}
