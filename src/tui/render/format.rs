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

/// Overview accounts rows only: `[███░░░]` with dim brackets around the bar.
/// Brackets render in `dim`; filled/empty cells keep their semantic util color.
/// All other bar sites use [`bar_string_with_cells`] directly (no brackets).
pub(super) fn window_summary_spans_bracketed(
    window: Option<&UsageWindow>,
    width: usize,
    include_bar: bool,
) -> Vec<Span<'static>> {
    let Some(window) = window else {
        return vec![Span::styled("—".to_string(), theme::faint())];
    };
    let pct = window.utilization.clamp(0.0, 100.0);
    let color = theme::util_color(pct);
    let style = Style::default().fg(color);

    if include_bar && width >= 26 {
        // [██████░░░░] XX% (reset)
        let reset = format_reset(window)
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        vec![
            Span::styled("[", theme::dim()),
            Span::styled(bar_string_with_cells(pct, 10), style),
            Span::styled("]", theme::dim()),
            Span::styled(format!(" {:>2.0}%{reset}", pct), style),
        ]
    } else if include_bar && width >= 17 {
        // [██████░░░░] XX%
        vec![
            Span::styled("[", theme::dim()),
            Span::styled(bar_string_with_cells(pct, 10), style),
            Span::styled("]", theme::dim()),
            Span::styled(format!(" {:>2.0}%", pct), style),
        ]
    } else if include_bar && width >= 12 {
        // [███░░░] XX%  — bar shrinks to fit
        let bar_cells = width.saturating_sub(7).clamp(3, 7);
        vec![
            Span::styled("[", theme::dim()),
            Span::styled(bar_string_with_cells(pct, bar_cells), style),
            Span::styled("]", theme::dim()),
            Span::styled(format!(" {:>2.0}%", pct), style),
        ]
    } else {
        vec![Span::styled(format!("{pct:>2.0}%"), style)]
    }
}

pub(super) fn format_reset(window: &UsageWindow) -> Option<String> {
    let resets_at = window.resets_at.as_deref()?;
    let target = iso_to_epoch_secs(resets_at)?;
    let secs = target - now_epoch_secs();
    Some(crate::usage::humanize_duration(secs))
}

/// Relative age of an epoch-ms timestamp per the cloudy-tui Time-formatting
/// contract: single largest unit under 30 days (`4m ago`, `2h ago`, `3d ago`,
/// `2w ago`); the absolute ISO date (`2026-04-12`) at 30 days and beyond.
/// `< 1 minute` reads `just now`.
pub(super) fn relative_age(epoch_ms: u64) -> String {
    let now = crate::usage::now_ms();
    let age_secs = (now.saturating_sub(epoch_ms) / 1000) as i64;
    let mins = age_secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    let weeks = days / 7;
    if age_secs < 60 {
        "just now".to_string()
    } else if days < 1 {
        if hours < 1 {
            format!("{mins}m ago")
        } else {
            format!("{hours}h ago")
        }
    } else if days < 30 {
        if weeks < 1 {
            format!("{days}d ago")
        } else {
            format!("{weeks}w ago")
        }
    } else {
        let iso = crate::usage::epoch_secs_to_iso((epoch_ms / 1000) as i64);
        iso.split('T').next().unwrap_or(&iso).to_string()
    }
}

/// Lowercase clock label for an epoch-ms timestamp: `jun 5, 18:27`. A comma sits
/// directly after the day, no space before it. With `utc = true` appends ` utc`
/// (used only on the detail `started` row). All times are UTC.
pub(super) fn clock_label(epoch_ms: u64, utc: bool) -> String {
    // `epoch_secs_to_iso` → `YYYY-MM-DDTHH:MM:SS+00:00`.
    let iso = crate::usage::epoch_secs_to_iso((epoch_ms / 1000) as i64);
    let bytes = iso.as_bytes();
    // Defensive: a malformed ISO string degrades to the raw value.
    if bytes.len() < 16 || bytes[10] != b'T' {
        return iso;
    }
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let month: usize = iso[5..7].parse().unwrap_or(1);
    let mon = MONTHS
        .get(month.saturating_sub(1))
        .copied()
        .unwrap_or("jan");
    // Day without a leading zero.
    let day: u32 = iso[8..10].parse().unwrap_or(0);
    let hm = &iso[11..16];
    let suffix = if utc { " utc" } else { "" };
    format!("{mon} {day}, {hm}{suffix}")
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
        ProfileActivity::Idle => theme::faint(),
    }
}

pub(super) fn activity_verb(activity: ProfileActivity) -> &'static str {
    match activity {
        ProfileActivity::Fetching => "fetching",
        ProfileActivity::Refreshing => "refreshing",
        ProfileActivity::Idle => "idle",
    }
}
