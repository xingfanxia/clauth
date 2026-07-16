use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::lock::with_state_lock;
use crate::profile::{
    AppConfig, ClaudeCredentials, Profile, atomic_write, atomic_write_600, claude_dir, profile_dir,
    read_json_file, save_profile,
};

fn claude_credentials_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join(".credentials.json"))
}

fn claude_settings_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join("settings.json"))
}

/// State of `~/.claude/.credentials.json` relative to a profile's stored credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkState {
    /// Symlink resolves to the profile's stored credentials, OR a regular file
    /// whose live OAuth access token matches the profile's stored one (macOS: Claude
    /// Code rewrites the file from the Keychain, replacing our symlink with an
    /// identical-content regular file — not divergence).
    LinkedTo,
    /// Path exists and its live credential differs from the profile's stored one —
    /// a genuine CC re-login / token rotation the user may want to capture.
    Diverged,
    /// Path does not exist.
    Missing,
}

pub(crate) fn classify_credentials_link(active: &str) -> Result<LinkState> {
    let link = claude_credentials_path()?;
    let expected = profile_dir(active)?.join("credentials.json");
    classify_link_at(&link, &expected)
}

/// Classify a symlink at `link` against `expected`; canonical paths when resolvable.
pub(crate) fn classify_link_at(link: &Path, expected: &Path) -> Result<LinkState> {
    let meta = match link.symlink_metadata() {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(LinkState::Missing),
        Err(e) => return Err(e).context("failed to stat .credentials.json"),
    };
    if !meta.file_type().is_symlink() {
        // Not our symlink. On macOS, Claude Code rewrites ~/.claude/.credentials.json
        // as a regular-file mirror of the Keychain after every run, clobbering the
        // symlink we created. That is NOT divergence when the credential is unchanged
        // — only a genuine re-login / token rotation (different access token) is.
        // Compare content instead of trusting symlink identity so an ordinary switch
        // doesn't falsely prompt to capture credentials that already match the profile.
        return Ok(
            match (
                read_json_file::<ClaudeCredentials>(link),
                read_json_file::<ClaudeCredentials>(expected),
            ) {
                (Ok(live), Ok(stored))
                    if live.access_token().is_some_and(|t| !t.is_empty())
                        && live.access_token() == stored.access_token() =>
                {
                    LinkState::LinkedTo
                }
                _ => LinkState::Diverged,
            },
        );
    }
    let target = std::fs::read_link(link).context("failed to read .credentials.json link")?;
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

/// True when the profile has no stored credentials but the live path is a regular
/// file with a completed OAuth login — first login after blank profile creation.
/// clauth adopts this rather than treating it as divergence.
pub(crate) fn is_first_login(active: &str) -> Result<bool> {
    let link = claude_credentials_path()?;
    let expected = profile_dir(active)?.join("credentials.json");
    Ok(is_first_login_at(&link, &expected))
}

/// Path-based core of [`is_first_login`], split for testing. The OAuth check
/// rejects partial writes (e.g. `{}`) so adoption waits for a completed login.
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
    read_json_file(&path).map(Some)
}

/// True when the credentials hold NO usable login: no OAuth block, or one whose
/// access AND refresh tokens are both absent/blank — Claude Code's logged-out
/// shell (it blanks the tokens and zeroes `expiresAt` when its own refresh
/// dies, keeping unrelated keys like `mcpOAuth`). A shell still classifies
/// [`LinkState::Diverged`], but there is no login in it to protect.
pub(crate) fn live_login_is_empty(creds: &ClaudeCredentials) -> bool {
    creds.access_token().filter(|t| !t.is_empty()).is_none()
        && creds.refresh_token().filter(|t| !t.is_empty()).is_none()
}

/// True when the live `.credentials.json` currently parses to such a logged-out
/// shell. An unreadable or non-JSON file reads `false` — it may be a Claude
/// Code write in progress, and "possibly a login" keeps the same protection as
/// a real one (the divergence guards stay armed).
pub(crate) fn live_credentials_are_shell() -> bool {
    matches!(
        read_claude_credentials(),
        Ok(Some(live)) if live_login_is_empty(&live)
    )
}

