use std::io::{self, Write};
use std::time::Duration;

use anyhow::Result;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use inquire::{CustomType, InquireError, Select};

use crate::actions::{delete_profile, edit_profile, is_cancelled, rename_profile, switch_profile};
use crate::fallback::{DEFAULT_THRESHOLD, threshold_for};
use crate::profile::{AppConfig, save_app_state, save_profile};
use crate::ui::{
    C_ACCENT, C_BOLD, C_DANGER, C_DIM, C_FAINT, C_FG_OFF, C_NOBOLD, C_ORANGE, C_RESET,
    endpoint_visible_width, format_profile_entry, format_submenu_title, visible_width,
    weekly_bar_visible_width,
};

// ── Profile submenu ───────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum SubmenuAction {
    Switch,
    Edit,
    Rename,
    Delete,
    Back,
}

impl SubmenuAction {
    fn label(self, is_active: bool) -> String {
        match self {
            Self::Switch if is_active => {
                format!("Switch to this profile{C_NOBOLD}  {C_FAINT}(already active){C_RESET}")
            }
            Self::Switch => "Switch to this profile".to_string(),
            Self::Edit => format!("Edit{C_NOBOLD}  {C_DIM}(URL / API key){C_RESET}"),
            Self::Rename => "Rename".to_string(),
            Self::Delete => format!("{C_DANGER}Delete{C_RESET}"),
            Self::Back => format!("{C_FAINT}← Back{C_RESET}"),
        }
    }
}

pub(crate) fn profile_submenu(config: &mut AppConfig, profile_name: &str) -> Result<()> {
    use SubmenuAction::*;
    const ACTIONS: [SubmenuAction; 5] = [Switch, Edit, Rename, Delete, Back];

    let start_row = crossterm::cursor::position().map(|(_, r)| r).ok();

    loop {
        if let Some(row) = start_row {
            let _ = execute!(
                io::stdout(),
                MoveTo(0, row),
                Clear(ClearType::FromCursorDown)
            );
        }

        let (title, is_active) = match config.find(profile_name) {
            Some(p) => (format_submenu_title(p), config.is_active(profile_name)),
            None => return Ok(()),
        };

        let labels: Vec<String> = ACTIONS.iter().map(|a| a.label(is_active)).collect();

        let idx = match Select::new(&title, labels)
            .without_filtering()
            .without_help_message()
            .raw_prompt()
        {
            Ok(opt) => opt.index,
            Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        let result: Result<bool> = match ACTIONS[idx] {
            Switch => switch_profile(config, profile_name).map(|_| true),
            Edit => edit_profile(config, profile_name).map(|_| false),
            Rename => rename_profile(config, profile_name),
            Delete => delete_profile(config, profile_name),
            Back => return Ok(()),
        };

        match result {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) if is_cancelled(&e) => {}
            Err(e) => return Err(e),
        }
    }
}

// ── Main menu ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
pub(crate) enum MainAction {
    Profile(usize),
    NewBlank,
    Capture,
    FallbackChain,
    Quit,
}

pub(crate) enum MainMenuResult {
    Selected(usize),
    Cancelled,
}

struct RawModeGuard {
    row: u16,
}

impl RawModeGuard {
    fn new(lines_needed: u16) -> Result<Self> {
        let mut out = io::stdout();
        let (_, rows) = terminal::size()?;
        let (_, mut row) = crossterm::cursor::position()?;

        let shift = (row + lines_needed).saturating_sub(rows);
        if shift > 0 {
            // `\n` only scrolls when the cursor is on the last row; otherwise
            // it just moves the cursor down and we end up rendering over
            // existing history. Park on the bottom row first so each newline
            // pushes a line into scrollback.
            execute!(out, MoveTo(0, rows.saturating_sub(1)))?;
            write!(out, "{}", "\n".repeat(shift as usize))?;
            out.flush()?;
            row = row.saturating_sub(shift);
        }

        terminal::enable_raw_mode()?;
        execute!(out, MoveTo(0, row), Hide)?;
        Ok(Self { row })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = execute!(
            out,
            MoveTo(0, self.row),
            Clear(ClearType::FromCursorDown),
            Show
        );
        let _ = terminal::disable_raw_mode();
    }
}

