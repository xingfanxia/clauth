use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::lock::with_state_lock;
use crate::profile::{
    AppConfig, ClaudeCredentials, Profile, atomic_write, home_dir, profile_dir, save_profile,
};

fn claude_credentials_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude").join(".credentials.json"))
}

fn claude_settings_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude").join("settings.json"))
}

/// State of `~/.claude/.credentials.json` relative to a profile's stored
/// credentials. Lets callers refuse to corrupt the profile when the live
/// path is no longer the symlink clauth installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkState {
    /// Symlink in place and resolving to the profile's stored credentials.
    LinkedTo,
    /// Live path exists but is not our symlink — CC re-logged via
    /// unlink+write, the user edited it by hand, or a stale post-shutdown
    /// copy is sitting there.
    Diverged,
    /// Live path does not exist.
    Missing,
}

pub(crate) fn classify_credentials_link(active: &str) -> Result<LinkState> {
    let link = claude_credentials_path()?;
    let expected = profile_dir(active)?.join("credentials.json");
    classify_link_at(&link, &expected)
}

/// Pure path classifier used by `classify_credentials_link` and the inline
/// tests. Symlink target comparison is canonical-when-possible, falling back
/// to literal path equality when either side does not currently resolve.
pub(crate) fn classify_link_at(link: &Path, expected: &Path) -> Result<LinkState> {
    let meta = match link.symlink_metadata() {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(LinkState::Missing),
        Err(e) => return Err(e).context("Failed to stat .credentials.json"),
    };
    if !meta.file_type().is_symlink() {
        return Ok(LinkState::Diverged);
    }
    let target = std::fs::read_link(link).context("Failed to read .credentials.json link")?;
    if paths_equivalent(&target, expected) {
        Ok(LinkState::LinkedTo)
    } else {
        Ok(LinkState::Diverged)
    }
}

fn paths_equivalent(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// True when `active` owns no stored credentials yet and the live
/// `.credentials.json` is a regular file carrying a completed OAuth login.
/// This is a credential-less profile's first login (e.g. a blank profile the
/// user just authenticated through Claude Code), which clauth adopts rather
/// than refusing as divergence. Mirrors the runtime watchdog's first-login
/// handling in `sync_credentials_unlocked`.
pub(crate) fn is_first_login(active: &str) -> Result<bool> {
    let link = claude_credentials_path()?;
    let expected = profile_dir(active)?.join("credentials.json");
    Ok(is_first_login_at(&link, &expected))
}

/// Pure path-based companion to [`is_first_login`], split out for testing.
/// `expected` is the profile's stored credentials file; its absence is the
/// "no stored credentials" signal. The OAuth check rejects a mid-flight
/// partial write (e.g. an empty `{}`) so adoption waits for a completed login.
fn is_first_login_at(link: &Path, expected: &Path) -> bool {
    if expected.exists() {
        return false;
    }
    let Ok(meta) = link.symlink_metadata() else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return false;
    }
    std::fs::read(link)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ClaudeCredentials>(&bytes).ok())
        .is_some_and(|creds| creds.claude_ai_oauth.is_some())
}

pub(crate) fn read_claude_credentials() -> Result<Option<ClaudeCredentials>> {
    let path = claude_credentials_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path).context("Failed to read .credentials.json")?;
    serde_json::from_str(&content)
        .context("Failed to parse .credentials.json")
        .map(Some)
}

#[cfg(unix)]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).context("Failed to create credential symlink")
}

#[cfg(windows)]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    match std::os::windows::fs::symlink_file(target, link) {
        Ok(()) => Ok(()),
        Err(_) => std::fs::copy(target, link)
            .map(|_| ())
            .context("Failed to copy credentials"),
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::fs::copy(target, link)
        .map(|_| ())
        .context("Failed to copy credentials")
}

/// Symlinks `~/.claude/.credentials.json` → profile's `credentials.json`;
/// copies on Windows without symlink privilege. Refuses to overwrite a
/// regular file at the live path unless its content already matches the
/// profile target — replacing a divergent regular file would silently
/// destroy whatever CC wrote there (typically a re-login the user hasn't
/// resolved yet).
pub(crate) fn link_profile_credentials(name: &str) -> Result<()> {
    with_state_lock(|| {
        let link = claude_credentials_path()?;
        let target = profile_dir(name)?.join("credentials.json");

        if let Ok(meta) = link.symlink_metadata() {
            if !meta.file_type().is_symlink() {
                let live_bytes = std::fs::read(&link).ok();
                let target_bytes = std::fs::read(&target).ok();
                if live_bytes != target_bytes {
                    anyhow::bail!(
                        "refusing to replace .credentials.json — live file differs from profile '{name}'; resolve divergence first"
                    );
                }
            }
            std::fs::remove_file(&link).context("Failed to remove old .credentials.json")?;
        }

        if target.exists() {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            create_symlink(&target, &link)?;
        }

        Ok(())
    })
}

