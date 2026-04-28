use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::profile::{AppConfig, home_dir, save_profile};

fn claude_credentials_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude").join(".credentials.json"))
}

fn claude_settings_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude").join("settings.json"))
}

pub(crate) fn read_claude_credentials() -> Result<Option<serde_json::Value>> {
    let path = claude_credentials_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path).context("Failed to read .credentials.json")?;
    serde_json::from_str(&content)
        .context("Failed to parse .credentials.json")
        .map(Some)
}

pub(crate) fn write_claude_credentials(credentials: Option<&serde_json::Value>) -> Result<()> {
    let path = claude_credentials_path()?;
    match credentials {
        Some(creds) => std::fs::write(&path, serde_json::to_string_pretty(creds)?)
            .context("Failed to write .credentials.json"),
        None if path.exists() => std::fs::remove_file(&path)
            .context("Failed to remove .credentials.json"),
        None => Ok(()),
    }
}

pub(crate) fn read_claude_endpoint_config() -> Result<(Option<String>, Option<String>)> {
    let path = claude_settings_path()?;
    if !path.exists() {
        return Ok((None, None));
    }
    let content = std::fs::read_to_string(&path).context("Failed to read settings.json")?;
    let settings: serde_json::Value =
        serde_json::from_str(&content).context("Failed to parse settings.json")?;
    Ok((
        settings["env"]["ANTHROPIC_BASE_URL"].as_str().map(str::to_owned),
        settings["env"]["ANTHROPIC_AUTH_TOKEN"].as_str().map(str::to_owned),
    ))
}

/// Patches only ANTHROPIC_BASE_URL and ANTHROPIC_AUTH_TOKEN inside the `env`
/// object of settings.json. Every other key and field is left untouched.
pub(crate) fn apply_endpoint_to_claude_settings(
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<()> {
    let path = claude_settings_path()?;

    let mut settings: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(&path).context("Failed to read settings.json")?;
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

    match base_url {
        Some(url) => { env.insert("ANTHROPIC_BASE_URL".into(), url.into()); }
        None => { env.remove("ANTHROPIC_BASE_URL"); }
    }
    match api_key {
        Some(key) => { env.insert("ANTHROPIC_AUTH_TOKEN".into(), key.into()); }
        None => { env.remove("ANTHROPIC_AUTH_TOKEN"); }
    }

    std::fs::write(&path, serde_json::to_string_pretty(&settings)?)
        .context("Failed to write settings.json")
}

/// Reads the live .credentials.json and saves it to the active profile.
pub(crate) fn snapshot_active_credentials(config: &mut AppConfig) -> Result<()> {
    let Some(active) = config.state.active_profile.clone() else {
        return Ok(());
    };
    let credentials = read_claude_credentials()?;
    if let Some(profile) = config.find_mut(&active) {
        profile.credentials = credentials;
        save_profile(profile)?;
    }
    Ok(())
}