fn render_main_menu(labels: &[String], selected: usize, row: u16) -> Result<()> {
    let mut out = io::stdout();
    queue!(out, MoveTo(0, row), Clear(ClearType::FromCursorDown))?;
    queue!(out, Print(format!("{C_ACCENT}?{C_RESET} clauth")))?;

    for (i, label) in labels.iter().enumerate() {
        queue!(out, MoveTo(0, row + i as u16 + 1))?;
        if i == selected {
            queue!(
                out,
                Print(format!("{C_ORANGE}▶{C_RESET} {C_BOLD}{label}{C_RESET}"))
            )?;
        } else {
            queue!(out, Print(format!("  {label}")))?;
        }
    }

    out.flush()?;
    Ok(())
}

pub(crate) fn main_menu_prompt(
    mut labels: Vec<String>,
    mut refresh: impl FnMut() -> Vec<String>,
) -> Result<MainMenuResult> {
    if labels.is_empty() {
        return Ok(MainMenuResult::Cancelled);
    }

    let lines_needed = labels.len() as u16 + 1;
    let raw_mode = RawModeGuard::new(lines_needed)?;

    let mut selected = 0;
    render_main_menu(&labels, selected, raw_mode.row)?;

    loop {
        if !event::poll(Duration::from_millis(500))? {
            let refreshed = refresh();
            if !refreshed.is_empty() {
                labels = refreshed;
                selected = selected.min(labels.len() - 1);
                render_main_menu(&labels, selected, raw_mode.row)?;
            }
            continue;
        }

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                selected = if selected == 0 {
                    labels.len() - 1
                } else {
                    selected - 1
                };
                render_main_menu(&labels, selected, raw_mode.row)?;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1) % labels.len();
                render_main_menu(&labels, selected, raw_mode.row)?;
            }
            KeyCode::Home => {
                selected = 0;
                render_main_menu(&labels, selected, raw_mode.row)?;
            }
            KeyCode::End => {
                selected = labels.len() - 1;
                render_main_menu(&labels, selected, raw_mode.row)?;
            }
            KeyCode::Enter => return Ok(MainMenuResult::Selected(selected)),
            KeyCode::Esc | KeyCode::Char('q' | 'Q') => {
                return Ok(MainMenuResult::Cancelled);
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(MainMenuResult::Cancelled);
            }
            _ => {}
        }
    }
}

pub(crate) fn build_main_menu(config: &AppConfig) -> Vec<(String, MainAction)> {
    let name_width = config
        .profiles
        .iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(0)
        .max(4);

    // Align bars to the widest endpoint among profiles that actually render
    // one. Profiles without a fetched 5-hour window don't push the column.
    let endpoint_width = config
        .profiles
        .iter()
        .filter(|p| {
            p.usage
                .as_ref()
                .and_then(|u| u.five_hour.as_ref())
                .is_some()
        })
        .map(endpoint_visible_width)
        .max()
        .unwrap_or(0);

    let cols = terminal::size().map(|(c, _)| c as usize).unwrap_or(80);
    let max_base_width = config
        .profiles
        .iter()
        .map(|p| {
            visible_width(&format_profile_entry(
                p,
                config.is_active(&p.name),
                name_width,
                endpoint_width,
                false,
            ))
        })
        .max()
        .unwrap_or(0);
    let max_weekly_width = config
        .profiles
        .iter()
        .map(weekly_bar_visible_width)
        .max()
        .unwrap_or(0);
    let show_weekly = max_weekly_width > 0 && max_base_width + max_weekly_width <= cols;

    let mut items = Vec::with_capacity(config.profiles.len() + 3);

    for (i, p) in config.profiles.iter().enumerate() {
        items.push((
            format_profile_entry(
                p,
                config.is_active(&p.name),
                name_width,
                endpoint_width,
                show_weekly,
            ),
            MainAction::Profile(i),
        ));
    }
    items.push((
        format!("{C_ORANGE}+{C_FG_OFF} New profile"),
        MainAction::NewBlank,
    ));
    items.push((
        format!("{C_ORANGE}+{C_FG_OFF} New from current profile"),
        MainAction::Capture,
    ));
    let chain_len = config.state.fallback_chain.len();
    let chain_hint = if chain_len == 0 {
        format!("{C_FAINT}empty{C_RESET}")
    } else {
        format!(
            "{C_DIM}{chain_len} profile{}{C_RESET}",
            if chain_len == 1 { "" } else { "s" }
        )
    };
    items.push((
        format!("{C_ACCENT}⇄{C_FG_OFF} Fallback chain  {chain_hint}"),
        MainAction::FallbackChain,
    ));
    items.push((format!("{C_FAINT}Quit{C_RESET}"), MainAction::Quit));

    items
}

