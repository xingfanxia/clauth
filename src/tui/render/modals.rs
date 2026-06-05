//! Modal dialogs — stacking layer above the screen.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

use super::super::app::{
    App, ConfirmAction, ConfirmState, DivergenceChoice, DivergenceForm, InputState, Modal, Tab,
};
use super::super::theme;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, modal: &Modal) {
    match modal {
        Modal::Confirm(state) => draw_confirm(frame, area, state),
        Modal::Divergence(form) => draw_divergence(frame, area, form),
        Modal::CaptureName(form) => draw_capture_name(frame, area, form.input.value.as_str()),
        Modal::Help => draw_help(frame, area, app),
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
                .fg(theme::TEXT_DIM)
                .add_modifier(Modifier::ITALIC),
        ),
        Span::raw(" "),
    ]);
    Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(theme::ACCENT_2))
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
    };

    let mut lines: Vec<Line<'_>> = vec![Line::from(Span::styled(
        state.message.clone(),
        theme::body(),
    ))];
    if let Some(detail) = &state.detail {
        lines.push(Line::from(Span::styled(detail.clone(), theme::dim())));
    }
    lines.push(Line::from(""));
    lines.push(choice_buttons(state.choice).alignment(Alignment::Center));
    lines.push(Line::from(""));
    lines.push(
        modal_footer_hints(&[("← →", "choose"), ("⏎", "apply")]).alignment(Alignment::Center),
    );

    draw_modal(frame, area, title, lines);
}

fn choice_buttons(choice: bool) -> Line<'static> {
    Line::from(vec![
        modal_button(" cancel ", !choice),
        Span::raw("  "),
        modal_button(" confirm ", choice),
    ])
}

fn modal_button(label: &str, focused: bool) -> Span<'static> {
    if focused {
        Span::styled(
            format!("\u{2590}{label}\u{258c}"),
            Style::default().fg(theme::BG).bg(theme::TEXT),
        )
    } else {
        Span::styled(label.to_string(), theme::dim())
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
                Style::default().fg(theme::ACCENT),
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

    lines.push(Line::from(""));
    lines.push(
        modal_footer_hints(&[("↑ ↓", "choose"), ("⏎", "apply"), ("⎋", "dismiss")])
            .alignment(Alignment::Center),
    );

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

fn draw_capture_name(frame: &mut Frame<'_>, area: Rect, value: &str) {
    let input = InputState {
        value: value.to_string(),
        cursor: value.len(),
    };
    let lines = vec![
        Line::from(Span::styled(
            "Stores the live ~/.claude/.credentials.json under this profile.",
            theme::dim(),
        )),
        Line::from(""),
        labelled_input("name", &input, true),
        Line::from(""),
        modal_footer_hints(&[("⏎", "capture"), ("⎋", "cancel")]).alignment(Alignment::Center),
    ];
    draw_modal(frame, area, "CAPTURE", lines);
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = "KEYS";

    let tab_specific: Vec<(&str, &[(&str, &str)])> = match app.tab {
        Tab::Overview => vec![(
            "accounts",
            &[
                ("\u{2191}\u{2193}", "move cursor"),
                ("\u{23ce}", "switch to selected account (confirm)"),
                ("\u{21e7}\u{2191}\u{2193}", "reorder account up / down"),
            ][..],
        )],
        Tab::Usage => vec![(
            "usage",
            &[("\u{2191}\u{2193}", "pick account to inspect")][..],
        )],
        Tab::Config => vec![(
            "config",
            &[
                ("\u{2191}\u{2193}", "pick account / + new, then a row"),
                ("\u{23ce}", "open settings · edit field · flip toggle"),
                ("\u{23ce} on a field", "edit inline; \u{23ce} again saves"),
                ("delete", "\u{23ce} once to arm, again to confirm"),
                ("\u{238b}", "stop editing / back to account list"),
            ][..],
        )],
        Tab::Fallback => vec![(
            "fallback chain",
            &[
                ("\u{2191}\u{2193}", "move cursor / detail row"),
                ("\u{21e7}\u{2191}\u{2193}", "reorder member up / down"),
                (
                    "\u{23ce}",
                    "open \u{00b7} edit threshold \u{00b7} remove \u{00b7} add",
                ),
                ("+ / -", "step threshold by 5"),
                ("0-9 \u{23ce}", "type a threshold, \u{23ce} saves"),
                ("\u{238b}", "back / cancel edit"),
            ][..],
        )],
    };

    let nav: &[(&str, &str)] = &[("\u{2190} \u{2192}", "previous / next tab")];

    let global: &[(&str, &str)] = &[
        ("n", "new account"),
        ("r", "refresh usage now"),
        ("t", "rotate all tokens"),
        ("?", "toggle this help"),
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
            Style::default().fg(theme::TEXT_DIM),
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
            Style::default().fg(theme::ACCENT).bold(),
        ),
        Span::styled(desc.to_string(), Style::default().fg(theme::TEXT)),
    ])
}

fn modal_footer_hints(hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", theme::faint()));
        }
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default().fg(theme::ACCENT).bold(),
        ));
        spans.push(Span::styled(format!(" {label}"), theme::dim()));
    }
    Line::from(spans)
}

fn labelled_input(label: &str, input: &InputState, focused: bool) -> Line<'static> {
    let (head, tail) = input.value.split_at(input.cursor.min(input.value.len()));
    let caret_style = if focused {
        Style::default()
            .fg(theme::TEXT)
            .bg(theme::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::TEXT)
    };
    let body_style = Style::default().fg(theme::TEXT).bg(theme::BG_SUNKEN);

    let mut tail_iter = tail.chars();
    let caret_char = tail_iter.next().unwrap_or(' ').to_string();
    let after: String = tail_iter.collect();

    Line::from(vec![
        Span::styled(label.to_string(), theme::label()),
        Span::raw(" "),
        Span::styled(head.to_string(), body_style),
        Span::styled(caret_char, caret_style),
        Span::styled(after, body_style),
    ])
}
