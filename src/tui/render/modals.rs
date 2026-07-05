//! Modal dialogs — stacking layer above the screen.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

use crate::profile::DivergenceChoice;

use super::super::app::{
    ActionMenuState, App, ConfirmAction, ConfirmState, DivergenceForm, EnvCollisionChoice,
    EnvCollisionForm, InputState, Modal, Tab,
};
use super::super::theme;
use super::panes::{bold_when, head_cols};

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, modal: &Modal) {
    match modal {
        Modal::Confirm(state) => draw_confirm(frame, area, state),
        Modal::Divergence(form) => draw_divergence(frame, area, form),
        Modal::CaptureName(form) => draw_capture_name(frame, area, &form.input),
        Modal::Help => draw_help(frame, area, app),
        Modal::ActionMenu(state) => draw_action_menu(frame, area, state),
        Modal::EnvCollision(form) => draw_env_collision(frame, area, form),
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(4));
    let h = height.min(area.height.saturating_sub(4));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// Modal sized to content: snaps to widest line/title, exact line count.
/// Chrome = rounded border (1) + `Padding::new(2,2,1,1)` = 6 cols, 4 rows.
fn draw_modal(frame: &mut Frame<'_>, area: Rect, title: &str, lines: Vec<Line<'_>>) {
    let content_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let w = (content_w + 6)
        .max(title.chars().count() as u16 + 4)
        .min(area.width.saturating_sub(4));
    let h = (lines.len() as u16 + 4).min(area.height.saturating_sub(4));

    let rect = centered(area, w, h);
    frame.render_widget(Clear, rect);
    let block = modal_block(title);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

/// Rounded `ACCENT_2` border, uppercase italic dim title, base `BG` fill.
fn modal_block(title: impl Into<String>) -> Block<'static> {
    let title_line = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            title.into().to_uppercase(),
            Style::default()
                .fg(theme::text_dim_color())
                .add_modifier(Modifier::ITALIC),
        ),
        Span::raw(" "),
    ]);
    Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(theme::accent_2_color()))
        .title(title_line)
        .style(theme::base())
        .padding(Padding::new(2, 2, 1, 1))
}

fn draw_confirm(frame: &mut Frame<'_>, area: Rect, state: &ConfirmState) {
    let title = match state.on_confirm {
        ConfirmAction::CaptureConflict(..) => "CONFIRM",
        ConfirmAction::Switch(_) => "CONFIRM",
        ConfirmAction::DiscardDivergence(_) => "CONFIRM",
        ConfirmAction::RotateAll => "CONFIRM",
        ConfirmAction::RotateOne(_) => "CONFIRM",
        ConfirmAction::WireMcpServers => "CONFIRM",
        ConfirmAction::RelinkCredentials(_) => "CONFIRM",
    };

    // Destructive/global ops carry a DANGER cue on their confirm button.
    let destructive = matches!(
        state.on_confirm,
        ConfirmAction::Switch(_) | ConfirmAction::RotateAll | ConfirmAction::RotateOne(_)
    );

    let mut lines: Vec<Line<'_>> = vec![Line::from(Span::styled(
        state.message.clone(),
        theme::body(),
    ))];
    if let Some(detail) = &state.detail {
        lines.push(Line::from(Span::styled(detail.clone(), theme::dim())));
    }
    lines.push(Line::from(""));
    lines.push(choice_buttons(state.choice, destructive).alignment(Alignment::Right));

    draw_modal(frame, area, title, lines);
}

fn choice_buttons(choice: bool, destructive_confirm: bool) -> Line<'static> {
    Line::from(vec![
        modal_button(" cancel ", !choice),
        Span::raw("   "),
        if destructive_confirm {
            danger_button(" confirm ", choice)
        } else {
            modal_button(" confirm ", choice)
        },
    ])
}

fn modal_button(label: &str, focused: bool) -> Span<'static> {
    if focused {
        Span::styled(
            label.to_string(),
            Style::default().fg(theme::bg()).bg(theme::text_color()),
        )
    } else {
        Span::styled(label.to_string(), theme::dim())
    }
}

/// Destructive variant of `modal_button`: DANGER fg unfocused, inverse DANGER block
/// focused. Same bar-less house style as `modal_button` (no `▐`/`▌`).
fn danger_button(label: &str, focused: bool) -> Span<'static> {
    if focused {
        Span::styled(
            label.to_string(),
            Style::default().fg(theme::bg()).bg(theme::danger_color()),
        )
    } else {
        Span::styled(label.to_string(), theme::danger())
    }
}

