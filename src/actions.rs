use anyhow::{Context, Result, bail};
use inquire::{Confirm, InquireError, Text};

use crate::claude::{
    ClaudeEndpoint, apply_profile_to_claude_settings, clear_claude_credentials,
    link_profile_credentials, read_claude_credentials, read_claude_endpoint_config,
    snapshot_active_credentials,
};
use crate::profile::{AppConfig, Profile, profile_dir, save_app_state, save_profile};

// ── Prompts ───────────────────────────────────────────────────────────────────

pub(crate) fn prompt_optional(label: &str, current: Option<&str>) -> Result<Option<String>> {
    let value = Text::new(label)
        .with_default(current.unwrap_or(""))
        .with_help_message("Leave empty to unset")
        .prompt()?;
    Ok((!value.trim().is_empty()).then_some(value))
}

pub(crate) fn prompt_profile_name(existing: &[&str], exclude: Option<&str>) -> Result<String> {
    let name = Text::new("Profile name:").prompt()?.trim().to_string();
    if name.is_empty() {
        bail!("Name cannot be empty.");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        || name.starts_with('.')
    {
        bail!(
            "Name must contain only letters, digits, '-', '_', or '.', and cannot start with '.'."
        );
    }
    if existing.iter().any(|&n| n == name && Some(n) != exclude) {
        bail!("A profile named '{name}' already exists.");
    }
    Ok(name)
}

pub(crate) fn is_cancelled(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<InquireError>(),
        Some(InquireError::OperationCanceled | InquireError::OperationInterrupted),
    )
}

// ── Submenu actions ───────────────────────────────────────────────────────────

pub(crate) fn switch_profile(config: &mut AppConfig, name: &str) -> Result<()> {
    if config.is_active(name) {
        return Ok(());
    }

    snapshot_active_credentials(config)?;

    let prev_env_keys: Vec<String> = config
        .state
        .active_profile
        .as_deref()
        .and_then(|n| config.find(n))
        .map(|p| p.env.keys().cloned().collect())
        .unwrap_or_default();

    link_profile_credentials(name)?;
    let profile = config.find(name).context("Profile not found")?;
    apply_profile_to_claude_settings(profile, &prev_env_keys)?;
    config.state.active_profile = Some(name.to_string());
    save_app_state(&config.state)
}

pub(crate) fn edit_profile(config: &mut AppConfig, name: &str) -> Result<()> {
    let (current_url, current_key) = {
        let profile = config.find(name).context("Profile not found")?;
        (profile.base_url.clone(), profile.api_key.clone())
    };

    let base_url = prompt_optional("Base URL:", current_url.as_deref())?;
    let api_key = prompt_optional("API key:", current_key.as_deref())?;

    let profile = config.find_mut(name).context("Profile not found")?;
    profile.base_url = base_url;
    profile.api_key = api_key;
    save_profile(profile)?;

    if config.is_active(name) {
        let profile = config.find(name).context("Profile not found")?;
        let prev_env_keys: Vec<String> = profile.env.keys().cloned().collect();
        apply_profile_to_claude_settings(profile, &prev_env_keys)?;
    }
    Ok(())
}

/// Returns true if the profile was renamed (submenu should exit to refresh list).
pub(crate) fn rename_profile(config: &mut AppConfig, old: &str) -> Result<bool> {
    let new = match prompt_profile_name(&config.names(), Some(old)) {
        Ok(n) => n,
        Err(e) if is_cancelled(&e) => return Ok(false),
        Err(e) => return Err(e),
    };

    let old_dir = profile_dir(old)?;
    if old_dir.exists() {
        std::fs::rename(&old_dir, profile_dir(&new)?)
            .with_context(|| format!("Failed to rename profile directory to '{new}'"))?;
    }

    if let Some(profile) = config.find_mut(old) {
        profile.name = new.clone();
    }
    if let Some(slot) = config.state.profiles.iter_mut().find(|n| n.as_str() == old) {
        *slot = new.clone();
    }
    let was_active = config.is_active(old);
    if was_active {
        config.state.active_profile = Some(new.clone());
    }

    save_app_state(&config.state)?;

    if was_active {
        link_profile_credentials(&new)?;
    }
    Ok(true)
}

/// Returns true if the profile was deleted (submenu should exit).
pub(crate) fn delete_profile(config: &mut AppConfig, name: &str) -> Result<bool> {
    let confirmed = match Confirm::new(&format!("Delete '{name}'?"))
        .with_default(false)
        .prompt()
    {
        Ok(c) => c,
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
            return Ok(false);
        }
        Err(e) => return Err(e.into()),
    };
    if !confirmed {
        return Ok(false);
    }

    let was_active = config.is_active(name);
    let dir = profile_dir(name)?;

    config.remove(name);
    save_app_state(&config.state)?;

    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("Failed to delete profile directory for '{name}'"))?;
    }
    if was_active {
        clear_claude_credentials()?;
    }
    Ok(true)
}

// ── Main-menu actions ─────────────────────────────────────────────────────────

pub(crate) fn create_blank_profile(config: &mut AppConfig) -> Result<()> {
    let name = prompt_profile_name(&config.names(), None)?;
    let base_url = prompt_optional("Base URL:", None)?;
    let api_key = if base_url.is_some() {
        prompt_optional("API key:", None)?
    } else {
        None
    };

    let profile = Profile::new(name, base_url, api_key);
    save_profile(&profile)?;
    config.add(profile);
    save_app_state(&config.state)
}

pub(crate) fn capture_current_profile(config: &mut AppConfig) -> Result<()> {
    let credentials = read_claude_credentials()?;
    let ClaudeEndpoint { base_url, api_key } = read_claude_endpoint_config()?;
    let name = prompt_profile_name(&config.names(), None)?;

    let mut profile = Profile::new(name.clone(), base_url, api_key);
    profile.credentials = credentials;
    save_profile(&profile)?;
    config.add(profile);

    if config.state.active_profile.is_none() {
        link_profile_credentials(&name)?;
        config.state.active_profile = Some(name);
    }
    save_app_state(&config.state)
}
