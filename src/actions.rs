//! Pure-data mutations against `AppConfig` and the live `~/.claude` state.
//!
//! Each function takes already-validated inputs from the TUI layer and applies
//! the change under the cross-process state lock.

use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::claude::{
    ClaudeEndpoint, LinkState, apply_profile_to_claude_settings, classify_credentials_link,
    clear_claude_credentials, force_link_profile_credentials, force_snapshot_active_credentials,
    is_first_login, link_profile_credentials, read_claude_credentials, read_claude_endpoint_config,
    snapshot_active_credentials,
};
use crate::lock::with_state_lock;
use crate::lockorder::RankedMutex;
use crate::oauth;
use crate::profile::{
    AppConfig, ClaudeCredentials, ModelSettings, Profile, profile_dir, save_app_state, save_profile,
};
use crate::spinner::Spinner;

/// ASCII alphanumeric + `-_.", not leading-dot, not empty, not a duplicate
/// (`exclude` exempts the current name for rename-in-place).
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

/// Force-snapshot the outgoing creds then force the symlink. CLI prompt path only.
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

/// CLI switch: relink (reconciling diverged live file via `[Y/n]` prompt), then
/// prime the 5h window. No token rotation — stale chains rotate lazily on first use.
pub(crate) fn switch_profile_cli(config: AppConfig, canonical: &str) -> Result<()> {
    let outgoing = config.state.active_profile.clone();

    // Diverged link = CC re-logged and wrote a regular file; must reconcile
    // (capture into outgoing profile) rather than refuse.
    let reconciled = match outgoing.as_deref() {
        Some(active) => {
            matches!(classify_credentials_link(active)?, LinkState::Diverged)
                && !is_first_login(active)?
        }
        None => false,
    };

    let config = Arc::new(RankedMutex::new(config));

    if reconciled {
        let active = {
            #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
            let cfg = config.lock().expect("config mutex poisoned");
            cfg.state
                .active_profile
                .as_deref()
                .unwrap_or("")
                .to_string()
        };
        print!(
            "active profile '{active}' has uncaptured credentials in ~/.claude \
             (a re-login or token rotation). capture them into '{active}' and \
             switch to '{canonical}'? [Y/n] "
        );
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        let answer = answer.trim().to_ascii_lowercase();
        if answer.is_empty() || answer == "y" || answer == "yes" {
            #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
            let mut cfg = config.lock().expect("config mutex poisoned");
            switch_profile_reconciled(&mut cfg, canonical)?;
        } else {
            println!("aborted — no changes made");
            return Ok(());
        }
    } else {
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let mut cfg = config.lock().expect("config mutex poisoned");
        switch_profile(&mut cfg, canonical)?;
    }

    // Prime the 5h window if opted in. Kicks with the current access token and
    // rotates once on a 401/429. One-shot — the CLI has no scheduler tick to
    // re-arm against, so no side channels.
    {
        let _spinner = Spinner::start("clauth: priming usage window");
        let _ = oauth::prime_window(&config, canonical);
    }
    println!("switched to '{canonical}'");
    Ok(())
}

/// Snapshot active creds then clear them so Claude Code can't spend any account.
/// Used by wrap-off mode when the whole chain is exhausted. No-op when no profile
/// is active. Caller must gate on divergence first — snapshot no-ops on a diverged
/// file, so clearing without checking would drop a fresh `/login`.
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
    // Capture outgoing env keys before active_profile is reassigned.
    let prev_env_keys: Vec<String> = config
        .state
        .active_profile
        .as_deref()
        .and_then(|n| config.find(n))
        .map(|p| p.env.keys().cloned().collect())
        .unwrap_or_default();
    let profile = config.find(name).context("Profile not found")?;
    apply_profile_to_claude_settings(profile, &prev_env_keys)?;
    config.state.active_profile = Some(name.into());
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
        let old_api_key = profile.api_key.clone();
        profile.base_url = base_url;
        profile.api_key = api_key;
        // Re-derive the provider — the in-memory config is authoritative until
        // the next disk reload, so a stale value here would keep (or block)
        // third-party fetches against the wrong endpoint. Also clear when only the
        // api key changed for the same provider (rotated key — old stats are stale).
        let provider = profile
            .base_url
            .as_deref()
            .and_then(crate::providers::Provider::from_base_url);
        if provider != profile.provider || (provider.is_some() && profile.api_key != old_api_key) {
            profile.third_party_usage = None;
        }
        profile.provider = provider;
        save_profile(profile)?;

        if config.is_active(name) {
            let profile = config.find(name).context("Profile not found")?;
            let prev_env_keys: Vec<String> = profile.env.keys().cloned().collect();
            apply_profile_to_claude_settings(profile, &prev_env_keys)?;
        }
        Ok(())
    })
}

/// Persist a profile's model configuration. Re-applies to the live
/// `~/.claude/settings.json` when the profile is active so a running `claude`
/// picks it up on its next settings read. Mirrors [`edit_profile_endpoint`].
pub(crate) fn edit_profile_model(
    config: &mut AppConfig,
    name: &str,
    models: ModelSettings,
) -> Result<()> {
    with_state_lock(|| {
        let profile = config.find_mut(name).context("Profile not found")?;
        profile.models = models;
        save_profile(profile)?;

        if config.is_active(name) {
            // A model-only edit never touches the generic `env` map, so passing
            // this profile's own keys as `prev` strips nothing (the removal loop
            // keeps every key the profile still carries). The model env keys
            // (`ANTHROPIC_DEFAULT_*`/`CLAUDE_CODE_SUBAGENT_MODEL`) are set or
            // cleared unconditionally inside `build_claude_settings_json`.
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

        let was_active = config.is_active(old);
        config.rename_all_occurrences(old, new);

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

        // Remove directory first: a failed delete keeps the profile in state so
        // the user can retry; persisting state first would leave an orphan dir.
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

/// Returns a profile whose `refresh_token` matches `live`. Matches on refresh
/// token only (stable identity); access tokens rotate and would produce false
/// misses and duplicate profiles.
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
            config.state.active_profile = Some(name.into());
        }
        save_app_state(&config.state)
    })
}

pub(crate) fn reorder_profile(config: &mut AppConfig, from: usize, to: usize) -> Result<()> {
    if from == to || from >= config.profiles.len() || to >= config.profiles.len() {
        return Ok(());
    }
    with_state_lock(|| {
        // Resync to fix length drift from a partial save in a prior session.
        config.sync_state_profiles();
        let profile = config.profiles.remove(from);
        config.profiles.insert(to, profile);
        let name = config.state.profiles.remove(from);
        config.state.profiles.insert(to, name);
        save_app_state(&config.state)
    })
}
