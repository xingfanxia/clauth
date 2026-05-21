//! Modal dialogs — stacking layer above the screen.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

use super::super::app::{
    App, ChainAction, ChainAddState, ChainItemMenuState, ChainThresholdForm, ConfirmAction,
    ConfirmState, EditProfileForm, EndpointField, InputState, Modal, NewProfileField,
    NewProfileForm, ProfileMenuAction, ProfileMenuState, RenameForm, Screen, profile_menu_options,
};
use super::super::theme;
use crate::fallback::{DEFAULT_THRESHOLD, threshold_for};

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, modal: &Modal) {
    match modal {
        Modal::NewProfile(form) => draw_new_profile(frame, area, form),
        Modal::EditProfile(form) => draw_edit_profile(frame, area, form),
        Modal::Rename(form) => draw_rename(frame, area, form),
        Modal::Confirm(state) => draw_confirm(frame, area, state),
        Modal::ReconcileKeep { active, choice } => {
            draw_reconcile_keep(frame, area, active, *choice)
        }
        Modal::ReconcileCaptureAsk { choice } => draw_reconcile_capture(frame, area, *choice),
        Modal::CaptureName(form) => draw_capture_name(frame, area, form.input.value.as_str()),
        Modal::ProfileMenu(state) => draw_profile_menu(frame, area, app, state),
        Modal::ChainItemMenu(state) => draw_chain_item_menu(frame, area, app, state),
        Modal::ChainAdd(state) => draw_chain_add(frame, area, state),
        Modal::ChainThreshold(form) => draw_chain_threshold(frame, area, form),
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

fn modal_block(title: impl Into<String>) -> Block<'static> {
    let title_text = title.into().to_uppercase();
    let title_line = Line::from(vec![
        Span::raw(" "),
        Span::styled(title_text, theme::label()),
        Span::raw(" "),
    ]);
    Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(theme::LINE_STRONG))
        .title(title_line)
        .style(Style::default().bg(theme::BG_RAISED))
        .padding(Padding::new(2, 2, 1, 1))
}

fn draw_new_profile(frame: &mut Frame<'_>, area: Rect, form: &NewProfileForm) {
    let rect = centered(area, 64, 14);
    frame.render_widget(Clear, rect);
    let block = modal_block("new profile");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let lines = vec![
        Line::from(Span::styled(
            "Create a blank OAuth or API endpoint profile.",
            theme::dim(),
        )),
        Line::from(""),
        labelled_input("name", &form.name, form.focus == NewProfileField::Name),
        labelled_input(
            "base url (blank = oauth)",
            &form.base_url,
            form.focus == NewProfileField::BaseUrl,
        ),
        labelled_input(
            "api key (only with base url)",
            &form.api_key,
            form.focus == NewProfileField::ApiKey,
        ),
        Line::from(""),
        modal_footer_hints(&[("⇥", "next field"), ("⏎", "submit"), ("⎋", "cancel")]),
    ];
    let para = Paragraph::new(lines).style(theme::base().bg(theme::BG_RAISED));
    frame.render_widget(para, inner);
}

fn draw_edit_profile(frame: &mut Frame<'_>, area: Rect, form: &EditProfileForm) {
    let rect = centered(area, 64, 12);
    frame.render_widget(Clear, rect);
    let block = modal_block(format!("edit · {}", form.name));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let lines = vec![
        Line::from(Span::styled(
            "Empty base URL = OAuth profile. API key only applies with a URL.",
            theme::dim(),
        )),
        Line::from(""),
        labelled_input(
            "base url",
            &form.base_url,
            form.focus == EndpointField::BaseUrl,
        ),
        labelled_input(
            "api key",
            &form.api_key,
            form.focus == EndpointField::ApiKey,
        ),
        Line::from(""),
        modal_footer_hints(&[("⇥", "next"), ("⏎", "save"), ("⎋", "cancel")]),
    ];
    let para = Paragraph::new(lines).style(theme::base().bg(theme::BG_RAISED));
    frame.render_widget(para, inner);
}

fn draw_rename(frame: &mut Frame<'_>, area: Rect, form: &RenameForm) {
    let rect = centered(area, 60, 9);
    frame.render_widget(Clear, rect);
    let block = modal_block(format!("rename · {}", form.old));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let lines = vec![
        Line::from(Span::styled(
            "Letters, digits, '-', '_', '.'  ·  no leading '.'",
            theme::dim(),
        )),
        Line::from(""),
        labelled_input("new name", &form.input, true),
        Line::from(""),
        modal_footer_hints(&[("⏎", "rename"), ("⎋", "cancel")]),
    ];
    let para = Paragraph::new(lines).style(theme::base().bg(theme::BG_RAISED));
    frame.render_widget(para, inner);
}

