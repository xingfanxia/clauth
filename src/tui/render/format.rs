//! Shared formatters and style helpers used across multiple screens.
//!
//! Anything that maps profile/usage state to a `Span`, `Style`, or display
//! string and is referenced by more than one screen lives here. Screen-only
//! helpers (e.g. fallback flow widget, overview column math) stay on their
//! screen module.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use super::super::theme;
use crate::format::endpoint_label;
use crate::profile::Profile;
use crate::usage::{FetchStatus, UsageWindow, iso_to_epoch_secs, now_epoch_secs};

pub(super) fn fixed(value: &str, width: usize) -> String {
    let (mut content, pad) = fixed_split(value, width);
    content.push_str(&pad);
    content
}

/// Split into the visible content (possibly truncated with `…`) and the trailing
/// padding. Callers that style only the content (e.g. underlined names) keep
/// the padding plain so decorations don't bleed past the text.
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
    let (text, style) = window_summary_parts(window, width, include_bar);
    Span::styled(fixed(&text, width), style)
}

/// Same content as [`window_summary_span`] but returns the raw text and its
/// style so the caller can append decorations (e.g. an auto-start marker) and
/// pad to the column width itself.
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
