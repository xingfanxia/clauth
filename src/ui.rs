use inquire::ui::{Attributes, Color, RenderConfig, StyleSheet, Styled};

use crate::profile::Profile;
use crate::usage::{PlanInfo, UsageInfo, UsageWindow, iso_to_epoch_secs, now_epoch_secs};

// ── Terminal palette (cloudy-ui CLI) ──────────────────────────────────────────

pub(crate) const C_RESET: &str = "\x1b[0m";
pub(crate) const C_BOLD: &str = "\x1b[1m";
// Targeted resets — used so inquire's "selected = bold" wrapper does not
// either leak through the whole label or get killed by an early full reset.
pub(crate) const C_NOBOLD: &str = "\x1b[22m"; // normal intensity, keeps current color
pub(crate) const C_FG_OFF: &str = "\x1b[39m"; // default foreground, keeps current attrs
pub(crate) const C_ACCENT: &str = "\x1b[38;2;67;171;229m"; // sapphire
pub(crate) const C_ORANGE: &str = "\x1b[38;2;217;119;87m"; // claude orange
pub(crate) const C_WARNING: &str = "\x1b[38;2;249;226;175m";
pub(crate) const C_DANGER: &str = "\x1b[38;2;243;139;168m";
pub(crate) const C_DIM: &str = "\x1b[38;2;166;173;200m";
pub(crate) const C_FAINT: &str = "\x1b[38;2;127;132;156m";

fn titlecase_words(s: &str) -> String {
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

/// Pulls the "5x"/"20x" multiplier off a tier string like
/// `default_claude_max_5x`. Returns None when the tier doesn't end in `Nx`.
fn max_multiplier(tier: &str) -> Option<&str> {
    let last = tier.rsplit('_').next()?;
    last.strip_suffix('x')
        .filter(|m| m.chars().all(|c| c.is_ascii_digit()))
}

fn plan_label(plan: &PlanInfo) -> String {
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
        other => titlecase_words(other.strip_prefix("claude_").unwrap_or(other)),
    };

    if base == "Max"
        && let Some(tier) = plan.rate_limit_tier.as_deref()
        && let Some(mult) = max_multiplier(tier)
    {
        return format!("Claude Max {mult}x");
    }
    format!("Claude {base}")
}

fn endpoint_label(profile: &Profile) -> String {
    if let Some(url) = &profile.base_url {
        return url.clone();
    }
    if let Some(plan) = profile.usage.as_ref().and_then(|u| u.plan.as_ref()) {
        return plan_label(plan);
    }
    // Fallback for offline / unfetched profiles: trust the (less reliable) tag
    // the OAuth credentials shipped with us.
    let sub = profile
        .credentials
        .as_ref()
        .and_then(|c| c.claude_ai_oauth.as_ref())
        .and_then(|o| o.subscription_type.as_deref())
        .unwrap_or("pro");
    format!("Claude {}", titlecase_words(sub))
}

fn format_reset(window: &UsageWindow) -> Option<String> {
    let resets_at = window.resets_at.as_deref()?;
    let target = iso_to_epoch_secs(resets_at)?;
    let secs = target - now_epoch_secs();
    Some(humanize_duration(secs))
}

fn humanize_duration(secs: i64) -> String {
    if secs <= 0 {
        return "now".to_string();
    }
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    if days > 0 {
        format!("{}d {}h", days, hours % 24)
    } else if hours > 0 {
        format!("{}h {}m", hours, mins % 60)
    } else {
        format!("{}m", mins.max(1))
    }
}

fn bar_for(pct: f64) -> (String, &'static str) {
    let pct = pct.clamp(0.0, 100.0);
    let filled = ((pct / 100.0) * 10.0).round() as usize;
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(10 - filled));
    let color = if pct >= 80.0 {
        C_DANGER
    } else if pct >= 60.0 {
        C_WARNING
    } else {
        C_DIM
    };
    (bar, color)
}

fn five_hour_chunk(info: &UsageInfo) -> String {
    let Some(window) = &info.five_hour else {
        return String::new();
    };
    let (bar, color) = bar_for(window.utilization);
    let pct = window.utilization.clamp(0.0, 100.0);
    let reset = format_reset(window)
        .map(|r| format!("{C_FAINT} · resets {r}{C_RESET}"))
        .unwrap_or_default();
    format!("  {color}[{bar}] {pct:.0}%{C_RESET}{reset}")
}

fn seven_day_chunk(info: &UsageInfo) -> String {
    let Some(window) = &info.seven_day else {
        return String::new();
    };
    let pct = window.utilization.clamp(0.0, 100.0);
    let color = if pct >= 80.0 {
        C_DANGER
    } else if pct >= 60.0 {
        C_WARNING
    } else {
        C_FAINT
    };
    let reset = format_reset(window)
        .map(|r| format!(" in {r}"))
        .unwrap_or_default();
    format!("{C_FAINT} · {color}7d {pct:.0}%{C_FAINT}{reset}{C_RESET}")
}

