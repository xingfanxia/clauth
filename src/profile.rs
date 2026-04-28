use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClaudeCredentials {
    #[serde(rename = "claudeAiOauth", skip_serializing_if = "Option::is_none")]
    pub(crate) claude_ai_oauth: Option<OAuthToken>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OAuthToken {
    #[serde(rename = "accessToken")]
    pub(crate) access_token: String,
    #[serde(rename = "refreshToken", skip_serializing_if = "Option::is_none")]
    pub(crate) refresh_token: Option<String>,
    #[serde(rename = "expiresAt", skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) scopes: Option<Vec<String>>,
    #[serde(rename = "subscriptionType", skip_serializing_if = "Option::is_none")]
    pub(crate) subscription_type: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Profile {
    pub(crate) name: String,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) credentials: Option<ClaudeCredentials>,
}

impl Profile {
    pub(crate) fn new(name: String, base_url: Option<String>, api_key: Option<String>) -> Self {
        Self { name, base_url, api_key, credentials: None }
    }
}

/// Stored at ~/.clauth/profiles.toml — ordering and active marker only.
/// Credentials and endpoint config live in per-profile subdirectories.
#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct AppState {
    pub(crate) active_profile: Option<String>,
    pub(crate) profiles: Vec<String>,
}

pub(crate) struct AppConfig {
    pub(crate) state: AppState,
    pub(crate) profiles: Vec<Profile>,
}

impl AppConfig {
    pub(crate) fn is_active(&self, name: &str) -> bool {
        self.state.active_profile.as_deref() == Some(name)
    }

    pub(crate) fn find(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    pub(crate) fn find_mut(&mut self, name: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.name == name)
    }

    pub(crate) fn names(&self) -> Vec<&str> {
        self.profiles.iter().map(|p| p.name.as_str()).collect()
    }

    pub(crate) fn add(&mut self, profile: Profile) {
        self.state.profiles.push(profile.name.clone());
        self.profiles.push(profile);
    }

    pub(crate) fn remove(&mut self, name: &str) {
        self.profiles.retain(|p| p.name != name);
        self.state.profiles.retain(|n| n != name);
        if self.is_active(name) {
            self.state.active_profile = None;
        }
    }
}

/// On-disk format for ~/.clauth/profiles/<name>/config.toml
#[derive(Debug, Serialize, Deserialize, Default)]
struct ProfileConfig {
    base_url: Option<String>,
    api_key: Option<String>,
}

// ── Path helpers ──────────────────────────────────────────────────────────────

pub(crate) fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("Cannot determine home directory")
}

fn clauth_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".clauth"))
}

fn profiles_root() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("profiles"))
}

fn app_state_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("profiles.toml"))
}

pub(crate) fn profile_dir(name: &str) -> Result<PathBuf> {
    Ok(profiles_root()?.join(name))
}

fn profile_config_path(name: &str) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join("config.toml"))
}

fn profile_credentials_path(name: &str) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join("credentials.json"))
}

// ── Persistence ───────────────────────────────────────────────────────────────

fn load_app_state() -> Result<AppState> {
    let path = app_state_path()?;
    if !path.exists() {
        return Ok(AppState::default());
    }
    let content = std::fs::read_to_string(&path).context("Failed to read profiles.toml")?;
    toml::from_str(&content).context("Failed to parse profiles.toml")
}

pub(crate) fn save_app_state(state: &AppState) -> Result<()> {
    std::fs::create_dir_all(clauth_dir()?)?;
    std::fs::write(app_state_path()?, toml::to_string_pretty(state)?)
        .context("Failed to write profiles.toml")
}

fn load_profile(name: &str) -> Result<Profile> {
    let config_path = profile_config_path(name)?;
    let config: ProfileConfig = if config_path.exists() {
        let content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read {name}/config.toml"))?;
        toml::from_str(&content)
            .with_context(|| format!("Failed to parse {name}/config.toml"))?
    } else {
        ProfileConfig::default()
    };

    let cred_path = profile_credentials_path(name)?;
    let credentials = if cred_path.exists() {
        let content = std::fs::read_to_string(&cred_path)
            .with_context(|| format!("Failed to read {name}/credentials.json"))?;
        Some(serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {name}/credentials.json"))?)
    } else {
        None
    };

    Ok(Profile {
        name: name.to_string(),
        base_url: config.base_url,
        api_key: config.api_key,
        credentials,
    })
}

pub(crate) fn save_profile(profile: &Profile) -> Result<()> {
    std::fs::create_dir_all(profile_dir(&profile.name)?)?;

    let config_toml = toml::to_string_pretty(&ProfileConfig {
        base_url: profile.base_url.clone(),
        api_key: profile.api_key.clone(),
    })?;
    std::fs::write(profile_config_path(&profile.name)?, config_toml)
        .context("Failed to write config.toml")?;

    let cred_path = profile_credentials_path(&profile.name)?;
    match &profile.credentials {
        Some(creds) => std::fs::write(&cred_path, serde_json::to_string_pretty(creds)?)
            .context("Failed to write credentials.json")?,
        None if cred_path.exists() => std::fs::remove_file(&cred_path)
            .context("Failed to remove credentials.json")?,
        None => {}
    }

    Ok(())
}

pub(crate) fn load_config() -> Result<AppConfig> {
    std::fs::create_dir_all(profiles_root()?)?;
    let state = load_app_state()?;
    let profiles = state.profiles.iter()
        .map(|n| load_profile(n))
        .collect::<Result<Vec<_>>>()?;
    Ok(AppConfig { state, profiles })
}
