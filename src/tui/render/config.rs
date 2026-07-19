//! Setup tab — account picker (plus a trailing `+ new` row) on the left, the
//! selected account's settings on the right. Editing happens inline in the
//! right pane: ⏎ on the left drops focus into the detail rows, ⏎ on a text row
//! opens an inline caret, ⏎ on a toggle flips it, and `+ new` turns the right
//! pane into a create form. No popups.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::app::{
    App, ConfigDraft, ConfigFocus, ConfigRow, InputState, MODEL_PRESETS, config_rows,
};
use super::super::theme;
use super::panes::{
    bold_when, cycle_option, draw_scrolled_lines, draw_selector_list, head_cols,
    help_tooltip_lines, highlight_row, key_cell, label_style, master_detail, name_color,
    picker_row, section_box, section_box_verbatim,
};

const KEY_W: usize = 11;
/// Fixed gap between the padded key and the value column (house standard).
const KEY_GUTTER: usize = 2;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // +1 for the trailing `+ new` picker row.
    let items = app.config().profiles.len() + 1;
    let (selector, settings) = master_detail(area, items);

    let profiles_focused = app.config_focus == ConfigFocus::Profiles;
    draw_selector(frame, selector, app, profiles_focused);
    draw_settings(frame, settings, app);
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
    model: String,
    opus: String,
    sonnet: String,
    haiku: String,
    subagent: String,
    /// Sorted `(key, value)` custom env entries — one `EnvEntry` row each.
    env: Vec<(String, String)>,
    auto_start: bool,
    /// Whether the profile holds a stored credential — the OAuth token or, for an
    /// API account, the api key. Drives the `Login` row's re-login vs first-login
    /// label and the `DeleteCreds` row's presence.
    logged_in: bool,
    /// Credential typing for the login / log-out rows (`Profile::login_is_oauth`,
    /// so a hybrid's stored pair wins over its base url). Endpoint-shaped rows
    /// keep tracking the base-url buffer instead.
    login_is_oauth: bool,
    /// `+ new` form only: the draft holds a minted login awaiting `create
    /// account` — flips the `Login` row to its `✓ logged in` state.
    captured: bool,
    /// Recognised third-party provider display name, if any.
    provider: Option<&'static str>,
    /// CLA-SPLIT sidecar state (`claude::session_token_status`): `None` = no
    /// sidecar; long-lived with its stamped horizon, or the mis-filled
    /// not-long-lived shape the split disengages for. Read per frame for the
    /// selected profile only (one small file).
    session_token: Option<crate::claude::SessionTokenStatus>,
}

impl Snap {
    /// Blank snapshot for the `+ new` form and the empty fallback.
    fn blank(title: &str) -> Snap {
        Snap {
            title: title.to_string(),
            name: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            opus: String::new(),
            sonnet: String::new(),
            haiku: String::new(),
            subagent: String::new(),
            env: Vec::new(),
            auto_start: false,
            logged_in: false,
            login_is_oauth: true,
            captured: false,
            provider: None,
            session_token: None,
        }
    }
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
        let mut snap = Snap::blank("+ new account");
        // Mirror commit_new_account's consume rule: a typed base url flips the
        // form to API mode and the mint will be discarded, so no stale ✓.
        snap.captured = app
            .config_draft
            .as_ref()
            .is_some_and(|d| d.captured_login.is_some() && d.base_url.value.trim().is_empty());
        return snap;
    }
    match cfg.profiles.get(app.profile_cursor) {
        Some(p) => Snap {
            title: p.name.to_string(),
            name: if with_text {
                p.name.to_string()
            } else {
                String::new()
            },
            base_url: text(&p.base_url),
            api_key: text(&p.api_key),
            model: text(&p.models.default),
            opus: text(&p.models.opus),
            sonnet: text(&p.models.sonnet),
            haiku: text(&p.models.haiku),
            subagent: text(&p.models.subagent),
            // Env rows render from the snapshot (no per-entry draft buffer), so
            // they're always populated — even while a draft owns the text fields.
            env: p.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            auto_start: p.auto_start,
            // OAuth accounts carry a token; API accounts carry an api key. Either
            // one flips the Login row to "re-login" and shows the log-out row.
            logged_in: if p.login_is_oauth() {
                p.credentials.is_some()
            } else {
                p.api_key.as_deref().is_some_and(|k| !k.trim().is_empty())
            },
            login_is_oauth: p.login_is_oauth(),
            captured: false,
            provider: p.provider.map(|p| p.display_name()),
            session_token: crate::claude::session_token_status(p.name.as_str()),
        },
        None => Snap::blank("settings"),
    }
}

