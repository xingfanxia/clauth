//! Config tab — account picker on the left, that account's settings on the
//! right. The settings rows mirror [`config_actions`]; ⏎ on the left pane drops
//! focus into the list, ⏎ on a row applies it (endpoint/rename open a modal,
//! auto-start and chain toggle in place, delete confirms).

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, ConfigAction, ConfigFocus, config_actions};
use super::super::theme;
use super::panes::{SELECTOR_WIDTH, draw_profile_selector, section_box};
use crate::fallback::threshold_for;
use crate::profile::{AppConfig, Profile};

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SELECTOR_WIDTH), Constraint::Min(20)])
        .split(area);

    let profiles_focused = app.config_focus == ConfigFocus::Profiles;
    draw_profile_selector(frame, cols[0], app, app.config_cursor, profiles_focused);
    draw_settings(frame, cols[1], app);
}

/// Owned snapshot of one profile's settings, taken under a single short-lived
/// `config` guard. Decoupling the read from the render lets us call
/// `config_actions` (which re-locks `config`) afterwards without nesting the
/// non-reentrant mutex — a nested lock would hang the whole UI thread.
struct ProfileSnap {
    name: String,
    endpoint: String,
    auto_start: bool,
    chain: (String, Style),
}

fn draw_settings(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let actions_focused = app.config_focus == ConfigFocus::Actions;

    let snap = {
        let cfg = app.config();
        let idx = app.config_cursor.min(cfg.profiles.len().saturating_sub(1));
        cfg.profiles.get(idx).map(|p| ProfileSnap {
            name: p.name.clone(),
            endpoint: endpoint_value(p),
            auto_start: p.auto_start,
            chain: chain_value(&cfg, p),
        })
    };

    // The settings box accents its border when the actions pane owns the cursor.
    let title = snap.as_ref().map(|s| s.name.as_str()).unwrap_or("settings");
    let block = section_box(title, actions_focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(snap) = snap else {
        let hint = Paragraph::new(Line::from(Span::styled(
            "no accounts yet — press n to create one",
            theme::muted(),
        )))
        .style(theme::base());
        frame.render_widget(hint, inner);
        return;
    };

    // No `config` guard is held here, so this re-lock is safe.
    let actions = config_actions(app, &snap.name);
    let cursor = app
        .config_action_cursor
        .min(actions.len().saturating_sub(1));

    let note = if actions_focused {
        "↑↓ choose · ⏎ apply · ⎋ back"
    } else {
        "⏎ to edit"
    };
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(note, theme::faint())),
        Line::from(""),
    ];
    for (i, action) in actions.iter().enumerate() {
        let selected = actions_focused && i == cursor;
        lines.push(action_row(*action, &snap, selected));
    }

    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

fn action_row(action: ConfigAction, snap: &ProfileSnap, selected: bool) -> Line<'static> {
    let arrow = if selected {
        Span::styled("▸ ", theme::accent())
    } else {
        Span::raw("  ")
    };
    match action {
        ConfigAction::Edit => kv_row(arrow, "endpoint", snap.endpoint.clone(), theme::muted()),
        ConfigAction::Rename => kv_row(arrow, "name", snap.name.clone(), theme::muted()),
        ConfigAction::ToggleAutoStart => {
            let (value, style) = if snap.auto_start {
                ("on".to_string(), theme::accent())
            } else {
                ("off".to_string(), theme::faint())
            };
            kv_row(arrow, "auto-start", value, style)
        }
        ConfigAction::ToggleChain => kv_row(arrow, "fallback", snap.chain.0.clone(), snap.chain.1),
        ConfigAction::Delete => {
            Line::from(vec![arrow, Span::styled("delete profile", theme::danger())])
        }
    }
}

/// Padded key + styled value, aligned under a 2-char arrow gutter.
fn kv_row(arrow: Span<'static>, key: &str, value: String, value_style: Style) -> Line<'static> {
    const KEY_W: usize = 11;
    let pad = KEY_W.saturating_sub(key.chars().count()).max(1);
    Line::from(vec![
        arrow,
        Span::styled(format!("{key}{}", " ".repeat(pad)), theme::faint()),
        Span::styled(value, value_style),
    ])
}

fn endpoint_value(profile: &Profile) -> String {
    if !profile.is_oauth() {
        let url = profile.base_url.clone().unwrap_or_default();
        if profile.api_key.is_some() {
            return format!("{url}  · key set");
        }
        return url;
    }
    "oauth (claude.ai)".to_string()
}

fn chain_value(cfg: &AppConfig, profile: &Profile) -> (String, Style) {
    match cfg
        .state
        .fallback_chain
        .iter()
        .position(|n| n == &profile.name)
    {
        Some(pos) => (
            format!("#{} @ {:.0}%", pos + 1, threshold_for(profile)),
            theme::muted(),
        ),
        None => ("not in chain".to_string(), theme::faint()),
    }
}