fn draw_divergence(frame: &mut Frame<'_>, area: Rect, form: &DivergenceForm) {
    let options = DivergenceForm::options();
    let cursor = form.cursor.min(options.len() - 1);

    let mut lines: Vec<Line<'_>> = vec![
        Line::from(vec![
            Span::styled("~/.claude/.credentials.json", theme::body()),
            Span::styled(" no longer points to ", theme::dim()),
            Span::styled(
                format!("'{}'", form.active),
                Style::default().fg(theme::accent_color()),
            ),
            Span::styled(".", theme::dim()),
        ]),
        Line::from(Span::styled(
            "Claude Code re-logged or refreshed via unlink+write.",
            theme::dim(),
        )),
        Line::from(""),
    ];

    for (i, option) in options.iter().enumerate() {
        let selected = i == cursor;
        let arrow = if selected {
            Span::styled("\u{276f} ", theme::accent())
        } else {
            Span::raw("  ")
        };
        let (label, detail) = divergence_option_text(*option, &form.active);
        let label_style = if selected {
            theme::accent()
        } else {
            theme::dim()
        };
        lines.push(Line::from(vec![arrow, Span::styled(label, label_style)]));
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(detail, theme::dim()),
        ]));
    }

    draw_modal(frame, area, "DIVERGENCE", lines);
}

fn divergence_option_text(option: DivergenceChoice, active: &str) -> (String, String) {
    match option {
        DivergenceChoice::Overwrite => (
            format!("overwrite '{active}' with new credentials"),
            "save the live tokens into the active profile and re-link".to_string(),
        ),
        DivergenceChoice::NewProfile => (
            "save new credentials as a new profile".to_string(),
            format!("preserve '{active}' as-is and capture the live tokens elsewhere"),
        ),
        DivergenceChoice::Discard => (
            format!("discard new credentials, restore '{active}'"),
            "overwrites the live file with the profile's stored tokens".to_string(),
        ),
    }
}

fn draw_env_collision(frame: &mut Frame<'_>, area: Rect, form: &EnvCollisionForm) {
    let options = EnvCollisionForm::options();
    let cursor = form.cursor.min(options.len() - 1);

    let mut lines: Vec<Line<'_>> = vec![
        Line::from(vec![
            Span::styled(
                format!("'{}'", form.key),
                Style::default().fg(theme::accent_color()),
            ),
            Span::styled(" is already used by ", theme::dim()),
            Span::styled(form.reason.clone(), theme::body()),
            Span::styled(".", theme::dim()),
        ]),
        Line::from(""),
    ];

    for (i, option) in options.iter().enumerate() {
        let selected = i == cursor;
        let arrow = if selected {
            Span::styled("\u{276f} ", theme::accent())
        } else {
            Span::raw("  ")
        };
        let (label, detail) = env_collision_option_text(*option, form);
        let label_style = if selected {
            theme::accent()
        } else {
            theme::dim()
        };
        lines.push(Line::from(vec![arrow, Span::styled(label, label_style)]));
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(detail, theme::dim()),
        ]));
    }

    draw_modal(frame, area, "KEY IN USE", lines);
}

fn env_collision_option_text(
    choice: EnvCollisionChoice,
    form: &EnvCollisionForm,
) -> (String, String) {
    match choice {
        EnvCollisionChoice::Overwrite => (
            "add the custom field anyway".to_string(),
            format!("this account's value overrides {}", form.reason),
        ),
        EnvCollisionChoice::KeepExisting => (
            "keep the existing value".to_string(),
            if form.existing_idx.is_some() {
                "jump to the existing custom field".to_string()
            } else {
                "leave it untouched; don't add the field".to_string()
            },
        ),
        EnvCollisionChoice::Cancel => ("cancel".to_string(), "back out, no change".to_string()),
    }
}

