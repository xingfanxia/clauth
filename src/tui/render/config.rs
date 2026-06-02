//! Config tab — account picker (plus a trailing `+ new` row) on the left, the
//! selected account's settings on the right. Editing happens inline in the
//! right pane: ⏎ on the left drops focus into the detail rows, ⏎ on a text row
//! opens an inline caret, ⏎ on a toggle flips it, and `+ new` turns the right
//! pane into a create form. No popups.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use super::super::app::{App, ConfigFocus, ConfigRow, InputState, config_rows};
use super::super::theme;
use super::panes::{SELECTOR_WIDTH, section_box};

/// Padded key column width for the detail rows.
const KEY_W: usize = 11;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SELECTOR_WIDTH), Constraint::Min(20)])
        .split(area);

    let profiles_focused = app.config_focus == ConfigFocus::Profiles;
    draw_selector(frame, cols[0], app, profiles_focused);
    draw_settings(frame, cols[1], app);
}

/// Account picker with a trailing `+ new` row. The cursor lands on `+ new`
/// when `config_cursor` equals the account count.
fn draw_selector(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    let block = section_box("accounts", focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let cfg = app.config();
    let count = cfg.profiles.len();
    let mut items: Vec<ListItem<'_>> = cfg
        .profiles
        .iter()
        .map(|p| {
            let active = cfg.is_active(&p.name);
            let dot = if active {
                Span::styled("◆ ", theme::orange())
            } else {
                Span::styled("◇ ", theme::faint())
            };
            let name_style = if active {
                Style::default().fg(theme::ACCENT_2)
            } else {
                Style::default().fg(theme::TEXT)
            };
            ListItem::new(Line::from(vec![
                dot,
                Span::styled(p.name.clone(), name_style),
            ]))
        })
        .collect();
    items.push(ListItem::new(Line::from(Span::styled(
        "+ new",
        theme::accent(),
    ))));

    let highlight = if focused {
        theme::selected_row().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::TEXT_DIM)
    };
    let list = List::new(items)
        .style(theme::base())
        .highlight_style(highlight);
    let mut state = ListState::default();
    state.select(Some(app.config_cursor.min(count)));
    frame.render_stateful_widget(list, inner, &mut state);
}

/// Owned snapshot of one selection, taken under a single short-lived `config`
/// guard. Decoupling the read from the render lets us call `config_rows`
/// (which re-locks `config`) afterwards without nesting the non-reentrant mutex.
struct Snap {
    title: String,
    is_new: bool,
    name: String,
    base_url: String,
    api_key: String,
    auto_start: bool,
}

fn draw_settings(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let actions_focused = app.config_focus == ConfigFocus::Actions;

    let snap = {
        let cfg = app.config();
        if app.config_cursor >= cfg.profiles.len() {
            Snap {
                title: "+ new account".to_string(),
                is_new: true,
                name: String::new(),
                base_url: String::new(),
                api_key: String::new(),
                auto_start: false,
            }
        } else {
            match cfg.profiles.get(app.config_cursor) {
                Some(p) => Snap {
                    title: p.name.clone(),
                    is_new: false,
                    name: p.name.clone(),
                    base_url: p.base_url.clone().unwrap_or_default(),
                    api_key: p.api_key.clone().unwrap_or_default(),
                    auto_start: p.auto_start,
                },
                None => Snap {
                    title: "settings".to_string(),
                    is_new: false,
                    name: String::new(),
                    base_url: String::new(),
                    api_key: String::new(),
                    auto_start: false,
                },
            }
        }
    };

    let block = section_box(&snap.title, actions_focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // No `config` guard held here, so this re-lock is safe.
    let rows = config_rows(app);
    let cursor = app.config_action_cursor.min(rows.len().saturating_sub(1));

    // Effective text buffers: the live draft while editing, else the snapshot.
    let draft = app.config_draft.as_ref();
    let (name_in, base_in, key_in) = match draft {
        Some(d) => (d.name.clone(), d.base_url.clone(), d.api_key.clone()),
        None => (
            InputState::new(&snap.name),
            InputState::new(&snap.base_url),
            InputState::new(&snap.api_key),
        ),
    };
    let editing = draft.and_then(|d| d.active);
    let armed_delete = draft.map(|d| d.armed_delete).unwrap_or(false);

    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            hint(actions_focused, editing.is_some(), &snap),
            theme::faint(),
        )),
        Line::from(""),
    ];
    for (i, row) in rows.iter().enumerate() {
        let selected = actions_focused && i == cursor;
        lines.push(detail_row(
            *row,
            selected,
            editing == Some(*row),
            armed_delete,
            &snap,
            &name_in,
            &base_in,
            &key_in,
        ));
    }

    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

