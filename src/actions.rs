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
    AppConfig, ClaudeCredentials, Profile, profile_dir, save_app_state, save_profile,
};
use crate::spinner::Spinner;
use crate::usage::OpResult;

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

/// `clauth <profile>` CLI switch: rotate the outgoing/incoming chains, apply the
/// relink (reconciling a diverged live file via the interactive `[Y/n]` prompt
/// when needed), then prime the target's 5h usage window. Takes the owned config
/// and wraps it in the shared `Arc<RankedMutex<…>>` the oauth fns require.
pub(crate) fn switch_profile_cli(config: AppConfig, canonical: &str) -> Result<()> {
    // Rotate only the outgoing active and incoming target profiles
    // before the FS relink. Rotating every other profile's single-use
    // refresh token on every switch is unnecessary and widens races with
    // the scheduler.
    let outgoing = config.state.active_profile.clone();
    // No scheduler running: pass `None` for the refetch queue and activity
    // store side-channels (nothing drains the queue, no spinner to drive), so
    // the CLI switch allocates no throwaway mutexes just to satisfy the shared
    // `rotate_one` / `start_window` signatures.
    //
    // CLI has no OpResult drain — drop the receiver immediately so
    // workers' `sender.send` returns disconnected-error which they
    // ignore (`let _ = …`). The Arc<Mutex<AppConfig>> wraps the
    // owned config so oauth fns can take/drop the lock per their
    // contract.
    let (op_sender, _op_receiver) = std::sync::mpsc::channel::<OpResult>();

    // Classify the outgoing active profile's live link BEFORE any
    // rotation. A diverged link means CC re-logged or rotated and wrote
    // a regular file — a different, still-valid chain. Rotating the
    // STORED chain in that case burns a single-use refresh token that
    // the reconcile path (`force_snapshot_active_credentials`) then
    // discards when it captures the live creds. Computing the verdict
    // first lets us skip the doomed rotation. These checks are pure
    // path/FS reads (no network, no config lock).
    let reconciled = match outgoing.as_deref() {
        Some(active) => {
            matches!(classify_credentials_link(active)?, LinkState::Diverged)
                && !is_first_login(active)?
        }
        None => false,
    };

    let config = Arc::new(RankedMutex::new(config));
    {
        // Scoped so the spinner stops before the interactive [Y/n]
        // prompt below — a live spinner during stdin read corrupts it.
        let _spinner = Spinner::start("clauth: rotating tokens…");
        // Skip the outgoing rotation when its live link diverged: its
        // stored chain is about to be overwritten by the live creds, so
        // rotating it only burns a refresh token for nothing.
        if let Some(ref active) = outgoing
            && active != canonical
            && !reconciled
        {
            oauth::rotate_one(&config, active, None, &op_sender);
        }
        oauth::rotate_one(&config, canonical, None, &op_sender);
    }

    // When the outgoing active profile has a diverged live credentials
    // file (CC re-logged or wrote a regular file), prompt rather than
    // refusing. On Yes: capture the live creds into the outgoing
    // profile first, then force the switch. On No: abort cleanly.
    if reconciled {
        let active = {
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
            let mut cfg = config.lock().expect("config mutex poisoned");
            switch_profile_reconciled(&mut cfg, canonical)?;
        } else {
            println!("aborted — no changes made");
            return Ok(());
        }
    } else {
        let mut cfg = config.lock().expect("config mutex poisoned");
        switch_profile(&mut cfg, canonical)?;
    }

    // Match the TUI: prime the 5h window if the target is opted in
    // via `auto_start = true`. Cooldown blocks repeated CLI switches
    // from re-kicking inside the same window.
    {
        let _spinner = Spinner::start("clauth: priming usage window…");
        let _ = oauth::start_window(&config, canonical, None, None, &op_sender);
    }
    println!("switched to '{canonical}'");
    Ok(())
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
        // Defensive: resync state.profiles from the in-memory list so a
        // partial save in a prior session can't cause a length mismatch panic
        // here.
        config.sync_state_profiles();
        let profile = config.profiles.remove(from);
        config.profiles.insert(to, profile);
        let name = config.state.profiles.remove(from);
        config.state.profiles.insert(to, name);
        save_app_state(&config.state)
    })
}