fn draw_settings(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let actions_focused = app.config_focus == ConfigFocus::Actions;
    let draft = app.config_draft.as_ref();
    let snap = build_snap(app, draft.is_none());

    // Profile names render verbatim; structural titles ("+ new account", "settings") stay uppercased.
    let is_profile_name = app.profile_cursor < app.config().profiles.len();
    let block = if is_profile_name {
        section_box_verbatim(&snap.title, actions_focused, false)
    } else {
        section_box(&snap.title, actions_focused, false)
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = config_rows(app);
    let cursor = app.config_action_cursor.min(rows.len().saturating_sub(1));

    draw_settings_rows(frame, inner, app, &rows, cursor, &snap, actions_focused);
}

/// CLA-SPLIT status row: a profile carrying a long-lived-token sidecar runs
/// its sessions on that static login, and the ~1yr horizon is the one thing
/// about it worth watching — expired means every switch would sign sessions
/// out, so it escalates to a DANGER re-mint hint (the same wording
/// `ensure_installable` logs); the last 30 days warn. A mis-filled sidecar
/// (rotating pair, split disengaged) reads as exactly that, in DANGER — the
/// operator thinks the split is armed and it isn't.
fn session_token_line(status: &crate::claude::SessionTokenStatus, now_ms: i64) -> Line<'static> {
    use crate::claude::SessionTokenStatus;
    let (text, style) = match status {
        SessionTokenStatus::LongLived(Some(ms)) => {
            if now_ms >= *ms {
                (
                    "expired · re-mint: claude setup-token".to_string(),
                    theme::danger(),
                )
            } else {
                // Truncating division: an expiry inside the next 24h reads
                // "~0d" and still warns; only a past expiry (handled above)
                // is DANGER, so a sub-day-expired token no longer mislabels as
                // "~0d / warning" while the install gate already refuses it.
                let days = (ms - now_ms) / 86_400_000;
                if days <= 30 {
                    (
                        format!("long-lived · expires in ~{days}d"),
                        theme::warning(),
                    )
                } else {
                    (
                        format!("long-lived · expires in ~{days}d"),
                        theme::accent(),
                    )
                }
            }
        }
        SessionTokenStatus::LongLived(None) => (
            "long-lived · no recorded expiry".to_string(),
            theme::accent(),
        ),
        SessionTokenStatus::NotLongLived => (
            "not long-lived (has a refresh token) · ignored".to_string(),
            theme::danger(),
        ),
    };
    Line::from(vec![
        Span::styled(key_cell("token", KEY_W, KEY_GUTTER), theme::label()),
        Span::styled(text, style),
    ])
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
    let editing = draft.and_then(|d| d.active);
    let armed_delete = draft.map(|d| d.armed_delete).unwrap_or(false);

    // Derived from the base-url buffer so it tracks the draft live.
    let is_api = !row_input(draft, snap, ConfigRow::BaseUrl)
        .value
        .trim()
        .is_empty();
    let (type_value, type_style) = if is_api {
        ("api", theme::accent())
    } else {
        ("oauth", theme::accent())
    };

    let mut lines: Vec<Line<'static>> = vec![Line::from(vec![
        Span::styled(key_cell("type", KEY_W, KEY_GUTTER), theme::label()),
        Span::styled(type_value, type_style),
    ])];

    // Provider row — only for recognised third-party providers. Hidden while a
    // draft empties the base-url buffer (`is_api` tracks the draft live).
    let provider_label = if is_api { snap.provider } else { None };
    if let Some(label) = provider_label {
        lines.push(Line::from(vec![
            Span::styled(key_cell("provider", KEY_W, KEY_GUTTER), theme::label()),
            Span::styled(label, theme::accent()),
        ]));
    }

    if let Some(status) = &snap.session_token {
        lines.push(session_token_line(status, crate::usage::now_ms() as i64));
    }

    lines.push(Line::from(""));
    // Tracks the absolute line index + buffer + row of the active edit row for
    // cursor placement after rendering. The header block above is variable
    // (type + optional provider + optional session + blank), so the row loop's
    // base index is simply what has been pushed so far.
    let mut edit_caret: Option<(u16, InputState, ConfigRow)> = None;
    let mut line_idx: u16 = lines.len() as u16;

    // Start + end of the focused row's block (row plus its tooltip lines), so a
    // wrapped hint can't scroll off the bottom while its row stays visible.
    let mut focus = (0usize, 1usize);
    for (i, row) in rows.iter().enumerate() {
        let selected = actions_focused && i == cursor;
        let is_editing = editing == Some(*row);
        let input = row_input(draft, snap, *row);
        let line = detail_row(*row, selected, is_editing, armed_delete, snap, &input);
        if is_editing {
            edit_caret = Some((line_idx, input, *row));
        }
        if selected || is_editing {
            focus.0 = line_idx as usize;
        }
        lines.push(if selected {
            highlight_row(line, inner.width as usize)
        } else {
            line
        });
        line_idx += 1;
        if selected
            && !is_editing
            && let Some(text) = row_hint(*row, snap)
        {
            let hint = help_tooltip_lines(&text, inner.width as usize);
            line_idx += hint.len() as u16;
            lines.extend(hint);
        }
        if selected || is_editing {
            focus.1 = line_idx as usize;
        }
    }

    // The row list outgrows a short terminal (env entries + model overrides are
    // unbounded), so it scrolls to the focused row rather than clipping its tail.
    let offset = draw_scrolled_lines(frame, inner, lines, focus);

    // Position the native terminal cursor at the caret when a text/model field is active.
    if let Some((ly, input, row)) = edit_caret
        && let Some(visible) = (ly as usize)
            .checked_sub(offset)
            .filter(|v| *v < inner.height as usize)
    {
        // x = "❯ " (2) + label block (row_label_cols: KEY_W+gutter, or key+gutter for a long env key) + caret cols
        let prefix_cols = 2 + row_label_cols(row, snap) + head_cols(&input);
        let cx = inner.x.saturating_add(prefix_cols as u16);
        let cy = inner.y.saturating_add(visible as u16);
        frame.set_cursor_position((cx, cy));
    }
}