/// macOS: mirror a profile's stored OAuth login into the Keychain so Claude Code
/// (which reads the Keychain, not the file) actually switches account. No-op when
/// the profile has no stored `credentials.json` (a base_url profile, whose
/// endpoint+token come from `settings.json`, or an OAuth profile not yet logged
/// in) — the existing Keychain login is left untouched in that case.
///
/// Runs after the symlink swap and is `?`-fatal: a failure leaves the file layer
/// switched while CC still reads the old Keychain login. Loud + recoverable —
/// both writes are idempotent, so retrying the switch re-runs the pair.
#[cfg(target_os = "macos")]
fn keychain_write_profile(name: &str) -> Result<()> {
    let path = profile_dir(name)?.join("credentials.json");
    if !path.exists() {
        return Ok(());
    }
    let creds: ClaudeCredentials = read_json_file(&path)?;
    crate::keychain::keychain_write(&creds)
}

#[cfg(unix)]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).context("failed to create credential symlink")
}

#[cfg(windows)]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    match std::os::windows::fs::symlink_file(target, link) {
        Ok(()) => Ok(()),
        Err(_) => std::fs::copy(target, link)
            .map(|_| ())
            .context("failed to copy credentials"),
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::fs::copy(target, link)
        .map(|_| ())
        .context("failed to copy credentials")
}

/// Symlink `~/.claude/.credentials.json` → profile's `credentials.json` (copy on
/// Windows). Refuses to overwrite a non-matching regular file — that would silently
/// drop a CC re-login the user hasn't resolved yet.
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
                        "refusing to replace .credentials.json: live file differs from profile '{name}'; {} first",
                        crate::format::RESOLVE_IN_TUI
                    );
                }
            }
            std::fs::remove_file(&link).context("failed to remove old .credentials.json")?;
        }

        if target.exists() {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            create_symlink(&target, &link)?;
            // macOS: make the switch real — Claude Code reads the Keychain.
            #[cfg(target_os = "macos")]
            if crate::keychain::enabled() {
                keychain_write_profile(name)?;
            }
        }

        Ok(())
    })
}

pub(crate) fn clear_claude_credentials() -> Result<()> {
    with_state_lock(|| {
        let link = claude_credentials_path()?;
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).context("failed to remove .credentials.json")?;
        }
        // macOS: also drop the live Keychain login so Claude Code can't spend the
        // account (parity with removing the credential file). This deletes
        // whatever the item holds at that moment — possibly a chain CC rotated
        // after our last capture, or a login clauth never wrote. The write-only
        // design can't snapshot it first (reading Claude's item prompts on every
        // call), so that tail is lost and needs a re-login; see keychain.rs.
        #[cfg(target_os = "macos")]
        if crate::keychain::enabled() {
            crate::keychain::keychain_delete()?;
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
    let settings: serde_json::Value = read_json_file(&path)?;
    Ok(ClaudeEndpoint {
        base_url: settings["env"]["ANTHROPIC_BASE_URL"]
            .as_str()
            .map(str::to_owned),
        api_key: settings["env"]["ANTHROPIC_AUTH_TOKEN"]
            .as_str()
            .map(str::to_owned),
    })
}

/// The Setup-tab field that owns a clauth-managed env key, phrased for the
/// collision prompt (`'X' is already set by …`). These are the keys clauth
/// derives from a profile's endpoint + model-tier fields; a custom env entry
/// equal to one of them would override the field's value in `settings.json`.
/// `None` when the key is not clauth-managed.
pub(crate) fn managed_env_key_label(key: &str) -> Option<&'static str> {
    Some(match key {
        "ANTHROPIC_BASE_URL" => "the base url field",
        "ANTHROPIC_AUTH_TOKEN" => "the api key field",
        "ANTHROPIC_DEFAULT_OPUS_MODEL" => "the opus model field",
        "ANTHROPIC_DEFAULT_SONNET_MODEL" => "the sonnet model field",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL" => "the haiku model field",
        "CLAUDE_CODE_SUBAGENT_MODEL" => "the subagent model field",
        _ => return None,
    })
}

/// Keys present in the live `~/.claude/settings.json` `env` object. Empty when
/// the file is absent or carries no `env` block. Used to detect a custom env key
/// that already exists in the inherited base settings.
pub(crate) fn claude_settings_env_keys() -> Result<Vec<String>> {
    let path = claude_settings_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let settings: serde_json::Value = read_json_file(&path)?;
    Ok(settings["env"]
        .as_object()
        .map(|env| env.keys().cloned().collect())
        .unwrap_or_default())
}

/// Patch `settings.json` `env` with profile's endpoint keys and env map;
/// strip `prev_env_keys` the new profile doesn't carry to clear stale entries.
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
        || !profile.models.is_empty()
        || !prev_env_keys.is_empty();
    if !has_anything && !path.exists() {
        return Ok(());
    }

    let content = build_claude_settings_json(Some(&path), profile, prev_env_keys)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&path, content).context("failed to write settings.json")
}