fn draw_confirm(frame: &mut Frame<'_>, area: Rect, state: &ConfirmState) {
    let rect = centered(area, 60, 10);
    frame.render_widget(Clear, rect);
    let title = match state.on_confirm {
        ConfirmAction::Delete(_) => "confirm · delete",
        ConfirmAction::CaptureConflict(_) => "confirm · duplicate",
    };
    let block = modal_block(title);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines: Vec<Line<'_>> = vec![Line::from(Span::styled(
        state.message.clone(),
        theme::muted(),
    ))];
    if let Some(detail) = &state.detail {
        lines.push(Line::from(Span::styled(detail.clone(), theme::dim())));
    }
    lines.push(Line::from(""));
    lines.push(yes_no_line(state.choice));
    lines.push(Line::from(""));
    lines.push(modal_footer_hints(&[
        ("← →", "choose"),
        ("y / n", "choose"),
        ("⏎", "apply"),
    ]));

    let para = Paragraph::new(lines).style(theme::base().bg(theme::BG_RAISED));
    frame.render_widget(para, inner);
}

fn yes_no_line(choice: bool) -> Line<'static> {
    let no = if !choice {
        Span::styled(
            " no ",
            Style::default().fg(theme::BG).bg(theme::TEXT).bold(),
        )
    } else {
        Span::styled(" no ", theme::dim())
    };
    let yes = if choice {
        Span::styled(
            " yes ",
            Style::default().fg(theme::TEXT).bg(theme::ACCENT).bold(),
        )
    } else {
        Span::styled(" yes ", theme::dim())
    };
    Line::from(vec![no, Span::raw("  "), yes])
}

fn draw_reconcile_keep(frame: &mut Frame<'_>, area: Rect, active: &str, choice: bool) {
    let rect = centered(area, 70, 13);
    frame.render_widget(Clear, rect);
    let block = modal_block("startup · credential divergence");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let lines = vec![
        Line::from(vec![
            Span::styled("~/.claude/.credentials.json", theme::muted()),
            Span::styled(" differs from this profile's saved tokens.", theme::dim()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Still logged in as ", theme::dim()),
            Span::styled(format!("'{active}'"), Style::default().fg(theme::ACCENT)),
            Span::styled("?", theme::dim()),
        ]),
        Line::from(""),
        yes_no_line(choice),
        Line::from(""),
        Line::from(vec![
            Span::styled("yes", theme::accent()),
            Span::styled(
                " — overwrites stored tokens with the live ones.",
                theme::dim(),
            ),
        ]),
        Line::from(vec![
            Span::styled("no", theme::accent()),
            Span::styled(
                " — disowns this profile and offers to capture instead.",
                theme::dim(),
            ),
        ]),
        Line::from(""),
        modal_footer_hints(&[("← →", "choose"), ("⏎", "apply")]),
    ];
    let para = Paragraph::new(lines).style(theme::base().bg(theme::BG_RAISED));
    frame.render_widget(para, inner);
}

fn draw_reconcile_capture(frame: &mut Frame<'_>, area: Rect, choice: bool) {
    let rect = centered(area, 60, 9);
    frame.render_widget(Clear, rect);
    let block = modal_block("startup · capture credentials?");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let lines = vec![
        Line::from(Span::styled(
            "Capture current credentials as a new profile?",
            theme::muted(),
        )),
        Line::from(""),
        yes_no_line(choice),
        Line::from(""),
        modal_footer_hints(&[("← →", "choose"), ("⏎", "apply")]),
    ];
    let para = Paragraph::new(lines).style(theme::base().bg(theme::BG_RAISED));
    frame.render_widget(para, inner);
}

fn draw_capture_name(frame: &mut Frame<'_>, area: Rect, value: &str) {
    let rect = centered(area, 60, 9);
    frame.render_widget(Clear, rect);
    let block = modal_block("capture · new profile name");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

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
        modal_footer_hints(&[("⏎", "capture"), ("⎋", "cancel")]),
    ];
    let para = Paragraph::new(lines).style(theme::base().bg(theme::BG_RAISED));
    frame.render_widget(para, inner);
}

