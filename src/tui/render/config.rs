//! Setup tab — account picker (plus a trailing `+ new` row) on the left, the
//! selected account's settings on the right. Editing happens inline in the
//! right pane: ⏎ on the left drops focus into the detail rows, ⏎ on a text row
//! opens an inline caret, ⏎ on a toggle flips it, and `+ new` turns the right
//! pane into a create form. No popups.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

// ── display-column helpers ────────────────────────────────────────────────────
// `InputState::cursor` is a byte offset; for screen-column math we need the
// char count of the text before the caret. All fields here are ASCII-only in
// practice, so `.chars().count()` equals display columns.

/// Number of display columns occupied by the text before the caret.
fn head_cols(input: &InputState) -> usize {
    input.value[..input.cursor.min(input.value.len())]
        .chars()
        .count()
}

use super::super::app::{App, ConfigFocus, ConfigRow, InputState, config_rows};
use super::super::theme;
use super::panes::{
    SELECTOR_WIDTH, active_dot, draw_selector_list, highlight_row, name_color, picker_row,
    section_box,
};

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

fn draw_selector(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    let cfg = app.config();
    let count = cfg.profiles.len();
    let sel = app.profile_cursor.min(count);
    draw_selector_list(frame, area, "accounts", focused, sel, |w| {
        let mut rows: Vec<_> = cfg
            .profiles
            .iter()
            .enumerate()
            .map(|(i, p)| {
                picker_row(
                    i == sel,
                    focused,
                    p.name.to_string(),
                    name_color(cfg.is_active(&p.name)),
                    w,
                )
            })
            .collect();
        rows.push(picker_row(
            count == sel,
            focused,
            "+ new".to_string(),
            theme::accent(),
            w,
        ));
        rows
    });
}

/// Snapshot taken under one short `config` guard, decoupled from render so
/// `config_rows` can re-lock without nesting the non-reentrant mutex.
/// Text fields are skipped when a draft is active — the draft buffers own them.
struct Snap {
    title: String,
    name: String,
    base_url: String,
    api_key: String,
    auto_start: bool,
    is_active: bool,
}

fn build_snap(app: &App, with_text: bool) -> Snap {
    let text = |s: &Option<String>| {
        if with_text {
            s.clone().unwrap_or_default()
        } else {
            String::new()
        }
    };
    let cfg = app.config();
    if app.profile_cursor >= cfg.profiles.len() {
        return Snap {
            title: "+ new account".to_string(),
            name: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            auto_start: false,
            is_active: false,
        };
    }
    match cfg.profiles.get(app.profile_cursor) {
        Some(p) => Snap {
            is_active: cfg.is_active(&p.name),
            title: p.name.to_string(),
            name: if with_text {
                p.name.to_string()
            } else {
                String::new()
            },
            base_url: text(&p.base_url),
            api_key: text(&p.api_key),
            auto_start: p.auto_start,
        },
        None => Snap {
            title: "settings".to_string(),
            name: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            auto_start: false,
            is_active: false,
        },
    }
}

