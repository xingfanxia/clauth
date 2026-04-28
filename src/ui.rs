use inquire::ui::{Attributes, Color, RenderConfig, StyleSheet, Styled};

use crate::profile::Profile;

// ── Terminal palette (cloudy-ui CLI) ──────────────────────────────────────────

pub(crate) const C_RESET:   &str = "\x1b[0m";
pub(crate) const C_BOLD:    &str = "\x1b[1m";
// Targeted resets — used so inquire's "selected = bold" wrapper does not
// either leak through the whole label or get killed by an early full reset.
pub(crate) const C_NOBOLD:  &str = "\x1b[22m"; // normal intensity, keeps current color
pub(crate) const C_FG_OFF:  &str = "\x1b[39m"; // default foreground, keeps current attrs
pub(crate) const C_ACCENT:  &str = "\x1b[38;2;67;171;229m";   // sapphire
pub(crate) const C_ORANGE:  &str = "\x1b[38;2;217;119;87m";   // claude orange
pub(crate) const C_SUCCESS: &str = "\x1b[38;2;166;227;161m";
pub(crate) const C_WARNING: &str = "\x1b[38;2;249;226;175m";
pub(crate) const C_DANGER:  &str = "\x1b[38;2;243;139;168m";
pub(crate) const C_DIM:     &str = "\x1b[38;2;166;173;200m";
pub(crate) const C_FAINT:   &str = "\x1b[38;2;127;132;156m";

pub(crate) const ENDPOINT_DEFAULT: &str = "Claude Pro / OAuth";

pub(crate) fn format_profile_entry(profile: &Profile, is_active: bool, name_width: usize) -> String {
    let endpoint = profile.base_url.as_deref().unwrap_or(ENDPOINT_DEFAULT);
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
        format!("{C_ACCENT}● {name:<name_width$}{C_NOBOLD}  {C_DIM}{endpoint}{C_RESET}{key_hint}{cred_warn}")
    } else {
        format!("  {name:<name_width$}{C_NOBOLD}  {C_DIM}{endpoint}{C_RESET}{key_hint}{cred_warn}")
    }
}

pub(crate) fn format_submenu_title(profile: &Profile) -> String {
    let url = profile.base_url.as_deref().unwrap_or(ENDPOINT_DEFAULT);
    let (cred_color, creds) = if profile.credentials.is_some() {
        (C_SUCCESS, "credentials saved")
    } else {
        (C_WARNING, "no credentials")
    };
    let name = &profile.name;
    format!(
        "{C_BOLD}{name}{C_RESET}{C_FAINT} · {C_RESET}{C_DIM}{url}{C_FAINT} · {C_RESET}{cred_color}{creds}{C_RESET}"
    )
}

pub(crate) fn build_render_config() -> RenderConfig<'static> {
    let orange = Color::Rgb { r: 217, g: 119, b: 87 };
    let blue   = Color::Rgb { r: 67,  g: 171, b: 229 };
    let faint  = Color::Rgb { r: 127, g: 132, b: 156 };

    RenderConfig::default()
        .with_prompt_prefix(Styled::new("?").with_fg(blue))
        .with_answered_prompt_prefix(Styled::new("?").with_fg(faint))
        .with_highlighted_option_prefix(Styled::new("▶").with_fg(orange))
        .with_selected_option(Some(StyleSheet::new().with_attr(Attributes::BOLD)))
        .with_answer(StyleSheet::new().with_fg(blue))
        .with_help_message(StyleSheet::new().with_fg(blue))
}