fn draw_capture_name(frame: &mut Frame<'_>, area: Rect, input: &InputState) {
    let lines = vec![
        Line::from(Span::styled(
            "stores the live ~/.claude/.credentials.json under this profile.",
            theme::dim(),
        )),
        Line::from(""),
        labelled_input("name", input, true),
    ];

    // Replicate draw_modal's geometry to place the native terminal cursor on the
    // input line (index 2 in the vec).  Chrome = border (1) + padding (2 left, 1 top).
    let title = "CAPTURE";
    let content_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let w = (content_w + 6)
        .max(title.chars().count() as u16 + 4)
        .min(area.width.saturating_sub(4));
    let h = (lines.len() as u16 + 4).min(area.height.saturating_sub(4));
    let rect = {
        let cw = w.min(area.width.saturating_sub(4));
        let ch = h.min(area.height.saturating_sub(4));
        Rect {
            x: area.x + (area.width.saturating_sub(cw)) / 2,
            y: area.y + (area.height.saturating_sub(ch)) / 2,
            width: cw,
            height: ch,
        }
    };
    // inner = rect + border (1) + padding left/top (2, 1)
    let inner_x = rect.x.saturating_add(3);
    let inner_y = rect.y.saturating_add(2);

    draw_modal(frame, area, title, lines);

    // x = edit gutter "✎ " (2) + label "name" (4) + " " (1) + cols before caret
    let cx = inner_x.saturating_add(2 + 4 + 1 + head_cols(input) as u16);
    let cy = inner_y.saturating_add(2); // line index 2
    frame.set_cursor_position((cx, cy));
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = "KEYS";

    let tab_specific: Vec<(&str, &[(&str, &str)])> = match app.tab {
        Tab::Overview => vec![(
            "accounts",
            &[
                ("\u{2191}\u{2193}", "move cursor"),
                ("\u{23ce}", "switch to selected account (confirm)"),
                ("shift \u{2191}\u{2193}", "reorder account up / down"),
            ][..],
        )],
        Tab::Usage => vec![(
            "usage",
            &[("\u{2191}\u{2193}", "pick account to inspect")][..],
        )],
        Tab::Tokens => vec![(
            "tokens",
            &[
                ("\u{23ce}", "open per-model breakdown"),
                ("\u{2191}\u{2193}", "pick model (in breakdown)"),
                ("c", "count cache in token figures"),
                ("r", "reload on-disk stats"),
                ("esc", "back to dashboard"),
            ][..],
        )],
        Tab::Setup => vec![(
            "setup",
            &[
                ("\u{2191}\u{2193}", "pick account / + new, then a row"),
                ("\u{23ce}", "open settings · edit field · flip toggle"),
                ("\u{23ce} on a field", "edit inline; \u{23ce} again saves"),
                ("env", "+ add env · \u{23ce} edits a value · a removes"),
                ("delete", "\u{23ce} once to arm, again to confirm"),
                ("\u{238b}", "stop editing / back to account list"),
            ][..],
        )],
        Tab::Config => vec![(
            "config",
            &[
                ("\u{2191}\u{2193}", "move between settings"),
                ("space", "cycle the focused setting"),
                ("\u{23ce}", "type a custom refresh interval"),
            ][..],
        )],
        Tab::Status => vec![(
            "status",
            &[
                ("\u{2191}\u{2193}", "pick incident / scroll detail"),
                ("\u{23ce}", "open incident timeline"),
                ("r", "refresh the feed"),
            ][..],
        )],
        Tab::Plugin => vec![(
            "plugin",
            &[
                ("\u{2191}\u{2193}", "pick check · scroll detail"),
                ("\u{23ce}", "open the selected row's detail"),
                ("f", "apply the selected row's fix"),
                ("r", "re-run all checks"),
                ("esc", "back to the list"),
            ][..],
        )],
        Tab::Fallback => vec![(
            "fallback chain",
            &[
                ("\u{2191}\u{2193}", "move cursor / detail row"),
                ("shift \u{2191}\u{2193}", "reorder member = priority"),
                (
                    "\u{23ce}",
                    "open \u{00b7} edit threshold \u{00b7} remove \u{00b7} add",
                ),
                ("+ / -", "step threshold by 5"),
                ("\u{23ce}", "type a threshold, \u{23ce} saves"),
                ("esc", "back / cancel edit"),
            ][..],
        )],
    };

    let nav: &[(&str, &str)] = &[(
        "\u{2190} \u{2192} \u{00b7} tab",
        "previous / next tab (shift tab: previous)",
    )];

    let global: &[(&str, &str)] = &[
        ("n", "new account"),
        ("r", "refresh usage now"),
        ("t", "rotate all tokens"),
        ("?", "toggle this help"),
        ("a", "actions"),
        ("q", "back / quit"),
        ("\u{2303}c", "quit from anywhere"),
    ];

    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.extend(key_section("tabs", nav));
    for (section, entries) in &tab_specific {
        lines.extend(key_section(section, entries));
    }
    lines.extend(key_section("global", global));
    lines.pop(); // trim trailing blank from last section
    draw_modal(frame, area, title, lines);
}

