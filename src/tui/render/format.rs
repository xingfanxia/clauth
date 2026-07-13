//! Shared formatters and style helpers used across multiple screens.
//! Screen-only helpers stay in their own modules.

use ratatui::style::{Color, Style};
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
    // Peek instead of a plain `for`: the loop head must not consume the char
    // AFTER the window, or a value exactly one char over `width` reads as
    // exhausted and gets silently cropped with no ellipsis
    // ("Max 20x" at 6 → "Max 20" instead of "Max 2…").
    let mut iter = value.chars().peekable();
    while count < width {
        let Some(ch) = iter.next() else { break };
        content.push(ch);
        count += 1;
    }
    if width > 0 && iter.peek().is_some() {
        content.pop();
        content.push('…');
        count = width;
    }
    let pad = " ".repeat(width - count);
    (content, pad)
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_format.rs"]
mod tests;

/// Fetch-state cue for a profile's bracketed bars: amber while the row serves
/// last-known numbers (cached / rate-limited both do — matches the amber
/// `rate limited` badge on the usage detail line), red when the fetch failed,
/// `None` when live.
pub(super) fn fetch_cue_color(profile: &Profile) -> Option<Color> {
    if !profile.is_oauth() {
        return None;
    }
    match profile.fetch_status {
        Some(FetchStatus::Cached | FetchStatus::RateLimited) => Some(theme::warning_color()),
        Some(FetchStatus::Failed) => Some(theme::danger_color()),
        _ => None,
    }
}

/// The cue color when one is live, else the resting style (dim brackets,
/// faint no-data dash).
pub(super) fn cue_style(cue: Option<Color>, resting: Style) -> Style {
    cue.map(|c| Style::default().fg(c)).unwrap_or(resting)
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
    if profile.credentials.is_some() {
        Style::default().fg(theme::accent_2_color())
    } else {
        theme::dim()
    }
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
    let bracket = theme::dim();
    let pct = window.utilization.clamp(0.0, 100.0);
    let color = theme::util_color(pct);
    let style = Style::default().fg(color);

    if include_bar && width >= 26 {
        // [██████░░░░] XX% (reset)
        let mut spans = vec![
            Span::styled("[", bracket),
            Span::styled(bar_string_with_cells(pct, 10), style),
            Span::styled("]", bracket),
            Span::styled(format!(" {:>3.0}%", pct), style),
        ];
        if let Some(r) = format_reset(window) {
            spans.push(Span::styled(format!(" ({r})"), theme::faint()));
        }
        spans
    } else if include_bar && width >= 17 {
        // [██████░░░░] XX%
        vec![
            Span::styled("[", bracket),
            Span::styled(bar_string_with_cells(pct, 10), style),
            Span::styled("]", bracket),
            Span::styled(format!(" {:>3.0}%", pct), style),
        ]
    } else if include_bar && width >= 12 {
        // [███░░░] XX%  — bar shrinks to fit
        let bar_cells = width.saturating_sub(7).clamp(3, 7);
        vec![
            Span::styled("[", bracket),
            Span::styled(bar_string_with_cells(pct, bar_cells), style),
            Span::styled("]", bracket),
            Span::styled(format!(" {:>3.0}%", pct), style),
        ]
    } else {
        vec![Span::styled(format!("{pct:>3.0}%"), style)]
    }
}

/// Seconds until the window resets (may be negative if the stamp is overdue).
pub(super) fn reset_in_secs(window: &UsageWindow) -> Option<i64> {
    let resets_at = window.resets_at.as_deref()?;
    let target = iso_to_epoch_secs(resets_at)?;
    Some(target - now_epoch_secs())
}

pub(super) fn format_reset(window: &UsageWindow) -> Option<String> {
    reset_in_secs(window).map(crate::usage::humanize_duration)
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
        ProfileActivity::Refreshing | ProfileActivity::Switching => theme::info(),
        ProfileActivity::Queued | ProfileActivity::Idle => theme::faint(),
    }
}

pub(super) fn activity_verb(activity: ProfileActivity) -> &'static str {
    match activity {
        ProfileActivity::Fetching => "fetching",
        ProfileActivity::Refreshing => "refreshing",
        ProfileActivity::Switching => "switching",
        ProfileActivity::Queued => "queued",
        ProfileActivity::Idle => "idle",
    }
}