/// Build the merged settings.json content. `prev_env_keys` are stripped before
/// the new profile's env is applied; pass `&[]` on start to leave existing keys.
/// Also writes the profile's model config — the top-level `model` setting and
/// the `ANTHROPIC_DEFAULT_*_MODEL` / `CLAUDE_CODE_SUBAGENT_MODEL` env keys —
/// each set when present and removed when unset, so a switch never inherits the
/// previous profile's model routing.
///
/// `base` is the settings file to merge onto; `None` (or a missing path) starts
/// from an empty object — used for an isolated runtime that must carry no
/// operator settings.
pub(crate) fn build_claude_settings_json(
    base: Option<&Path>,
    profile: &Profile,
    prev_env_keys: &[String],
) -> Result<String> {
    let mut settings: serde_json::Value = match base {
        Some(p) if p.exists() => read_json_file(p)?,
        _ => serde_json::json!({}),
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

    // Model-tier and subagent overrides — clauth-owned env keys, always set or
    // cleared deterministically so a switch never inherits the prior profile's.
    let model_env = [
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", &profile.models.opus),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", &profile.models.sonnet),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", &profile.models.haiku),
        ("CLAUDE_CODE_SUBAGENT_MODEL", &profile.models.subagent),
    ];
    for (key, value) in model_env {
        match value {
            Some(v) => {
                env.insert(key.into(), v.clone().into());
            }
            None => {
                env.remove(key);
            }
        }
    }

    // Profile env last: explicit ANTHROPIC_* entries win over base_url/api_key.
    for (k, v) in &profile.env {
        env.insert(k.clone(), v.clone().into());
    }

    // Top-level `model` setting (not env). The `env` borrow above has ended, so
    // `settings` is free to mutate again.
    let obj = settings
        .as_object_mut()
        .context("settings.json is not an object")?;
    match profile.models.default.as_deref() {
        Some(model) => {
            obj.insert("model".into(), model.into());
        }
        None => {
            obj.remove("model");
        }
    }

    serde_json::to_string_pretty(&settings).context("failed to serialize settings.json")
}

/// Save live `.credentials.json` into the active profile. No-op on divergence
/// (would silently overwrite stored identity); divergence is resolved via
/// `force_snapshot_active_credentials` after user confirmation. First-login
/// on a credential-less profile is adopted instead.
pub(crate) fn snapshot_active_credentials(config: &mut AppConfig) -> Result<()> {
    with_state_lock(|| {
        let Some(active) = config.state.active_profile.clone() else {
            return Ok(());
        };
        if matches!(classify_credentials_link(&active)?, LinkState::Diverged) {
            if is_first_login(&active)? {
                adopt_first_login(config, &active)?;
            }
            return Ok(());
        }
        snapshot_active_credentials_unchecked(config, &active)
    })
}

/// Store the live `.credentials.json` into the profile then replace it with a
/// symlink. Must only be called after `is_first_login` returns true.
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

/// Snapshot the live `.credentials.json` into the active profile unconditionally.
pub(crate) fn force_snapshot_active_credentials(config: &mut AppConfig) -> Result<()> {
    with_state_lock(|| {
        let Some(active) = config.state.active_profile.clone() else {
            return Ok(());
        };
        snapshot_active_credentials_unchecked(config, &active)
    })
}

/// Re-link `.credentials.json` to `name`'s stored credentials, overwriting the live path.
pub(crate) fn force_link_profile_credentials(name: &str) -> Result<()> {
    with_state_lock(|| {
        let link = claude_credentials_path()?;
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).context("failed to remove .credentials.json")?;
        }
        let target = profile_dir(name)?.join("credentials.json");
        if target.exists() {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            create_symlink(&target, &link)?;
            // macOS: make the switch real — Claude Code reads the Keychain.
            #[cfg(target_os = "macos")]
            if crate::keychain::enabled() {
                keychain_write_profile(name)?;
            }
        }
        Ok(())
    })
}

/// True when both sides have an OAuth block and access or refresh token differs.
/// Missing data on either side returns false (snapshot/skip is safer than guessing).
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

/// Replace the symlink at `.credentials.json` with a regular file (same bytes).
/// No-op if already a regular file or absent. Prevents CC writes from bleeding
/// into the profile's storage after the user disowns the active profile.
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
            std::fs::read(&path).context("failed to read .credentials.json before detach")?;
        std::fs::remove_file(&path).context("failed to remove .credentials.json symlink")?;
        atomic_write_600(&path, content).context("failed to write detached .credentials.json")?;
        Ok(())
    })
}

#[cfg(test)]
#[path = "../tests/inline/claude.rs"]
mod tests;
