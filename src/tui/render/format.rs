//! Shared formatters and style helpers used across multiple screens.
//! Screen-only helpers stay in their own modules.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use super::super::theme;
use crate::format::endpoint_label;
use crate::profile::Profile;
use crate::usage::{FetchStatus, ProfileActivity, UsageWindow, iso_to_epoch_secs, now_epoch_secs};

pub(super) fn fixed(value: &str, width: usize) -> String {
    let (mut content, pad) = fixed_split(value, width);
    content.push_str(&pad);
    content
}

/// Truncates with `…` and returns `(content, padding)` separately so callers
/// can style only the content without decorations bleeding past the text.
pub(super) fn fixed_split(value: &str, width: usize) -> (String, String) {
    let mut content = String::with_capacity(width);
    let mut count = 0;
    let mut iter = value.chars();
    for ch in iter.by_ref() {
        if count >= width {
            break;
        }
        content.push(ch);
        count += 1;
    }
    if width > 0 && iter.next().is_some() {
        content.pop();
        content.push('…');
        count = width;
    }
    let pad = " ".repeat(width - count);
    (content, pad)
}

pub(super) fn name_style(profile: &Profile) -> Style {
    let base = Style::default().fg(theme::text_color());
    if !profile.is_oauth() {
        return base;
    }
    match profile.fetch_status {
        Some(FetchStatus::Cached) => base
            .underline_color(theme::warning_color())
            .add_modifier(Modifier::UNDERLINED),
        Some(FetchStatus::Failed) => base
            .underline_color(theme::danger_color())
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

pub(super) fn account_type_style(_profile: &Profile) -> Style {
    theme::dim()
}

pub(super) fn bar_string_with_cells(pct: f64, cells: usize) -> String {
    let pct = pct.clamp(0.0, 100.0);
    let filled = ((pct / 100.0) * cells as f64).round() as usize;
    let filled = filled.min(cells);
    format!("{}{}", "█".repeat(filled), "░".repeat(cells - filled))
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
    let (text, style) = window_summary_parts(window, width, include_bar);
    Span::styled(fixed(&text, width), style)
}

/// Like [`window_summary_span`] but returns `(text, style)` so the caller can
/// append decorations and pad to the column width itself.
pub(super) fn window_summary_parts(
    window: Option<&UsageWindow>,
    width: usize,
    include_bar: bool,
) -> (String, Style) {
    let Some(window) = window else {
        return ("—".to_string(), theme::faint());
    };
    let pct = window.utilization.clamp(0.0, 100.0);
    let color = theme::util_color(pct);
    let text = if include_bar && width >= 26 {
        let reset = format_reset(window)
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        format!("{} {:>2.0}%{reset}", bar_string_with_cells(pct, 10), pct)
    } else if include_bar && width >= 17 {
        format!("{} {:>2.0}%", bar_string_with_cells(pct, 10), pct)
    } else if include_bar && width >= 12 {
        let bar_cells = width.saturating_sub(5).clamp(3, 7);
        format!("{}{:>2.0}", bar_string_with_cells(pct, bar_cells), pct)
    } else {
        format!("{pct:>2.0}%")
    };
    (text, Style::default().fg(color))
}

/// Green/yellow/red headroom against a member's threshold (crossing = rotate).
pub(super) fn health_color(pct: f64, threshold: f64) -> ratatui::style::Color {
    if pct >= threshold {
        theme::danger_color()
    } else if pct >= threshold * 0.8 {
        theme::warning_color()
    } else {
        theme::success_color()
    }
}

use crate::spinner::SPINNER_FRAMES;

pub(super) fn spinner_frame(tick: u64) -> &'static str {
    SPINNER_FRAMES[(tick as usize) % SPINNER_FRAMES.len()]
}

/// Distinct tint per activity so states read differently at a glance.
pub(super) fn spinner_style(activity: ProfileActivity) -> Style {
    match activity {
        ProfileActivity::Fetching => theme::accent(),
        ProfileActivity::Refreshing => theme::info(),
        ProfileActivity::Switching => theme::orange(),
        ProfileActivity::Starting => theme::warning(),
        ProfileActivity::AutoStarting => theme::success(),
        ProfileActivity::Idle => theme::faint(),
    }
}

pub(super) fn activity_verb(activity: ProfileActivity) -> &'static str {
    match activity {
        ProfileActivity::Fetching => "fetching",
        ProfileActivity::Refreshing => "refreshing",
        ProfileActivity::Switching => "switching",
        ProfileActivity::Starting => "starting",
        ProfileActivity::AutoStarting => "auto-starting",
        ProfileActivity::Idle => "idle",
    }
}