pub(crate) fn clear_claude_credentials() -> Result<()> {
    with_state_lock(|| {
        let link = claude_credentials_path()?;
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).context("Failed to remove .credentials.json")?;
        }
        Ok(())
    })
}

pub(crate) struct ClaudeEndpoint {
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
}

pub(crate) fn read_claude_endpoint_config() -> Result<ClaudeEndpoint> {
    let path = claude_settings_path()?;
    if !path.exists() {
        return Ok(ClaudeEndpoint {
            base_url: None,
            api_key: None,
        });
    }
    let content = std::fs::read_to_string(&path).context("Failed to read settings.json")?;
    let settings: serde_json::Value =
        serde_json::from_str(&content).context("Failed to parse settings.json")?;
    Ok(ClaudeEndpoint {
        base_url: settings["env"]["ANTHROPIC_BASE_URL"]
            .as_str()
            .map(str::to_owned),
        api_key: settings["env"]["ANTHROPIC_AUTH_TOKEN"]
            .as_str()
            .map(str::to_owned),
    })
}

/// Patches `settings.json`'s `env` block with ANTHROPIC_BASE_URL,
/// ANTHROPIC_AUTH_TOKEN, and the profile's `env` map. Keys in `prev_env_keys`
/// that the new profile doesn't carry are removed first so stale entries from
/// the previously active profile don't linger. Every other field is untouched.
pub(crate) fn apply_profile_to_claude_settings(
    profile: &Profile,
    prev_env_keys: &[String],
) -> Result<()> {
    with_state_lock(|| apply_profile_to_claude_settings_inner(profile, prev_env_keys))
}

fn apply_profile_to_claude_settings_inner(
    profile: &Profile,
    prev_env_keys: &[String],
) -> Result<()> {
    let path = claude_settings_path()?;

    let has_anything = profile.base_url.is_some()
        || profile.api_key.is_some()
        || !profile.env.is_empty()
        || !prev_env_keys.is_empty();
    if !has_anything && !path.exists() {
        return Ok(());
    }

    let content = build_claude_settings_json(&path, profile, prev_env_keys)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&path, content).context("Failed to write settings.json")
}

/// Merges `base_path`'s settings.json (or `{}` when missing) with the profile's
/// endpoint keys and env overlay. `prev_env_keys` lists env keys to strip
/// before applying the new profile — used by the switch path to clear the
/// previously active profile's custom env. `start` passes `&[]` so existing
/// keys in the file stay untouched.
pub(crate) fn build_claude_settings_json(
    base_path: &Path,
    profile: &Profile,
    prev_env_keys: &[String],
) -> Result<String> {
    let mut settings: serde_json::Value = if base_path.exists() {
        let content = std::fs::read_to_string(base_path).context("Failed to read settings.json")?;
        serde_json::from_str(&content).context("Failed to parse settings.json")?
    } else {
        serde_json::json!({})
    };

    if settings.get("env").is_none() {
        settings["env"] = serde_json::json!({});
    }

    let env = settings["env"]
        .as_object_mut()
        .context("settings.json `env` is not an object")?;

    for key in prev_env_keys {
        if !profile.env.contains_key(key) {
            env.remove(key);
        }
    }

    match profile.base_url.as_deref() {
        Some(url) => {
            env.insert("ANTHROPIC_BASE_URL".into(), url.into());
        }
        None => {
            env.remove("ANTHROPIC_BASE_URL");
        }
    }
    match profile.api_key.as_deref() {
        Some(key) => {
            env.insert("ANTHROPIC_AUTH_TOKEN".into(), key.into());
        }
        None => {
            env.remove("ANTHROPIC_AUTH_TOKEN");
        }
    }

    // Apply profile env last so an explicit ANTHROPIC_* entry in the profile
    // env map wins over the dedicated base_url / api_key fields.
    for (k, v) in &profile.env {
        env.insert(k.clone(), v.clone().into());
    }

    serde_json::to_string_pretty(&settings).context("Failed to serialize settings.json")
}