/// Width of a row's label block (caret excluded) for native-cursor placement:
/// the shared key-cell width (`max(KEY_W, key.len()) + KEY_GUTTER`), mirroring
/// [`kv_field`] so the caret lands right after the gap.
fn row_label_cols(row: ConfigRow, snap: &Snap) -> usize {
    match row {
        ConfigRow::EnvEntry(i) => {
            let key_len = snap.env.get(i).map(|(k, _)| k.chars().count()).unwrap_or(0);
            KEY_W.max(key_len) + KEY_GUTTER
        }
        _ => KEY_W + KEY_GUTTER,
    }
}

/// The edit buffer for a row: the live draft buffer when present, else a
/// throwaway `InputState` seeded from the read-only [`Snap`]. Toggle/action rows
/// have no buffer and resolve to an empty one (never rendered as a field).
fn row_input(draft: Option<&ConfigDraft>, snap: &Snap, row: ConfigRow) -> InputState {
    draft
        .and_then(|d| d.field(row))
        .cloned()
        .unwrap_or_else(|| InputState::new(snap_value(snap, row)))
}

fn snap_value(snap: &Snap, row: ConfigRow) -> &str {
    match row {
        ConfigRow::Name => &snap.name,
        ConfigRow::BaseUrl => &snap.base_url,
        ConfigRow::ApiKey => &snap.api_key,
        ConfigRow::Model => &snap.model,
        ConfigRow::OpusModel => &snap.opus,
        ConfigRow::SonnetModel => &snap.sonnet,
        ConfigRow::HaikuModel => &snap.haiku,
        ConfigRow::SubagentModel => &snap.subagent,
        ConfigRow::EnvEntry(i) => snap.env.get(i).map(|(_, v)| v.as_str()).unwrap_or(""),
        ConfigRow::EnvAdd
        | ConfigRow::ModelOverrideAdd
        | ConfigRow::AutoStart
        | ConfigRow::Login
        | ConfigRow::DeleteCreds
        | ConfigRow::Delete
        | ConfigRow::Create => "",
    }
}