fn draw_settings(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let actions_focused = app.config_focus == ConfigFocus::Actions;
    let draft = app.config_draft.as_ref();
    let snap = build_snap(app, draft.is_none());

    // Detail pane: second panel on this screen.
    let block = section_box(&snap.title, actions_focused, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = config_rows(app);
    let cursor = app.config_action_cursor.min(rows.len().saturating_sub(1));

    draw_settings_rows(frame, inner, app, &rows, cursor, &snap, actions_focused);
}

fn draw_settings_rows(
    frame: &mut Frame<'_>,
    inner: Rect,
    app: &App,
    rows: &[ConfigRow],
    cursor: usize,
    snap: &Snap,
    actions_focused: bool,
) {
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

    // Derived from the base-url buffer so it tracks the draft live.
    let is_api = !base_in.value.trim().is_empty();
    let (type_value, type_style) = if is_api {
        ("API", theme::accent())
    } else {
        ("OAuthed", Style::default().fg(theme::accent_2_color()))
    };

    let mut type_spans = vec![
        Span::styled(
            format!("type{}", " ".repeat(KEY_W - 4)),
            Style::default().fg(theme::text_color()),
        ),
        Span::styled(type_value, type_style),
    ];
    if snap.is_active {
        // "● active" = 8 chars; left side = KEY_W + type_value chars; pad the gap.
        let left_w = KEY_W + type_value.chars().count();
        let indicator_w = "● active".chars().count(); // 8
        let pad = (inner.width as usize)
            .saturating_sub(left_w)
            .saturating_sub(indicator_w);
        type_spans.push(Span::raw(" ".repeat(pad)));
        type_spans.extend(active_dot());
    }
    let mut lines: Vec<Line<'static>> = vec![Line::from(type_spans), Line::from("")];
    // Track which absolute line index the editing row occupies so we can set
    // the native terminal cursor after rendering.
    let mut edit_line_y: Option<u16> = None;
    // `lines` starts with [type, blank] = 2 entries before the loop.
    let mut line_idx: u16 = 2;

    for (i, row) in rows.iter().enumerate() {
        let selected = actions_focused && i == cursor;
        let is_editing = editing == Some(*row);
        let line = detail_row(
            *row,
            selected,
            is_editing,
            armed_delete,
            snap,
            &name_in,
            &base_in,
            &key_in,
        );
        if is_editing {
            edit_line_y = Some(line_idx);
        }
        lines.push(if selected {
            highlight_row(line, inner.width as usize)
        } else {
            line
        });
        line_idx += 1;
        if selected
            && !is_editing
            && let Some(text) = row_hint(*row)
        {
            lines.push(Line::from(vec![
                Span::styled("  └ ", theme::faint()),
                Span::styled(text, theme::faint()),
            ]));
            line_idx += 1;
        }
    }

    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);

    // Position the native terminal cursor at the caret when a field is active.
    if let (Some(row), Some(ly)) = (editing, edit_line_y) {
        let input = match row {
            ConfigRow::Name => &name_in,
            ConfigRow::BaseUrl => &base_in,
            ConfigRow::ApiKey => &key_in,
            _ => return,
        };
        // x = "❯ " (2) + key + pad (always KEY_W + 1 cols) + cols before caret
        let prefix_cols = 2 + KEY_W + 1 + head_cols(input);
        let cx = inner.x.saturating_add(prefix_cols as u16);
        let cy = inner.y.saturating_add(ly);
        frame.set_cursor_position((cx, cy));
    }
}

/// Inline help for rows whose labels don't self-describe.
fn row_hint(row: ConfigRow) -> Option<&'static str> {
    match row {
        ConfigRow::BaseUrl => Some("custom api endpoint; empty = claude.ai oauth"),
        ConfigRow::ApiKey => Some("x-api-key for a non-oauth endpoint"),
        ConfigRow::AutoStart => Some("launch a session on idle to arm the 5h window"),
        ConfigRow::Name | ConfigRow::Delete | ConfigRow::Create => None,
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
        Span::styled("❯ ", theme::accent())
    } else {
        Span::raw("  ")
    };
    match row {
        ConfigRow::Name => kv_field(arrow, "name", name_in, editing),
        ConfigRow::BaseUrl => kv_field(arrow, "base url", base_in, editing),
        ConfigRow::ApiKey => kv_field(arrow, "api key", key_in, editing),
        ConfigRow::AutoStart => {
            let (value, style) = if snap.auto_start {
                (theme::toggle_on().to_string(), theme::accent())
            } else {
                (theme::toggle_off().to_string(), theme::faint())
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

fn kv_field(arrow: Span<'static>, key: &str, input: &InputState, editing: bool) -> Line<'static> {
    let pad = KEY_W.saturating_sub(key.chars().count()).max(1);
    let mut spans = vec![
        arrow,
        Span::styled(
            format!("{key}{}", " ".repeat(pad)),
            Style::default().fg(theme::text_color()),
        ),
    ];
    spans.extend(value_spans(input, editing));
    Line::from(spans)
}

fn kv_static(arrow: Span<'static>, key: &str, value: String, value_style: Style) -> Line<'static> {
    let pad = KEY_W.saturating_sub(key.chars().count()).max(1);
    Line::from(vec![
        arrow,
        Span::styled(
            format!("{key}{}", " ".repeat(pad)),
            Style::default().fg(theme::text_color()),
        ),
        Span::styled(value, value_style),
    ])
}

fn value_spans(input: &InputState, editing: bool) -> Vec<Span<'static>> {
    if !editing {
        if input.value.is_empty() {
            return vec![Span::styled("—", theme::faint())];
        }
        return vec![Span::styled(input.value.clone(), theme::body())];
    }
    // In edit mode the terminal cursor (set via frame.set_cursor_position) owns
    // the caret glyph — no simulated block highlight needed.
    let body = Style::default()
        .fg(theme::text_color())
        .bg(theme::bg_sunken());
    vec![Span::styled(input.value.clone(), body)]
}