/// Reads the live .credentials.json and saves it to the active profile.
/// No-op when the live path has diverged from our symlink — accepting a
/// divergent live file as authoritative would silently overwrite the
/// profile's stored identity. The reconciliation modal resolves divergence
/// by calling `force_snapshot_active_credentials` after the user picks
/// "Overwrite".
pub(crate) fn snapshot_active_credentials(config: &mut AppConfig) -> Result<()> {
    with_state_lock(|| {
        let Some(active) = config.state.active_profile.clone() else {
            return Ok(());
        };
        if matches!(classify_credentials_link(&active)?, LinkState::Diverged) {
            // A divergent live file is normally a re-login the user must
            // resolve, so the stored identity stays untouched. The one
            // exception is a credential-less profile's first login: adopt
            // Claude Code's write so the profile gains an identity.
            if is_first_login(&active)? {
                adopt_first_login(config, &active)?;
            }
            return Ok(());
        }
        snapshot_active_credentials_unchecked(config, &active)
    })
}

/// Adopt a credential-less profile's first login: store the live
/// `.credentials.json` into the active profile, then replace it with a symlink
/// so later Claude Code writes stay owned. Callers gate this on
/// [`is_first_login`]; calling it otherwise would overwrite stored identity.
pub(crate) fn adopt_first_login(config: &mut AppConfig, active: &str) -> Result<()> {
    with_state_lock(|| {
        snapshot_active_credentials_unchecked(config, active)?;
        force_link_profile_credentials(active)
    })
}

fn snapshot_active_credentials_unchecked(config: &mut AppConfig, active: &str) -> Result<()> {
    let credentials = read_claude_credentials()?;
    if let Some(profile) = config.find_mut(active) {
        profile.credentials = credentials;
        save_profile(profile)?;
    }
    Ok(())
}

/// Snapshot the live .credentials.json into the active profile even when
/// the link is diverged. Called by the divergence-resolution modal's
/// "Overwrite active profile with live creds" action.
pub(crate) fn force_snapshot_active_credentials(config: &mut AppConfig) -> Result<()> {
    with_state_lock(|| {
        let Some(active) = config.state.active_profile.clone() else {
            return Ok(());
        };
        snapshot_active_credentials_unchecked(config, &active)
    })
}

/// Re-link `~/.claude/.credentials.json` to `name`'s stored credentials,
/// overwriting whatever's at the live path. Used by the divergence modal's
/// "Discard new creds" action to restore the profile's stored identity.
pub(crate) fn force_link_profile_credentials(name: &str) -> Result<()> {
    with_state_lock(|| {
        let link = claude_credentials_path()?;
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).context("Failed to remove .credentials.json")?;
        }
        let target = profile_dir(name)?.join("credentials.json");
        if target.exists() {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            create_symlink(&target, &link)?;
        }
        Ok(())
    })
}

/// Returns true when both sides carry an OAuth block and either the access
/// token or refresh token differs. Missing data on either side returns false
/// — the caller's normal snapshot/skip path is safer than guessing.
pub(crate) fn credentials_diverged(
    stored: Option<&ClaudeCredentials>,
    live: Option<&ClaudeCredentials>,
) -> bool {
    let Some(stored) = stored.and_then(|c| c.claude_ai_oauth.as_ref()) else {
        return false;
    };
    let Some(live) = live.and_then(|c| c.claude_ai_oauth.as_ref()) else {
        return false;
    };
    stored.access_token != live.access_token || stored.refresh_token != live.refresh_token
}

/// Replaces the symlink at `~/.claude/.credentials.json` with a regular file
/// containing the same bytes. No-op when the path is already a regular file
/// or doesn't exist. Called when the user disowns the active profile so
/// subsequent Claude Code writes don't bleed into that profile's storage
/// through the symlink.
pub(crate) fn detach_credentials_link() -> Result<()> {
    with_state_lock(|| {
        let path = claude_credentials_path()?;
        let Ok(meta) = path.symlink_metadata() else {
            return Ok(());
        };
        if !meta.file_type().is_symlink() {
            return Ok(());
        }
        let content =
            std::fs::read(&path).context("Failed to read .credentials.json before detach")?;
        std::fs::remove_file(&path).context("Failed to remove .credentials.json symlink")?;
        atomic_write(&path, content).context("Failed to write detached .credentials.json")?;
        Ok(())
    })
}

#[cfg(test)]
#[path = "../tests/inline/claude.rs"]
mod tests;