fn key_section(title: &str, pairs: &[(&str, &str)]) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            title.to_uppercase(),
            Style::default().fg(theme::text_dim_color()),
        )),
        Line::from(""),
    ];
    for (key, desc) in pairs {
        lines.push(help_row(key, desc));
    }
    lines.push(Line::from(""));
    lines
}

fn help_row(key: &str, desc: &str) -> Line<'static> {
    // Always leave at least 1 space — `{:<18}` emits no padding at the width.
    const KEY_W: usize = 18;
    let pad = KEY_W.saturating_sub(key.chars().count()).max(1);
    Line::from(vec![
        Span::styled(
            format!("  {key}{}", " ".repeat(pad)),
            Style::default().fg(theme::accent_color()).bold(),
        ),
        Span::styled(desc.to_string(), Style::default().fg(theme::text_color())),
    ])
}

fn labelled_input(label: &str, input: &InputState, focused: bool) -> Line<'static> {
    // When focused the native terminal cursor owns the caret — no block highlight.
    // Unfocused fields still render with plain text styling (no BG_SUNKEN tint).
    // A focused field carries the `✎` edit-mode gutter glyph (same as form rows);
    // the 2-col gutter is accounted for in the caller's cursor-x math.
    let value_style = if focused {
        Style::default()
            .fg(theme::text_color())
            .bg(theme::bg_sunken())
    } else {
        Style::default().fg(theme::text_color())
    };
    let gutter = if focused {
        Span::styled(format!("{} ", theme::edit_glyph()), theme::accent())
    } else {
        Span::raw("  ")
    };
    Line::from(vec![
        gutter,
        Span::styled(label.to_string(), theme::label()),
        Span::raw(" "),
        Span::styled(input.value.clone(), value_style),
    ])
}

fn draw_action_menu(frame: &mut Frame<'_>, area: Rect, state: &ActionMenuState) {
    const HOTKEY_W: u16 = 1; // 1 char for hotkey letter, or 1 space if none
    const GUTTER: u16 = 2; // "❯ " or "  "

    // Render rows with right-aligned hotkeys — can't use draw_modal because that
    // wraps all lines in one Paragraph, preventing per-row background tinting.
    // Custom draw: measure → size → clear → border → per-row widgets.
    let max_label_w = state
        .items
        .iter()
        .map(|item| item.label.chars().count())
        .max()
        .unwrap_or(0) as u16;
    let content_w = GUTTER + max_label_w + 3 + HOTKEY_W;
    let title = "actions";
    let w = (content_w + 6)
        .max(title.chars().count() as u16 + 4)
        .min(area.width.saturating_sub(4));
    // items rows + 4 chrome (border + padding)
    let h = (state.items.len() as u16 + 4).min(area.height.saturating_sub(4));

    let rect = centered(area, w, h);
    frame.render_widget(Clear, rect);
    let block = modal_block(title);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let inner_w = inner.width;
    for (i, item) in state.items.iter().enumerate() {
        let focused = i == state.cursor;
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let row_area = Rect {
            y,
            height: 1,
            ..inner
        };

        let label_style = bold_when(Style::default().fg(theme::text_color()), focused);
        let row_bg = if focused {
            Style::default().bg(theme::bg_hover())
        } else {
            theme::base()
        };
        let glyph = if focused {
            Span::styled("❯ ", Style::default().fg(theme::accent_color()).bold())
        } else {
            Span::styled("  ", Style::default())
        };
        let label_len = item.label.chars().count() as u16;
        let pad = inner_w
            .saturating_sub(GUTTER)
            .saturating_sub(label_len)
            .saturating_sub(HOTKEY_W);
        let padding = Span::styled(" ".repeat(pad as usize), Style::default());
        let hotkey_span = match item.hotkey {
            Some(c) => Span::styled(c.to_string(), Style::default().fg(theme::text_dim_color())),
            None => Span::styled(
                " ".to_string(),
                Style::default().fg(theme::text_dim_color()),
            ),
        };
        let line = Line::from(vec![
            glyph,
            Span::styled(item.label.to_string(), label_style),
            padding,
            hotkey_span,
        ])
        .style(row_bg);
        frame.render_widget(Paragraph::new(line).style(row_bg), row_area);
    }
}