fn extra_usage_chunk(info: &UsageInfo) -> String {
    let Some(extra) = &info.extra_usage else {
        return String::new();
    };
    if !extra.is_enabled {
        return String::new();
    }
    let used = extra.used_credits.unwrap_or(0.0);
    let limit = extra.monthly_limit.unwrap_or(0.0);
    let currency = extra.currency.as_deref().unwrap_or("");
    let pct = extra.utilization.unwrap_or(0.0).clamp(0.0, 100.0);
    let color = if pct >= 80.0 {
        C_DANGER
    } else if pct >= 60.0 {
        C_WARNING
    } else {
        C_FAINT
    };
    let prefix = if currency.is_empty() { "" } else { " " };
    format!(
        "{C_FAINT} · {color}extra {used_div:.2}/{limit_div:.2}{prefix}{currency}{C_RESET}",
        used_div = used / 100.0,
        limit_div = limit / 100.0,
    )
}

/// Visible width (chars, no ANSI) of the plan/URL label. Used to align the
/// usage bar across rows based on the longest endpoint label only.
pub(crate) fn endpoint_visible_width(profile: &Profile) -> usize {
    endpoint_label(profile).chars().count()
}

/// Plan/usage info is OAuth-only — an API-endpoint profile uses the
/// proxy's own quota and Anthropic's `/oauth/usage` numbers don't apply.
fn is_oauth_profile(profile: &Profile) -> bool {
    profile.base_url.is_none()
}

pub(crate) fn format_profile_entry(
    profile: &Profile,
    is_active: bool,
    name_width: usize,
    endpoint_width: usize,
) -> String {
    let endpoint = endpoint_label(profile);
    let usage_hint = if is_oauth_profile(profile) {
        profile
            .usage
            .as_ref()
            .map(five_hour_chunk)
            .unwrap_or_default()
    } else {
        String::new()
    };
    // Only pad to the alignment column when this row actually has a bar.
    // API-profile rows let their suffixes hug the URL instead of dangling
    // at the end of an empty column.
    let endpoint_pad = if usage_hint.is_empty() {
        String::new()
    } else {
        " ".repeat(endpoint_width.saturating_sub(endpoint.chars().count()))
    };
    let key_hint = if profile.base_url.is_some() && profile.api_key.is_some() {
        format!("{C_FAINT} · API key set{C_RESET}")
    } else {
        String::new()
    };
    let cred_warn = if profile.credentials.is_none() {
        format!("{C_WARNING} · no credentials{C_RESET}")
    } else {
        String::new()
    };
    let name = &profile.name;

    if is_active {
        format!(
            "{C_ACCENT}● {name:<name_width$}{C_NOBOLD}  {C_DIM}{endpoint}{endpoint_pad}{C_RESET}{usage_hint}{key_hint}{cred_warn}"
        )
    } else {
        format!(
            "  {name:<name_width$}{C_NOBOLD}  {C_DIM}{endpoint}{endpoint_pad}{C_RESET}{usage_hint}{key_hint}{cred_warn}"
        )
    }
}

pub(crate) fn format_submenu_title(profile: &Profile) -> String {
    let name = &profile.name;
    let url = endpoint_label(profile);
    let credentials = if profile.credentials.is_none() {
        format!(" · {C_WARNING}no credentials")
    } else {
        String::new()
    };
    let usage = if is_oauth_profile(profile) {
        profile.usage.as_ref()
    } else {
        None
    };
    let five_hour = usage.map(five_hour_chunk).unwrap_or_default();
    let seven_day = usage.map(seven_day_chunk).unwrap_or_default();
    let extra = usage.map(extra_usage_chunk).unwrap_or_default();
    format!(
        "{C_BOLD}{name}{C_RESET}{C_FAINT} · {C_RESET}{C_DIM}{url}{C_FAINT}{credentials}{C_RESET}{five_hour}{seven_day}{extra}"
    )
}

pub(crate) fn build_render_config() -> RenderConfig<'static> {
    let orange = Color::Rgb {
        r: 217,
        g: 119,
        b: 87,
    };
    let blue = Color::Rgb {
        r: 67,
        g: 171,
        b: 229,
    };
    let faint = Color::Rgb {
        r: 127,
        g: 132,
        b: 156,
    };

    RenderConfig::default()
        .with_prompt_prefix(Styled::new("?").with_fg(blue))
        .with_answered_prompt_prefix(Styled::new("?").with_fg(faint))
        .with_highlighted_option_prefix(Styled::new("▶").with_fg(orange))
        .with_selected_option(Some(StyleSheet::new().with_attr(Attributes::BOLD)))
        .with_answer(StyleSheet::new().with_attr(Attributes::ITALIC))
        .with_help_message(StyleSheet::new().with_fg(blue))
}
