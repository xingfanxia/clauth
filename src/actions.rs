//! Pure-data mutations against `AppConfig` and the live `~/.claude` state.
//!
//! Each function takes already-validated inputs from the TUI layer and applies
//! the change under the cross-process state lock.

use anyhow::{Context, Result, bail};

use crate::claude::{
    ClaudeEndpoint, apply_profile_to_claude_settings, clear_claude_credentials,
    force_link_profile_credentials, force_snapshot_active_credentials, link_profile_credentials,
    read_claude_credentials, read_claude_endpoint_config, snapshot_active_credentials,
};
use crate::lock::with_state_lock;
use crate::profile::{
    AppConfig, ClaudeCredentials, Profile, profile_dir, save_app_state, save_profile,
};

// ── Validation ────────────────────────────────────────────────────────────────

/// Verifies `name` is a usable profile slug. Same rules as the legacy
/// inquire prompt: ASCII alphanumeric plus `-`, `_`, `.`, not leading-dot,
/// not empty, not a duplicate of any other profile (allowing `exclude` for
/// rename-in-place).
pub(crate) fn validate_profile_name(
    name: &str,
    existing: &[&str],
    exclude: Option<&str>,
) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("Name cannot be empty.");
    }
    let valid_chars = trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid_chars || trimmed.starts_with('.') {
        bail!(
            "Name must contain only letters, digits, '-', '_', or '.', and cannot start with '.'."
        );
    }
    if existing
        .iter()
        .any(|&n| n.eq_ignore_ascii_case(trimmed) && Some(n) != exclude)
    {
        bail!("A profile named '{trimmed}' already exists.");
    }
    Ok(())
}

// ── Profile actions ───────────────────────────────────────────────────────────

pub(crate) fn switch_profile(config: &mut AppConfig, name: &str) -> Result<()> {
    with_state_lock(|| {
        if config.is_active(name) {
            return Ok(());
        }
        snapshot_active_credentials(config)?;
        link_profile_credentials(name)?;
        finish_switch(config, name)
    })
}

/// Switch after the caller has accepted reconciling a diverged live file:
/// preserve the outgoing profile's live creds unconditionally, then force
/// the symlink to the target. Used by the CLI prompt path only.
pub(crate) fn switch_profile_reconciled(config: &mut AppConfig, name: &str) -> Result<()> {
    with_state_lock(|| {
        if config.is_active(name) {
            return Ok(());
        }
        force_snapshot_active_credentials(config)?;
        force_link_profile_credentials(name)?;
        finish_switch(config, name)
    })
}

/// Turn off all accounts: preserve the active profile's live credentials, then
/// clear the live `~/.claude` credentials and unset the active profile so
/// Claude Code can't spend any account. Used by the wrap-off auto-switch mode
/// when the whole chain is exhausted and no 100%-threshold sink exists. No-op
/// when no profile is active.
///
/// `snapshot_active_credentials` no-ops on a diverged live file (an unsaved
/// `/login`), so the caller is expected to gate on divergence first — clearing
/// a diverged file would otherwise drop a fresh login. The TUI auto-switch path
/// raises its standard Divergence prompt before reaching here.
pub(crate) fn switch_off(config: &mut AppConfig) -> Result<()> {
    with_state_lock(|| {
        if config.state.active_profile.is_none() {
            return Ok(());
        }
        snapshot_active_credentials(config)?;
        clear_claude_credentials()?;
        config.state.active_profile = None;
        save_app_state(&config.state)
    })
}

fn finish_switch(config: &mut AppConfig, name: &str) -> Result<()> {
    // Capture prev env keys before active_profile is reassigned so
    // apply_profile_to_claude_settings can clear the outgoing profile's env.
    let prev_env_keys: Vec<String> = config
        .state
        .active_profile
        .as_deref()
        .and_then(|n| config.find(n))
        .map(|p| p.env.keys().cloned().collect())
        .unwrap_or_default();
    let profile = config.find(name).context("Profile not found")?;
    apply_profile_to_claude_settings(profile, &prev_env_keys)?;
    config.state.active_profile = Some(name.to_string());
    save_app_state(&config.state)
}

pub(crate) fn edit_profile_endpoint(
    config: &mut AppConfig,
    name: &str,
    base_url: Option<String>,
    api_key: Option<String>,
) -> Result<()> {
    with_state_lock(|| {
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
    })
}