fn draw_profile_menu(frame: &mut Frame<'_>, area: Rect, app: &App, state: &ProfileMenuState) {
    let options = profile_menu_options(app, &state.name);
    let body_height = options.len() as u16 + 4;
    let rect = centered(area, 56, body_height.max(8));
    frame.render_widget(Clear, rect);
    let block = modal_block(format!("profile \u{00b7} {}", state.name));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let cursor = state.cursor.min(options.len().saturating_sub(1));
    let auto_on = app
        .config
        .find(&state.name)
        .map(|p| p.auto_start)
        .unwrap_or(false);
    let in_chain = app
        .config
        .state
        .fallback_chain
        .iter()
        .any(|n| n == &state.name);
    let current_threshold = app
        .config
        .find(&state.name)
        .map(threshold_for)
        .unwrap_or(DEFAULT_THRESHOLD);

    let mut lines: Vec<Line<'_>> = options
        .iter()
        .enumerate()
        .map(|(i, action)| {
            let arrow = if i == cursor {
                Span::styled("\u{25b6} ", theme::orange())
            } else {
                Span::raw("  ")
            };
            let label = profile_menu_label(*action, auto_on, in_chain, current_threshold);
            let style = match action {
                ProfileMenuAction::Delete => theme::danger(),
                ProfileMenuAction::Back => theme::faint(),
                _ => Style::default().fg(theme::TEXT),
            };
            Line::from(vec![arrow, Span::styled(label, style)])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(modal_footer_hints(&[
        ("\u{2191}\u{2193}", "nav"),
        ("\u{23ce}", "select"),
        ("\u{238b}", "close"),
    ]));
    let para = Paragraph::new(lines).style(theme::base().bg(theme::BG_RAISED));
    frame.render_widget(para, inner);
}

fn profile_menu_label(
    action: ProfileMenuAction,
    auto_on: bool,
    in_chain: bool,
    threshold: f64,
) -> String {
    match action {
        ProfileMenuAction::Switch => "Switch to this profile".to_string(),
        ProfileMenuAction::Details => "Open details".to_string(),
        ProfileMenuAction::Edit => "Edit endpoint".to_string(),
        ProfileMenuAction::Rename => "Rename".to_string(),
        ProfileMenuAction::ToggleAutoStart => {
            if auto_on {
                "Auto-start usage: on  \u{2192}  turn off".to_string()
            } else {
                "Auto-start usage: off  \u{2192}  turn on".to_string()
            }
        }
        ProfileMenuAction::AddToChain => "Add to fallback chain".to_string(),
        ProfileMenuAction::SetThreshold => {
            if in_chain {
                format!("Set threshold (current: {threshold:.0}%)")
            } else {
                "Set threshold".to_string()
            }
        }
        ProfileMenuAction::RemoveFromChain => "Remove from fallback chain".to_string(),
        ProfileMenuAction::Delete => "Delete profile".to_string(),
        ProfileMenuAction::Back => "\u{2190} Back".to_string(),
    }
}

fn draw_chain_item_menu(frame: &mut Frame<'_>, area: Rect, app: &App, state: &ChainItemMenuState) {
    let rect = centered(area, 56, 14);
    frame.render_widget(Clear, rect);
    let block = modal_block(format!("chain · {}", state.name));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let chain_len = app.config.state.fallback_chain.len();
    let position = app
        .config
        .state
        .fallback_chain
        .iter()
        .position(|n| n == &state.name);
    let current = app
        .config
        .find(&state.name)
        .map(threshold_for)
        .unwrap_or(DEFAULT_THRESHOLD);

    let mut options: Vec<(ChainAction, String)> = vec![(
        ChainAction::Threshold,
        format!("Set threshold (current: {current:.0}%)"),
    )];
    if matches!(position, Some(p) if p > 0) {
        options.push((ChainAction::MoveUp, "Move up".to_string()));
    }
    if matches!(position, Some(p) if p + 1 < chain_len) {
        options.push((ChainAction::MoveDown, "Move down".to_string()));
    }
    options.push((ChainAction::Remove, "Remove from chain".to_string()));
    options.push((ChainAction::Back, "← Back".to_string()));

    let cursor = state.cursor.min(options.len().saturating_sub(1));
    let mut lines: Vec<Line<'_>> = options
        .iter()
        .enumerate()
        .map(|(i, (action, label))| {
            let arrow = if i == cursor {
                Span::styled("▶ ", theme::orange())
            } else {
                Span::raw("  ")
            };
            let body = match action {
                ChainAction::Remove => Span::styled(label.clone(), theme::danger()),
                ChainAction::Back => Span::styled(label.clone(), theme::faint()),
                _ => Span::styled(label.clone(), Style::default().fg(theme::TEXT)),
            };
            Line::from(vec![arrow, body])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(modal_footer_hints(&[
        ("↑↓", "nav"),
        ("⏎", "select"),
        ("⎋", "back"),
    ]));
    let para = Paragraph::new(lines).style(theme::base().bg(theme::BG_RAISED));
    frame.render_widget(para, inner);
}

fn draw_chain_add(frame: &mut Frame<'_>, area: Rect, state: &ChainAddState) {
    let rect = centered(
        area,
        50,
        (state.candidates.len() as u16 + 8).min(area.height),
    );
    frame.render_widget(Clear, rect);
    let block = modal_block("chain · add profile");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let cursor = state.cursor.min(state.candidates.len().saturating_sub(1));
    let mut lines: Vec<Line<'_>> = state
        .candidates
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let arrow = if i == cursor {
                Span::styled("▶ ", theme::orange())
            } else {
                Span::raw("  ")
            };
            Line::from(vec![
                arrow,
                Span::styled(name.clone(), Style::default().fg(theme::TEXT)),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(modal_footer_hints(&[
        ("↑↓", "nav"),
        ("⏎", "add"),
        ("⎋", "cancel"),
    ]));
    let para = Paragraph::new(lines).style(theme::base().bg(theme::BG_RAISED));
    frame.render_widget(para, inner);
}

fn draw_chain_threshold(frame: &mut Frame<'_>, area: Rect, form: &ChainThresholdForm) {
    let rect = centered(area, 60, 11);
    frame.render_widget(Clear, rect);
    let block = modal_block(format!("threshold · {}", form.name));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let lines = vec![
        Line::from(Span::styled(
            "Auto-switch off this profile when 5h utilization ≥ this value.",
            theme::dim(),
        )),
        Line::from(Span::styled(
            "Range 0..=100. 100 marks the profile as a last-resort slot.",
            theme::dim(),
        )),
        Line::from(""),
        labelled_input("threshold %", &form.input, true),
        Line::from(""),
        modal_footer_hints(&[("⏎", "save"), ("⎋", "cancel")]),
    ];
    let para = Paragraph::new(lines).style(theme::base().bg(theme::BG_RAISED));
    frame.render_widget(para, inner);
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rect = centered(area, 70, 24);
    frame.render_widget(Clear, rect);
    let title = match app.screen {
        Screen::Overview => "help \u{00b7} overview",
        Screen::Chain => "help \u{00b7} fallback chain",
        Screen::ProfileDetail { .. } => "help \u{00b7} profile detail",
    };
    let block = modal_block(title);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let screen_specific: Vec<(&str, &[(&str, &str)])> = match app.screen {
        Screen::Overview => vec![
            (
                "ACCOUNTS",
                &[
                    (
                        "\u{23ce} / m",
                        "open per-profile menu (every action lives here)",
                    ),
                    ("Shift+j / Shift+k", "reorder profile up / down"),
                ][..],
            ),
            (
                "LIST",
                &[
                    ("\u{2191}\u{2193} / j k", "move cursor"),
                    ("/", "filter by name"),
                    ("r", "refresh usage now"),
                ][..],
            ),
        ],
        Screen::Chain => vec![(
            "CHAIN",
            &[
                ("\u{2191}\u{2193} / j k", "move cursor"),
                ("\u{23ce}", "open entry / add profile"),
                ("\u{238b}", "back to overview"),
                ("r", "refresh usage now"),
            ][..],
        )],
        Screen::ProfileDetail { .. } => vec![(
            "PROFILE",
            &[
                (
                    "\u{23ce} / m",
                    "open per-profile menu (every action lives here)",
                ),
                ("r", "refresh usage now"),
                ("\u{238b}", "back to overview"),
            ][..],
        )],
    };

    let global: &[(&str, &str)] = &[
        ("?", "toggle this help"),
        ("q", "quit"),
        ("Ctrl+C", "quit from anywhere"),
    ];

    let mut lines: Vec<Line<'_>> = Vec::new();
    for (section, entries) in &screen_specific {
        lines.push(Line::from(Span::styled(*section, theme::label())));
        lines.push(Line::from(""));
        for (key, desc) in *entries {
            lines.push(help_row(key, desc));
        }
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled("GLOBAL", theme::label())));
    lines.push(Line::from(""));
    for (key, desc) in global {
        lines.push(help_row(key, desc));
    }
    let para = Paragraph::new(lines).style(theme::base().bg(theme::BG_RAISED));
    frame.render_widget(para, inner);
}

fn help_row(key: &str, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {key:<18}", key = key),
            Style::default().fg(theme::ACCENT).bold(),
        ),
        Span::styled(desc.to_string(), theme::dim()),
    ])
}

/// Footer hint line: accent-bold key + dim label, separated by `   `.
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
        Span::styled(format!("{label:<20}", label = label), theme::label()),
        Span::raw(" "),
        Span::styled(head.to_string(), body_style),
        Span::styled(caret_char, caret_style),
        Span::styled(after, body_style),
    ])
}
