use std::io::{self, Write};
use std::time::Duration;

use anyhow::Result;
use crossterm::cursor::{Hide, MoveToColumn, RestorePosition, SavePosition, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use inquire::{InquireError, Select};

use crate::actions::{delete_profile, edit_profile, is_cancelled, rename_profile, switch_profile};
use crate::profile::AppConfig;
use crate::ui::{
    C_ACCENT, C_BOLD, C_DANGER, C_DIM, C_FAINT, C_FG_OFF, C_NOBOLD, C_ORANGE, C_RESET,
    format_profile_entry, format_submenu_title,
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

    loop {
        let (title, is_active) = match config.find(profile_name) {
            Some(p) => (format_submenu_title(p), config.is_active(profile_name)),
            None => return Ok(()),
        };

        let labels: Vec<String> = ACTIONS.iter().map(|a| a.label(is_active)).collect();

        let idx = match Select::new(&title, labels).without_filtering().raw_prompt() {
            Ok(opt) => opt.index,
            Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        let result: Result<bool> = match ACTIONS[idx] {
            Switch => {
                switch_profile(config, profile_name)?;
                std::process::exit(0);
            }
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
    Quit,
}

pub(crate) enum MainMenuResult {
    Selected(usize),
    Cancelled,
}

struct RawModeGuard;

impl RawModeGuard {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), Hide)?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = execute!(out, RestorePosition, Clear(ClearType::FromCursorDown), Show);
        let _ = terminal::disable_raw_mode();
    }
}

fn render_main_menu(labels: &[String], selected: usize) -> Result<()> {
    let mut out = io::stdout();
    queue!(out, RestorePosition, Clear(ClearType::FromCursorDown))?;
    queue!(
        out,
        MoveToColumn(0),
        Print(format!("{C_ACCENT}?{C_RESET} clauth\r\n"))
    )?;

    for (i, label) in labels.iter().enumerate() {
        queue!(out, MoveToColumn(0))?;
        if i == selected {
            queue!(
                out,
                Print(format!("{C_ORANGE}▶{C_RESET} {C_BOLD}{label}{C_RESET}\r\n"))
            )?;
        } else {
            queue!(out, Print(format!("  {label}\r\n")))?;
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

    let _raw_mode = RawModeGuard::new()?;
    execute!(io::stdout(), SavePosition)?;

    let mut selected = 0;
    render_main_menu(&labels, selected)?;

    loop {
        if !event::poll(Duration::from_millis(500))? {
            let refreshed = refresh();
            if !refreshed.is_empty() {
                labels = refreshed;
                selected = selected.min(labels.len() - 1);
                render_main_menu(&labels, selected)?;
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
                selected = selected.saturating_sub(1);
                render_main_menu(&labels, selected)?;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(labels.len() - 1);
                render_main_menu(&labels, selected)?;
            }
            KeyCode::Home => {
                selected = 0;
                render_main_menu(&labels, selected)?;
            }
            KeyCode::End => {
                selected = labels.len() - 1;
                render_main_menu(&labels, selected)?;
            }
            KeyCode::Enter => return Ok(MainMenuResult::Selected(selected)),
            KeyCode::Esc => return Ok(MainMenuResult::Cancelled),
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

    let mut items = Vec::with_capacity(config.profiles.len() + 3);

    for (i, p) in config.profiles.iter().enumerate() {
        items.push((
            format_profile_entry(p, config.is_active(&p.name), name_width),
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
    items.push((format!("{C_FAINT}Quit{C_RESET}"), MainAction::Quit));

    items
}