fn hint(actions_focused: bool, editing: bool, snap: &Snap) -> &'static str {
    if !actions_focused {
        return if snap.is_new {
            "⏎ to create a new account"
        } else {
            "⏎ to configure this account"
        };
    }
    if editing {
        "type · ⏎ save · ⎋ cancel"
    } else if snap.is_new {
        "↑↓ choose · ⏎ edit / create · ⎋ back"
    } else {
        "↑↓ choose · ⏎ edit / toggle · ⎋ back"
    }
}

#[allow(clippy::too_many_arguments)]
fn detail_row(
    row: ConfigRow,
    selected: bool,
    editing: bool,
    armed_delete: bool,
    snap: &Snap,
    name_in: &InputState,
    base_in: &InputState,
    key_in: &InputState,
) -> Line<'static> {
    let arrow = if selected {
        Span::styled("▸ ", theme::accent())
    } else {
        Span::raw("  ")
    };
    match row {
        ConfigRow::Name => kv_field(arrow, "name", name_in, editing),
        ConfigRow::BaseUrl => kv_field(arrow, "base url", base_in, editing),
        ConfigRow::ApiKey => kv_field(arrow, "api key", key_in, editing),
        ConfigRow::AutoStart => {
            let (value, style) = if snap.auto_start {
                ("on".to_string(), theme::accent())
            } else {
                ("off".to_string(), theme::faint())
            };
            kv_static(arrow, "auto-start", value, style)
        }
        ConfigRow::Delete => {
            let label = if armed_delete {
                "delete account — ⏎ again to confirm".to_string()
            } else {
                "delete account".to_string()
            };
            Line::from(vec![arrow, Span::styled(label, theme::danger())])
        }
        ConfigRow::Create => {
            Line::from(vec![arrow, Span::styled("create account", theme::accent())])
        }
    }
}

/// A text-field row: padded key, then either the value or an inline caret.
fn kv_field(arrow: Span<'static>, key: &str, input: &InputState, editing: bool) -> Line<'static> {
    let pad = KEY_W.saturating_sub(key.chars().count()).max(1);
    let mut spans = vec![
        arrow,
        Span::styled(format!("{key}{}", " ".repeat(pad)), theme::faint()),
    ];
    spans.extend(value_spans(input, editing));
    Line::from(spans)
}

/// A non-editable row: padded key + a styled value (toggles, fallback, …).
fn kv_static(arrow: Span<'static>, key: &str, value: String, value_style: Style) -> Line<'static> {
    let pad = KEY_W.saturating_sub(key.chars().count()).max(1);
    Line::from(vec![
        arrow,
        Span::styled(format!("{key}{}", " ".repeat(pad)), theme::faint()),
        Span::styled(value, value_style),
    ])
}

/// Value rendering for a text field. Editing shows a block caret over a sunken
/// input strip; otherwise the plain value (or a faint placeholder when empty).
fn value_spans(input: &InputState, editing: bool) -> Vec<Span<'static>> {
    if !editing {
        if input.value.is_empty() {
            return vec![Span::styled("—", theme::faint())];
        }
        return vec![Span::styled(input.value.clone(), theme::muted())];
    }
    let (head, tail) = input.value.split_at(input.cursor.min(input.value.len()));
    let caret_style = Style::default()
        .fg(theme::TEXT)
        .bg(theme::ACCENT)
        .add_modifier(Modifier::BOLD);
    let body = Style::default().fg(theme::TEXT).bg(theme::BG_SUNKEN);
    let mut tail_iter = tail.chars();
    let caret = tail_iter.next().unwrap_or(' ').to_string();
    let after: String = tail_iter.collect();
    vec![
        Span::styled(head.to_string(), body),
        Span::styled(caret, caret_style),
        Span::styled(after, body),
    ]
}
