//! cloudy-ui palette and shared style helpers.
//!
//! Catppuccin Mocha is the only theme — most terminals run dark, and the
//! design language reads strongest on the dark surface. Every color in the
//! TUI comes from this module; raw hex anywhere else is a bug.

use ratatui::style::{Color, Modifier, Style};

// ── Surfaces ──────────────────────────────────────────────────────────────────
pub(crate) const BG: Color = Color::Rgb(30, 30, 46);
pub(crate) const BG_RAISED: Color = Color::Rgb(24, 24, 37);
pub(crate) const BG_SUNKEN: Color = Color::Rgb(17, 17, 27);
pub(crate) const BG_HOVER: Color = Color::Rgb(40, 40, 56);

// ── Lines ─────────────────────────────────────────────────────────────────────
pub(crate) const LINE: Color = Color::Rgb(49, 50, 68);
pub(crate) const LINE_STRONG: Color = Color::Rgb(69, 71, 90);

// ── Text ──────────────────────────────────────────────────────────────────────
pub(crate) const TEXT: Color = Color::Rgb(205, 214, 244);
pub(crate) const TEXT_MUTED: Color = Color::Rgb(186, 194, 222);
pub(crate) const TEXT_DIM: Color = Color::Rgb(166, 173, 200);
pub(crate) const TEXT_FAINT: Color = Color::Rgb(127, 132, 156);

// ── Accents ───────────────────────────────────────────────────────────────────
/// Sapphire primary — the cool accent that carries the UI.
pub(crate) const ACCENT: Color = Color::Rgb(67, 171, 229);
/// Claude orange — the warm secondary; cloudy-ui rule "once per screen max".
pub(crate) const ACCENT_2: Color = Color::Rgb(217, 119, 87);

// ── Semantic ──────────────────────────────────────────────────────────────────
pub(crate) const SUCCESS: Color = Color::Rgb(166, 227, 161);
pub(crate) const WARNING: Color = Color::Rgb(249, 226, 175);
pub(crate) const DANGER: Color = Color::Rgb(243, 139, 168);
pub(crate) const INFO: Color = Color::Rgb(116, 199, 236);

// ── Style helpers ─────────────────────────────────────────────────────────────

pub(crate) fn base() -> Style {
    Style::default().fg(TEXT).bg(BG)
}

pub(crate) fn dim() -> Style {
    Style::default().fg(TEXT_DIM)
}

pub(crate) fn faint() -> Style {
    Style::default().fg(TEXT_FAINT)
}

pub(crate) fn muted() -> Style {
    Style::default().fg(TEXT_MUTED)
}

/// Uppercase tracked eyebrow label — 11px in web translates to bold + dim
/// in the terminal per cloudy-ui's CLI mapping.
pub(crate) fn label() -> Style {
    Style::default().fg(TEXT_DIM).add_modifier(Modifier::BOLD)
}

pub(crate) fn accent() -> Style {
    Style::default().fg(ACCENT)
}

pub(crate) fn orange() -> Style {
    Style::default().fg(ACCENT_2)
}

pub(crate) fn warning() -> Style {
    Style::default().fg(WARNING)
}

pub(crate) fn danger() -> Style {
    Style::default().fg(DANGER)
}

/// Background for the selected list row — cloudy-ui's `--accent-soft` plus an
/// accent left-bar is the canonical "active row" treatment.
pub(crate) fn selected_row() -> Style {
    Style::default().bg(BG_HOVER)
}

/// Maps a utilization percentage (0..=100) to the cloudy-ui semantic color
/// the bar fill and percentage label should wear. Mirrors the web rule:
/// dim under 60%, warning at 60–80%, danger past 80%.
pub(crate) fn util_color(pct: f64) -> Color {
    let pct = pct.clamp(0.0, 100.0);
    if pct >= 80.0 {
        DANGER
    } else if pct >= 60.0 {
        WARNING
    } else {
        TEXT_DIM
    }
}

/// Sapphire info accent. Used as the spinner color for refresh operations to
/// keep ACCENT (the primary sapphire) reserved for the active fetch spinner.
pub(crate) fn info() -> Style {
    Style::default().fg(INFO)
}

/// Catppuccin green — success / confirmation tint. Spinner color for the
/// auto-start kick path so a successful window arming reads green.
pub(crate) fn success() -> Style {
    Style::default().fg(SUCCESS)
}
