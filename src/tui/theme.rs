//! cloudy-ui palette and shared style helpers.
//!
//! Catppuccin Mocha is the only palette. Two capability tiers select the color
//! depth: `full` uses 24-bit RGB; `compatible` uses the nearest xterm-256 index.
//! Every color in the TUI comes from this module — raw `Color::Rgb` or raw index
//! values anywhere else are a bug.
//!
//! # Initialization
//!
//! Call [`init`] exactly once before the TUI starts. The tier is locked for the
//! process lifetime — renders read it via the accessor fns below.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

// ── Tier ──────────────────────────────────────────────────────────────────────

/// Color-depth capability tier. `full` = 24-bit RGB; `compatible` = xterm-256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    /// 24-bit truecolor. Requires `$COLORTERM=truecolor|24bit` or an explicit
    /// CLI / config override.
    Full,
    /// Nearest xterm-256 palette index. Safe on any xterm-compatible terminal.
    Compatible,
}

/// Process-global tier, set once at startup by [`init`].
static TIER: OnceLock<Tier> = OnceLock::new();

/// Detect the tier from `$COLORTERM` per the cloudy-tui contract:
/// `truecolor` or `24bit` → [`Tier::Full`]; anything else → [`Tier::Compatible`].
pub(crate) fn detect() -> Tier {
    match std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "truecolor" | "24bit" => Tier::Full,
        _ => Tier::Compatible,
    }
}

/// Lock the process tier. Call exactly once before the first render.
/// Precedence (highest first): explicit override → auto-detect.
/// A second call is silently ignored (the first caller wins).
pub(crate) fn init(override_tier: Option<Tier>) {
    let tier = override_tier.unwrap_or_else(detect);
    let _ = TIER.set(tier);
}

/// Return the active tier. Falls back to auto-detect if [`init`] was not called.
#[inline]
pub(crate) fn tier() -> Tier {
    *TIER.get_or_init(detect)
}

// ── Palette tables ────────────────────────────────────────────────────────────
//
// Each row: (full: Color::Rgb, compatible: Color::Indexed(xterm-256))
// The xterm-256 index is the nearest match per the cloudy-tui SKILL.md table.

#[inline]
fn pick(full: Color, compatible: Color) -> Color {
    match tier() {
        Tier::Full => full,
        Tier::Compatible => compatible,
    }
}

// ── Surfaces ──────────────────────────────────────────────────────────────────
#[inline]
pub(crate) fn bg() -> Color {
    pick(Color::Rgb(30, 30, 46), Color::Indexed(235))
}
#[inline]
pub(crate) fn bg_sunken() -> Color {
    pick(Color::Rgb(17, 17, 27), Color::Indexed(233))
}
#[inline]
pub(crate) fn bg_hover() -> Color {
    pick(Color::Rgb(40, 40, 56), Color::Indexed(236))
}

// ── Lines ─────────────────────────────────────────────────────────────────────
#[inline]
pub(crate) fn line_color() -> Color {
    pick(Color::Rgb(49, 50, 68), Color::Indexed(238))
}
#[inline]
pub(crate) fn line_strong_color() -> Color {
    pick(Color::Rgb(69, 71, 90), Color::Indexed(240))
}

// ── Text ──────────────────────────────────────────────────────────────────────
#[inline]
pub(crate) fn text_color() -> Color {
    pick(Color::Rgb(205, 214, 244), Color::Indexed(189))
}
#[inline]
pub(crate) fn text_dim_color() -> Color {
    pick(Color::Rgb(166, 173, 200), Color::Indexed(145))
}
#[inline]
pub(crate) fn text_faint_color() -> Color {
    pick(Color::Rgb(127, 132, 156), Color::Indexed(102))
}

// ── Accents ───────────────────────────────────────────────────────────────────
/// Sapphire primary — the cool accent that carries the UI.
#[inline]
pub(crate) fn accent_color() -> Color {
    pick(Color::Rgb(67, 171, 229), Color::Indexed(75))
}
/// Claude orange — the warm secondary; cloudy-ui rule "once per screen max".
#[inline]
pub(crate) fn accent_2_color() -> Color {
    pick(Color::Rgb(217, 119, 87), Color::Indexed(173))
}

// ── Semantic ──────────────────────────────────────────────────────────────────
#[inline]
pub(crate) fn success_color() -> Color {
    pick(Color::Rgb(166, 227, 161), Color::Indexed(151))
}
#[inline]
pub(crate) fn warning_color() -> Color {
    pick(Color::Rgb(249, 226, 175), Color::Indexed(223))
}
#[inline]
pub(crate) fn danger_color() -> Color {
    pick(Color::Rgb(243, 139, 168), Color::Indexed(211))
}
#[inline]
pub(crate) fn info_color() -> Color {
    pick(Color::Rgb(116, 199, 236), Color::Indexed(117))
}

// ── Banner background tints ───────────────────────────────────────────────────
/// DANGER wash blended into BG — banner background for critical conditions.
#[inline]
pub(crate) fn bg_danger_color() -> Color {
    pick(Color::Rgb(75, 35, 44), Color::Indexed(52))
}

// ── Toggle glyphs (tier-sensitive) ────────────────────────────────────────────

/// Toggle switch in the **on** state.
/// `full`: `─●`  `compatible`: `[on]`
pub(crate) fn toggle_on() -> &'static str {
    match tier() {
        Tier::Full => "─●",
        Tier::Compatible => "[on]",
    }
}

/// Toggle switch in the **off** state.
/// `full`: `○─`  `compatible`: `[off]`
pub(crate) fn toggle_off() -> &'static str {
    match tier() {
        Tier::Full => "○─",
        Tier::Compatible => "[off]",
    }
}

// ── Style helpers ─────────────────────────────────────────────────────────────

pub(crate) fn base() -> Style {
    Style::default().fg(text_color()).bg(bg())
}

/// Plain body text — foreground only.
pub(crate) fn body() -> Style {
    Style::default().fg(text_color())
}

/// Stronger line color — empty-gauge track and structural fills above `line_color()`.
pub(crate) fn line_strong() -> Style {
    Style::default().fg(line_strong_color())
}

pub(crate) fn dim() -> Style {
    Style::default().fg(text_dim_color())
}

pub(crate) fn faint() -> Style {
    Style::default().fg(text_faint_color())
}

/// Eyebrow label — bold + dim per cloudy-ui's CLI mapping.
pub(crate) fn label() -> Style {
    Style::default()
        .fg(text_dim_color())
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn accent() -> Style {
    Style::default().fg(accent_color())
}

pub(crate) fn orange() -> Style {
    Style::default().fg(accent_2_color())
}

pub(crate) fn warning() -> Style {
    Style::default().fg(warning_color())
}

pub(crate) fn danger() -> Style {
    Style::default().fg(danger_color())
}

/// Background for the selected list row.
pub(crate) fn selected_row() -> Style {
    Style::default().bg(bg_hover())
}

/// Utilization color: dim <60%, warning 60–80%, danger >80%.
pub(crate) fn util_color(pct: f64) -> Color {
    let pct = pct.clamp(0.0, 100.0);
    if pct >= 80.0 {
        danger_color()
    } else if pct >= 60.0 {
        warning_color()
    } else {
        text_dim_color()
    }
}

/// Sapphire info accent; spinner color for refresh ops.
pub(crate) fn info() -> Style {
    Style::default().fg(info_color())
}

/// Catppuccin green — success tint; spinner color for auto-start.
pub(crate) fn success() -> Style {
    Style::default().fg(success_color())
}
