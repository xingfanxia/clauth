use anyhow::Result;
use inquire::{InquireError, Select};

use crate::actions::{
    delete_profile, edit_profile, is_cancelled, rename_profile, switch_profile,
};
use crate::profile::AppConfig;
use crate::ui::{
    C_DANGER, C_DIM, C_FAINT, C_FG_OFF, C_NOBOLD, C_ORANGE, C_RESET,
    format_profile_entry, format_submenu_title,
};

// ── Profile submenu ───────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum SubmenuAction { Switch, Edit, Rename, Delete, Back }

impl SubmenuAction {
    fn label(self, is_active: bool) -> String {
        match self {
            Self::Switch if is_active =>
                format!("Switch to this profile{C_NOBOLD}  {C_FAINT}(already active){C_RESET}"),
            Self::Switch => "Switch to this profile".to_string(),
            Self::Edit   => format!("Edit{C_NOBOLD}  {C_DIM}(URL / API key){C_RESET}"),
            Self::Rename => "Rename".to_string(),
            Self::Delete => format!("{C_DANGER}Delete{C_RESET}"),
            Self::Back   => format!("{C_FAINT}← Back{C_RESET}"),
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
            Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => return Ok(()),
            Err(e) => return Err(e.into()),
        };

        let result: Result<bool> = match ACTIONS[idx] {
            Switch => switch_profile(config, profile_name).map(|_| true),
            Edit   => edit_profile(config, profile_name).map(|_| false),
            Rename => rename_profile(config, profile_name),
            Delete => delete_profile(config, profile_name),
            Back   => return Ok(()),
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

pub(crate) fn build_main_menu(config: &AppConfig) -> Vec<(String, MainAction)> {
    let name_width = config.profiles.iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(0)
        .max(4);

    let mut items = Vec::with_capacity(config.profiles.len() + 3);

    for (i, p) in config.profiles.iter().enumerate() {
        items.push((format_profile_entry(p, config.is_active(&p.name), name_width), MainAction::Profile(i)));
    }
    items.push((format!("{C_ORANGE}+{C_FG_OFF} New profile"), MainAction::NewBlank));
    items.push((format!("{C_ORANGE}+{C_FG_OFF} New from current profile"), MainAction::Capture));
    items.push((format!("{C_FAINT}Quit{C_RESET}"), MainAction::Quit));

    items
}