/// Inline help for rows whose labels don't self-describe, phrased for the row's
/// current value so it re-explains itself as the value changes. `login_is_oauth`
/// (not the base-url buffer) picks the login/log-out wording — the copy has to
/// name what ⏎ really does — while `auto_start` / `base_url` flip on their own
/// value.
fn row_hint(row: ConfigRow, snap: &Snap) -> Option<String> {
    let api_login = !snap.login_is_oauth;
    let hint = match row {
        ConfigRow::BaseUrl if snap.base_url.trim().is_empty() => {
            "leave empty for a claude.ai account, or set an api endpoint"
        }
        ConfigRow::BaseUrl => "the api endpoint this account calls instead of claude.ai",
        ConfigRow::ApiKey => "api key sent to the endpoint above",
        // The value grammar (`space cycle · ↵ custom`) already lives in the footer.
        ConfigRow::Model => "default model for this account",
        ConfigRow::OpusModel => "what the opus alias resolves to (full model id)",
        ConfigRow::SonnetModel => "what the sonnet alias resolves to (full model id)",
        ConfigRow::HaikuModel => "what the haiku alias resolves to (full model id)",
        ConfigRow::SubagentModel => "model forced for every subagent in this account",
        ConfigRow::EnvEntry(_) => "env var set for claude code while this account is active",
        ConfigRow::EnvAdd => "add an env var for this account",
        ConfigRow::AutoStart if snap.auto_start => {
            "starts a throwaway session when idle so the 5h window counts"
        }
        ConfigRow::AutoStart => "never starts a session on its own",
        ConfigRow::ModelOverrideAdd => "pin what an alias resolves to, or force the subagent model",
        ConfigRow::Login if api_login => "re-enter the base url + api key for this account",
        ConfigRow::Login => "browser OAuth login; mints fresh tokens for this account",
        ConfigRow::DeleteCreds if api_login => {
            "clears the stored api key; keeps the account and its settings"
        }
        ConfigRow::DeleteCreds => {
            "clears the stored OAuth login; keeps the account and its settings"
        }
        ConfigRow::Delete => {
            "deletes the account and everything stored for it, usage history included"
        }
        ConfigRow::Name | ConfigRow::Create => return None,
    };
    Some(hint.to_string())
}

fn detail_row(
    row: ConfigRow,
    selected: bool,
    editing: bool,
    armed_delete: bool,
    snap: &Snap,
    input: &InputState,
) -> Line<'static> {
    let arrow = if editing {
        Span::styled(format!("{} ", theme::edit_glyph()), theme::accent())
    } else if selected {
        Span::styled("❯ ", theme::accent())
    } else {
        Span::raw("  ")
    };
    match row {
        ConfigRow::Name => kv_field(arrow, "name", input, editing, selected, false),
        ConfigRow::BaseUrl => kv_field(arrow, "base url", input, editing, selected, false),
        ConfigRow::ApiKey => kv_field(arrow, "api key", input, editing, selected, true),
        // Hybrid: the alias cycle at rest, a plain text field while typing a custom id.
        ConfigRow::Model if !editing => model_cycle_line(arrow, &input.value, selected),
        ConfigRow::Model => kv_field(arrow, "model", input, editing, selected, false),
        ConfigRow::OpusModel => kv_field(arrow, "opus", input, editing, selected, false),
        ConfigRow::SonnetModel => kv_field(arrow, "sonnet", input, editing, selected, false),
        ConfigRow::HaikuModel => kv_field(arrow, "haiku", input, editing, selected, false),
        ConfigRow::SubagentModel => kv_field(arrow, "subagent", input, editing, selected, false),
        // A custom env entry: its key is the label; mask the value when the key
        // looks like a credential (mirrors the api-key row).
        ConfigRow::EnvEntry(i) => {
            let key = snap.env.get(i).map(|(k, _)| k.clone()).unwrap_or_default();
            let mask = env_key_is_secret(&key);
            kv_field(arrow, &key, input, editing, selected, mask)
        }
        // While editing, the typed text is the new key; at rest, the add chip.
        ConfigRow::EnvAdd if editing => kv_field(arrow, "key", input, editing, selected, false),
        ConfigRow::EnvAdd => Line::from(vec![
            arrow,
            Span::styled("+ add env", bold_when(theme::accent(), selected)),
        ]),
        ConfigRow::ModelOverrideAdd => Line::from(vec![
            arrow,
            Span::styled("+ model override", bold_when(theme::accent(), selected)),
        ]),
        ConfigRow::AutoStart => {
            let (value, style) = if snap.auto_start {
                (theme::toggle_on().to_string(), theme::accent())
            } else {
                (theme::toggle_off().to_string(), theme::faint())
            };
            kv_static(arrow, "auto-start", value, style, selected)
        }
        ConfigRow::Delete => {
            let label = if armed_delete {
                "press again to delete".to_string()
            } else {
                "delete account".to_string()
            };
            Line::from(vec![arrow, Span::styled(label, theme::danger().bold())])
        }
        ConfigRow::Create => Line::from(vec![
            arrow,
            Span::styled("create account", bold_when(theme::accent(), selected)),
        ]),
        ConfigRow::Login => {
            // A draft-held mint renders the done state; ⏎ re-runs the login but
            // confirms first before replacing the stash.
            if snap.captured {
                Line::from(vec![
                    arrow,
                    Span::styled("✓ logged in", bold_when(theme::success(), selected)),
                ])
            } else {
                let label = if snap.logged_in {
                    "re-login"
                } else {
                    "+ login"
                };
                Line::from(vec![
                    arrow,
                    Span::styled(label, bold_when(theme::accent(), selected)),
                ])
            }
        }
        ConfigRow::DeleteCreds => {
            Line::from(vec![arrow, Span::styled("log out", theme::danger().bold())])
        }
    }
}