pub(crate) fn rename_profile(config: &mut AppConfig, old: &str, new: &str) -> Result<()> {
    with_state_lock(|| {
        let old_dir = profile_dir(old)?;
        if old_dir.exists() {
            std::fs::rename(&old_dir, profile_dir(new)?)
                .with_context(|| format!("Failed to rename profile directory to '{new}'"))?;
        }

        if let Some(profile) = config.find_mut(old) {
            profile.name = new.to_string();
        }
        if let Some(slot) = config.state.profiles.iter_mut().find(|n| n.as_str() == old) {
            *slot = new.to_string();
        }
        if let Some(slot) = config
            .state
            .fallback_chain
            .iter_mut()
            .find(|n| n.as_str() == old)
        {
            *slot = new.to_string();
        }
        let was_active = config.is_active(old);
        if was_active {
            config.state.active_profile = Some(new.to_string());
        }

        save_app_state(&config.state)?;

        if was_active {
            link_profile_credentials(new)?;
        }
        Ok(())
    })
}

pub(crate) fn delete_profile(config: &mut AppConfig, name: &str) -> Result<()> {
    with_state_lock(|| {
        let was_active = config.is_active(name);
        let dir = profile_dir(name)?;

        // Remove the directory first so a filesystem failure keeps the profile
        // visible in state and the user can retry. Persisting state ahead of a
        // failed delete would leave an orphan directory the loader ignores.
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("Failed to delete profile directory for '{name}'"))?;
        }
        config.remove(name);
        save_app_state(&config.state)?;

        if was_active {
            clear_claude_credentials()?;
        }
        Ok(())
    })
}

pub(crate) fn create_blank_profile(
    config: &mut AppConfig,
    name: String,
    base_url: Option<String>,
    api_key: Option<String>,
) -> Result<()> {
    with_state_lock(|| {
        let profile = Profile::new(name, base_url, api_key);
        save_profile(&profile)?;
        config.add(profile);
        save_app_state(&config.state)
    })
}

/// Reads the current `~/.claude` credentials/endpoint and saves them as a new
/// profile under `name`. Returns the matching profile name if these OAuth
/// tokens already belong to one (caller can warn before proceeding).
///
/// Matches on `refresh_token` alone, like `which::resolve_profile`: the refresh
/// token is the stable account identity, while access tokens rotate on every
/// refresh. Keying on the access token here would miss a freshly-rotated login
/// and let capture create a duplicate profile sharing one refresh chain.
pub(crate) fn find_matching_oauth_profile<'a>(
    config: &'a AppConfig,
    live: Option<&ClaudeCredentials>,
) -> Option<&'a str> {
    let live_refresh = live?.refresh_token().filter(|t| !t.is_empty())?;
    config
        .profiles
        .iter()
        .find(|p| p.refresh_token() == Some(live_refresh))
        .map(|p| p.name.as_str())
}

/// Snapshot of the live `~/.claude` state, ready to be turned into a profile.
#[derive(Debug, Clone)]
pub(crate) struct CaptureSnapshot {
    pub(crate) credentials: Option<ClaudeCredentials>,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
}

pub(crate) fn capture_snapshot() -> Result<CaptureSnapshot> {
    let credentials = read_claude_credentials()?;
    let ClaudeEndpoint { base_url, api_key } = read_claude_endpoint_config()?;
    Ok(CaptureSnapshot {
        credentials,
        base_url,
        api_key,
    })
}

pub(crate) fn capture_into_profile(
    config: &mut AppConfig,
    name: String,
    snapshot: CaptureSnapshot,
) -> Result<()> {
    with_state_lock(|| {
        let CaptureSnapshot {
            credentials,
            base_url,
            api_key,
        } = snapshot;
        let mut profile = Profile::new(name.clone(), base_url, api_key);
        profile.credentials = credentials;
        save_profile(&profile)?;
        config.add(profile);

        if config.state.active_profile.is_none() {
            link_profile_credentials(&name)?;
            config.state.active_profile = Some(name);
        }
        save_app_state(&config.state)
    })
}

pub(crate) fn reorder_profile(config: &mut AppConfig, from: usize, to: usize) -> Result<()> {
    if from == to || from >= config.profiles.len() || to >= config.profiles.len() {
        return Ok(());
    }
    with_state_lock(|| {
        // Defensive: resync state.profiles from the in-memory list so a
        // partial save in a prior session can't cause a length mismatch panic
        // here.
        config.state.profiles = config.profiles.iter().map(|p| p.name.clone()).collect();
        let profile = config.profiles.remove(from);
        config.profiles.insert(to, profile);
        let name = config.state.profiles.remove(from);
        config.state.profiles.insert(to, name);
        save_app_state(&config.state)
    })
}