// ── Fallback chain editor ─────────────────────────────────────────────────────

fn five_hour_pct(config: &AppConfig, name: &str) -> Option<f64> {
    config
        .find(name)?
        .usage
        .as_ref()?
        .five_hour
        .as_ref()
        .map(|w| w.utilization)
}

fn chain_member_label(config: &AppConfig, position: usize, name: &str) -> String {
    let threshold = config
        .find(name)
        .map(threshold_for)
        .unwrap_or(DEFAULT_THRESHOLD);
    let usage = match five_hour_pct(config, name) {
        Some(pct) => {
            let color = if pct >= threshold {
                C_DANGER
            } else if pct >= threshold * 0.8 {
                C_ORANGE
            } else {
                C_DIM
            };
            format!("{color}5h {pct:.0}%{C_RESET}")
        }
        None => format!("{C_FAINT}5h n/a{C_RESET}"),
    };
    let active = if config.is_active(name) {
        format!("{C_ACCENT}● {C_RESET}")
    } else {
        "  ".to_string()
    };
    format!(
        "{C_FAINT}{position:>2}.{C_RESET} {active}{C_BOLD}{name}{C_RESET}  {C_FAINT}@ {threshold:.0}%{C_RESET}  {usage}"
    )
}

#[derive(Clone, Copy)]
enum ChainItemAction {
    Threshold,
    MoveUp,
    MoveDown,
    Remove,
    Back,
}