fn kv_field(
    arrow: Span<'static>,
    key: &str,
    input: &InputState,
    editing: bool,
    focused: bool,
    mask_value: bool,
) -> Line<'static> {
    let mut spans = vec![
        arrow,
        Span::styled(key_cell(key, KEY_W, KEY_GUTTER), label_style(focused)),
    ];
    spans.extend(value_spans(input, editing, mask_value));
    Line::from(spans)
}

fn kv_static(
    arrow: Span<'static>,
    key: &str,
    value: String,
    value_style: Style,
    focused: bool,
) -> Line<'static> {
    Line::from(vec![
        arrow,
        Span::styled(key_cell(key, KEY_W, KEY_GUTTER), label_style(focused)),
        Span::styled(value, value_style),
    ])
}

/// Mask a custom env value when its key names a credential (mirrors the api-key row).
fn env_key_is_secret(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "AUTH"]
        .iter()
        .any(|needle| upper.contains(needle))
}

fn value_spans(input: &InputState, editing: bool, mask_value: bool) -> Vec<Span<'static>> {
    if !editing {
        if input.value.is_empty() {
            return vec![Span::styled("—", theme::faint())];
        }
        let display = if mask_value {
            "••••••••".to_string()
        } else {
            input.value.clone()
        };
        return vec![Span::styled(display, theme::accent())];
    }
    // In edit mode the terminal cursor (set via frame.set_cursor_position) owns
    // the caret glyph — no simulated block highlight needed.
    let body = Style::default()
        .fg(theme::text_color())
        .bg(theme::bg_sunken());
    vec![Span::styled(input.value.clone(), body)]
}

/// The `model` row at rest: a segmented alias control (`default` + presets).
/// The active option is `ACCENT` and wraps in `[]` only while the row is the
/// cursor (the row widens by 2 on focus — the Config-tab focus cue); the rest
/// stay bare `TEXT_FAINT`. A custom id (set via ⏎) matches no preset, so the
/// real value is appended in `ACCENT` rather than mis-bracketing the nearest
/// alias.
fn model_cycle_line(arrow: Span<'static>, current: &str, selected: bool) -> Line<'static> {
    let mut spans = vec![
        arrow,
        Span::styled(key_cell("model", KEY_W, KEY_GUTTER), label_style(selected)),
    ];
    let mut options: Vec<(&str, bool)> = vec![("default", current.is_empty())];
    options.extend(MODEL_PRESETS.iter().map(|p| (*p, *p == current)));
    for (i, (label, active)) in options.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(cycle_option(label, *active, selected));
    }
    if !current.is_empty() && !MODEL_PRESETS.contains(&current) {
        spans.push(Span::styled(format!("   {current}"), theme::accent()));
    }
    Line::from(spans)
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_config.rs"]
mod tests;