fn chain_item_submenu(config: &mut AppConfig, name: &str) -> Result<()> {
    use ChainItemAction::*;

    loop {
        let position = match config.state.fallback_chain.iter().position(|n| n == name) {
            Some(p) => p,
            None => return Ok(()),
        };
        let chain_len = config.state.fallback_chain.len();
        let current = config
            .find(name)
            .map(threshold_for)
            .unwrap_or(DEFAULT_THRESHOLD);

        let mut actions: Vec<(String, ChainItemAction)> = Vec::new();
        actions.push((
            format!("Set threshold  {C_FAINT}(current: {current:.0}%){C_RESET}"),
            Threshold,
        ));
        if position > 0 {
            actions.push(("Move up".to_string(), MoveUp));
        }
        if position + 1 < chain_len {
            actions.push(("Move down".to_string(), MoveDown));
        }
        actions.push((format!("{C_DANGER}Remove from chain{C_RESET}"), Remove));
        actions.push((format!("{C_FAINT}← Back{C_RESET}"), Back));

        let title = format!("{C_BOLD}{name}{C_RESET}{C_FAINT} · fallback chain{C_RESET}");
        let labels: Vec<String> = actions.iter().map(|(l, _)| l.clone()).collect();
        let idx = match Select::new(&title, labels)
            .without_filtering()
            .without_help_message()
            .raw_prompt()
        {
            Ok(opt) => opt.index,
            Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        match actions[idx].1 {
            Threshold => {
                let entered: Result<f64, _> = CustomType::<f64>::new("Threshold percentage:")
                    .with_default(current)
                    .with_help_message(
                        "Auto-switch off this profile at this 5h utilization (0..=100).",
                    )
                    .with_error_message("Enter a number between 0 and 100.")
                    .with_validator(|v: &f64| {
                        if (0.0..=100.0).contains(v) {
                            Ok(inquire::validator::Validation::Valid)
                        } else {
                            Ok(inquire::validator::Validation::Invalid(
                                "Threshold must be between 0 and 100.".into(),
                            ))
                        }
                    })
                    .prompt()
                    .map_err(anyhow::Error::from);
                let value = match entered {
                    Ok(v) => v,
                    Err(e) if is_cancelled(&e) => continue,
                    Err(e) => return Err(e),
                };
                if let Some(profile) = config.find_mut(name) {
                    profile.fallback_threshold = Some(value);
                    save_profile(profile)?;
                }
            }
            MoveUp => {
                config.state.fallback_chain.swap(position - 1, position);
                save_app_state(&config.state)?;
            }
            MoveDown => {
                config.state.fallback_chain.swap(position, position + 1);
                save_app_state(&config.state)?;
            }
            Remove => {
                config.state.fallback_chain.retain(|n| n != name);
                save_app_state(&config.state)?;
                return Ok(());
            }
            Back => return Ok(()),
        }
    }
}

fn add_to_chain_prompt(config: &mut AppConfig) -> Result<()> {
    let candidates: Vec<String> = config
        .profiles
        .iter()
        .map(|p| p.name.clone())
        .filter(|n| !config.state.fallback_chain.iter().any(|c| c == n))
        .collect();
    if candidates.is_empty() {
        return Ok(());
    }

    let title = format!("{C_BOLD}Add profile to chain{C_RESET}");
    let selection = Select::new(&title, candidates.clone())
        .without_filtering()
        .without_help_message()
        .raw_prompt();
    let idx = match selection {
        Ok(opt) => opt.index,
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let name = candidates[idx].clone();

    if let Some(profile) = config.find_mut(&name)
        && profile.fallback_threshold.is_none()
    {
        profile.fallback_threshold = Some(DEFAULT_THRESHOLD);
        save_profile(profile)?;
    }
    config.state.fallback_chain.push(name);
    save_app_state(&config.state)?;
    Ok(())
}

pub(crate) fn fallback_chain_menu(config: &mut AppConfig) -> Result<()> {
    let start_row = crossterm::cursor::position().map(|(_, r)| r).ok();

    loop {
        if let Some(row) = start_row {
            let _ = execute!(
                io::stdout(),
                MoveTo(0, row),
                Clear(ClearType::FromCursorDown)
            );
        }

        let chain = config.state.fallback_chain.clone();
        let mut entries: Vec<(String, Option<String>)> = Vec::new();
        for (i, name) in chain.iter().enumerate() {
            entries.push((chain_member_label(config, i + 1, name), Some(name.clone())));
        }

        let any_unchained = config
            .profiles
            .iter()
            .any(|p| !chain.iter().any(|c| c == &p.name));
        if any_unchained {
            entries.push((format!("{C_ORANGE}+{C_FG_OFF} Add profile to chain"), None));
        }
        entries.push((format!("{C_FAINT}← Back{C_RESET}"), Some(String::new())));

        let title = if chain.is_empty() {
            format!(
                "{C_BOLD}Fallback chain{C_RESET}{C_FAINT} · empty — add a profile to enable auto-switch{C_RESET}"
            )
        } else {
            format!(
                "{C_BOLD}Fallback chain{C_RESET}{C_FAINT} · {} profile{}{C_RESET}",
                chain.len(),
                if chain.len() == 1 { "" } else { "s" }
            )
        };
        let labels: Vec<String> = entries.iter().map(|(l, _)| l.clone()).collect();
        let idx = match Select::new(&title, labels)
            .without_filtering()
            .without_help_message()
            .raw_prompt()
        {
            Ok(opt) => opt.index,
            Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        match entries[idx].1.clone() {
            None => add_to_chain_prompt(config)?,
            Some(s) if s.is_empty() => return Ok(()),
            Some(name) => {
                if let Err(e) = chain_item_submenu(config, &name)
                    && !is_cancelled(&e)
                {
                    return Err(e);
                }
            }
        }
    }
}
