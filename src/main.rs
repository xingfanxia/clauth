use anyhow::{Context, Result, bail};
use inquire::{Confirm, InquireError, Select, Text};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ── Data model ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Profile {
    name: String,
    base_url: Option<String>,
    api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credentials: Option<serde_json::Value>,
}

impl Profile {
    fn new(name: String, base_url: Option<String>, api_key: Option<String>) -> Self {
        Self {
            name,
            base_url,
            api_key,
            credentials: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct AppConfig {
    active_profile: Option<String>,
    profiles: Vec<Profile>,
}

impl AppConfig {
    fn load(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))
    }

    fn save(&self, path: &PathBuf) -> Result<()> {
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))
    }

    fn find_profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    fn find_profile_mut(&mut self, name: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.name == name)
    }

    fn is_active(&self, name: &str) -> bool {
        self.active_profile.as_deref() == Some(name)
    }
}

// ── Path helpers ──────────────────────────────────────────────────────────────

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("Cannot determine home directory")
}

fn app_config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".clauth.json"))
}

fn claude_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude"))
}

fn credentials_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join(".credentials.json"))
}

fn settings_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join("settings.json"))
}

// ── File operations ───────────────────────────────────────────────────────────

fn read_credentials() -> Result<Option<serde_json::Value>> {
    let path = credentials_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path).context("Failed to read .credentials.json")?;
    serde_json::from_str(&content)
        .context("Failed to parse .credentials.json")
        .map(Some)
}

fn write_credentials(credentials: &Option<serde_json::Value>) -> Result<()> {
    let path = credentials_path()?;
    match credentials {
        Some(creds) => {
            std::fs::write(&path, serde_json::to_string_pretty(creds)?)
                .context("Failed to write .credentials.json")?;
        }
        None => {
            if path.exists() {
                std::fs::remove_file(&path).context("Failed to remove .credentials.json")?;
            }
        }
    }
    Ok(())
}

/// Reads the current ANTHROPIC_BASE_URL and ANTHROPIC_AUTH_TOKEN from settings.json.
fn read_active_endpoint_config() -> Result<(Option<String>, Option<String>)> {
    let path = settings_path()?;
    if !path.exists() {
        return Ok((None, None));
    }
    let content = std::fs::read_to_string(&path).context("Failed to read settings.json")?;
    let settings: serde_json::Value =
        serde_json::from_str(&content).context("Failed to parse settings.json")?;

    let base_url = settings["env"]["ANTHROPIC_BASE_URL"]
        .as_str()
        .map(str::to_owned);
    let api_key = settings["env"]["ANTHROPIC_AUTH_TOKEN"]
        .as_str()
        .map(str::to_owned);

    Ok((base_url, api_key))
}

/// Patches only ANTHROPIC_BASE_URL and ANTHROPIC_AUTH_TOKEN in settings.json.
/// Every other key is left untouched.
fn apply_endpoint_to_settings(base_url: Option<&str>, api_key: Option<&str>) -> Result<()> {
    let path = settings_path()?;

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
        Some(url) => {
            env.insert("ANTHROPIC_BASE_URL".into(), url.into());
        }
        None => {
            env.remove("ANTHROPIC_BASE_URL");
        }
    }
    match api_key {
        Some(key) => {
            env.insert("ANTHROPIC_AUTH_TOKEN".into(), key.into());
        }
        None => {
            env.remove("ANTHROPIC_AUTH_TOKEN");
        }
    }

    std::fs::write(&path, serde_json::to_string_pretty(&settings)?)
        .context("Failed to write settings.json")
}

/// Snapshots the live .credentials.json into the currently active profile.
/// Call before switching away from a profile.
fn snapshot_active_credentials(config: &mut AppConfig) -> Result<()> {
    let Some(active_name) = config.active_profile.clone() else {
        return Ok(());
    };
    let credentials = read_credentials()?;
    if let Some(profile) = config.find_profile_mut(&active_name) {
        profile.credentials = credentials;
    }
    Ok(())
}

// ── Display helpers ───────────────────────────────────────────────────────────

fn format_profile_entry(profile: &Profile, is_active: bool, name_width: usize) -> String {
    let marker = if is_active { "▶" } else { " " };

    let endpoint = match &profile.base_url {
        Some(url) => url.as_str(),
        None => "Claude Pro / OAuth",
    };

    let key_hint = if profile.base_url.is_some() && profile.api_key.is_some() {
        " · API key set"
    } else {
        ""
    };

    let cred_hint = if profile.credentials.is_some() {
        ""
    } else {
        " · no credentials"
    };

    format!(
        "{marker} {name:<width$}  {endpoint}{key_hint}{cred_hint}",
        name = profile.name,
        width = name_width,
    )
}

// ── Prompts ───────────────────────────────────────────────────────────────────

fn prompt_optional(label: &str, current: Option<&str>) -> Result<Option<String>> {
    let value = Text::new(label)
        .with_default(current.unwrap_or(""))
        .with_help_message("Leave empty to unset")
        .prompt()?;
    Ok(if value.trim().is_empty() {
        None
    } else {
        Some(value)
    })
}

fn prompt_profile_name(existing_names: &[&str], exclude: Option<&str>) -> Result<String> {
    let name = Text::new("Profile name:").prompt()?;
    let name = name.trim().to_string();

    if name.is_empty() {
        bail!("Name cannot be empty.");
    }
    let taken = existing_names
        .iter()
        .any(|&n| n == name && Some(n) != exclude);
    if taken {
        bail!("A profile named '{}' already exists.", name);
    }
    Ok(name)
}

// ── Cancellation helpers ──────────────────────────────────────────────────────

fn is_cancelled(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<InquireError>()
        .map(|e| {
            matches!(
                e,
                InquireError::OperationCanceled | InquireError::OperationInterrupted
            )
        })
        .unwrap_or(false)
}

// ── Individual actions ────────────────────────────────────────────────────────

fn do_switch(config: &mut AppConfig, name: &str) -> Result<()> {
    if config.is_active(name) {
        println!("Already active.");
        return Ok(());
    }

    snapshot_active_credentials(config)?;

    let (target_credentials, target_base_url, target_api_key) = {
        let profile = config.find_profile(name).context("Profile not found")?;
        (
            profile.credentials.clone(),
            profile.base_url.clone(),
            profile.api_key.clone(),
        )
    };

    write_credentials(&target_credentials)?;
    apply_endpoint_to_settings(target_base_url.as_deref(), target_api_key.as_deref())?;
    config.active_profile = Some(name.to_string());

    println!("Switched to '{}'.", name);

    if target_credentials.is_none() {
        println!(
            "No saved credentials — run `claude` to authenticate, \
             then come back to save them."
        );
    }

    Ok(())
}

fn do_edit(config: &mut AppConfig, name: &str) -> Result<()> {
    let (current_base_url, current_api_key) = {
        let profile = config.find_profile(name).context("Profile not found")?;
        (profile.base_url.clone(), profile.api_key.clone())
    };

    let base_url = prompt_optional("Base URL:", current_base_url.as_deref())?;
    let api_key = prompt_optional("API key:", current_api_key.as_deref())?;

    {
        let profile = config.find_profile_mut(name).context("Profile not found")?;
        profile.base_url = base_url.clone();
        profile.api_key = api_key.clone();
    }

    if config.is_active(name) {
        apply_endpoint_to_settings(base_url.as_deref(), api_key.as_deref())?;
        println!("Updated and applied to settings.json.");
    } else {
        println!("Updated.");
    }

    Ok(())
}

/// Returns true if the rename completed (so the submenu can exit to refresh the list).
fn do_rename(config: &mut AppConfig, old_name: &str) -> Result<bool> {
    let all_names: Vec<&str> = config.profiles.iter().map(|p| p.name.as_str()).collect();
    let new_name = match prompt_profile_name(&all_names, Some(old_name)) {
        Ok(n) => n,
        Err(e) if is_cancelled(&e) => return Ok(false),
        Err(e) => return Err(e),
    };

    if let Some(profile) = config.find_profile_mut(old_name) {
        profile.name = new_name.clone();
    }
    if config.active_profile.as_deref() == Some(old_name) {
        config.active_profile = Some(new_name.clone());
    }

    println!("Renamed to '{}'.", new_name);
    Ok(true)
}

/// Returns true if the profile was deleted (so the submenu can exit).
fn do_delete(config: &mut AppConfig, name: &str) -> Result<bool> {
    let confirmed = match Confirm::new(&format!("Delete '{}'?", name))
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
    config.profiles.retain(|p| p.name != name);

    if was_active {
        config.active_profile = None;
        println!(
            "Deleted '{}'. No active profile — run clauth to switch.",
            name
        );
    } else {
        println!("Deleted '{}'.", name);
    }

    Ok(true)
}

fn action_new_blank(config: &mut AppConfig) -> Result<()> {
    let all_names: Vec<&str> = config.profiles.iter().map(|p| p.name.as_str()).collect();
    let name = prompt_profile_name(&all_names, None)?;

    let base_url = prompt_optional("Base URL:", None)?;
    let api_key = if base_url.is_some() {
        prompt_optional("API key:", None)?
    } else {
        None
    };

    config
        .profiles
        .push(Profile::new(name.clone(), base_url, api_key));
    println!("Created '{}'.", name);
    Ok(())
}

/// Creates a new profile from the current live .credentials.json and settings.json values.
fn action_capture_current(config: &mut AppConfig) -> Result<()> {
    let credentials = read_credentials()?;
    let (base_url, api_key) = read_active_endpoint_config()?;

    let url_display = base_url.as_deref().unwrap_or("Claude Pro / OAuth");
    let cred_display = if credentials.is_some() {
        "credentials found"
    } else {
        "no credentials"
    };
    println!("Current state: {url_display} · {cred_display}");

    let all_names: Vec<&str> = config.profiles.iter().map(|p| p.name.as_str()).collect();
    let name = prompt_profile_name(&all_names, None)?;

    let mut profile = Profile::new(name.clone(), base_url, api_key);
    profile.credentials = credentials;
    config.profiles.push(profile);

    if config.active_profile.is_none() {
        config.active_profile = Some(name.clone());
        println!("Captured as '{}' and set as active.", name);
    } else {
        println!("Captured as '{}'.", name);
    }

    Ok(())
}

// ── Profile submenu ───────────────────────────────────────────────────────────

// inquire's Select has no non-selectable items — every entry in the Vec is
// reachable by the cursor. Separators are therefore not usable. Items here are
// visually distinct from the main menu through their wording alone.

const SUB_SWITCH: &str = "Switch to this profile";
const SUB_SWITCH_ACTIVE: &str = "Switch to this profile  (already active)";
const SUB_EDIT: &str = "Edit  (URL / API key)";
const SUB_RENAME: &str = "Rename";
const SUB_DELETE: &str = "Delete";
const SUB_BACK: &str = "← Back";

fn profile_submenu(config: &mut AppConfig, profile_name: &str) -> Result<()> {
    loop {
        let (is_active, title) = {
            let profile = match config.find_profile(profile_name) {
                Some(p) => p,
                None => return Ok(()),
            };
            let url = profile.base_url.as_deref().unwrap_or("Claude Pro / OAuth");
            let creds = if profile.credentials.is_some() {
                "credentials saved"
            } else {
                "no credentials"
            };
            (
                config.is_active(profile_name),
                format!("{} · {} · {}", profile.name, url, creds),
            )
        };

        let switch_label = if is_active {
            SUB_SWITCH_ACTIVE
        } else {
            SUB_SWITCH
        };

        let options = vec![switch_label, SUB_EDIT, SUB_RENAME, SUB_DELETE, SUB_BACK];

        let choice = match Select::new(&title, options).without_filtering().prompt() {
            Ok(c) => c,
            Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        let result = if choice == SUB_SWITCH || choice == SUB_SWITCH_ACTIVE {
            do_switch(config, profile_name).map(|_| false)
        } else if choice == SUB_EDIT {
            do_edit(config, profile_name).map(|_| false)
        } else if choice == SUB_RENAME {
            do_rename(config, profile_name)
        } else if choice == SUB_DELETE {
            do_delete(config, profile_name)
        } else {
            return Ok(()); // Back
        };

        match result {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) if is_cancelled(&e) => {}
            Err(e) => eprintln!("Error: {e:#}"),
        }
    }
}

// ── Main menu ─────────────────────────────────────────────────────────────────

// inquire's Select has no non-selectable items — separators cannot exist.
// Visual grouping is achieved through display format: profile entries carry a
// ▶/space marker and inline URL info; action items start with `+` or are
// plain single words.

const MENU_NEW: &str = "+ New profile";
const MENU_CAPTURE: &str = "+ Capture current as new profile";
const MENU_QUIT: &str = "Quit";

fn main() -> Result<()> {
    let config_path = app_config_path()?;
    let mut config = AppConfig::load(&config_path)?;

    loop {
        let name_width = config
            .profiles
            .iter()
            .map(|p| p.name.len())
            .max()
            .unwrap_or(0)
            .max(4);

        let profile_displays: Vec<String> = config
            .profiles
            .iter()
            .map(|p| format_profile_entry(p, config.is_active(&p.name), name_width))
            .collect();

        let profile_names: Vec<String> = config.profiles.iter().map(|p| p.name.clone()).collect();

        let mut options: Vec<&str> = profile_displays.iter().map(String::as_str).collect();
        options.push(MENU_NEW);
        options.push(MENU_CAPTURE);
        options.push(MENU_QUIT);

        let choice = match Select::new("clauth", options).without_filtering().prompt() {
            Ok(c) => c,
            Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => break,
            Err(e) => return Err(e.into()),
        };

        if choice == MENU_QUIT {
            break;
        }

        let action_result = if choice == MENU_NEW {
            action_new_blank(&mut config)
        } else if choice == MENU_CAPTURE {
            action_capture_current(&mut config)
        } else if let Some(idx) = profile_displays.iter().position(|d| d == choice) {
            let name = profile_names[idx].clone();
            profile_submenu(&mut config, &name)
        } else {
            Ok(())
        };

        if let Err(e) = action_result
            && !is_cancelled(&e) {
                eprintln!("Error: {e:#}");
            }

        config.save(&config_path)?;
        println!();
    }

    config.save(&config_path)?;
    Ok(())
}
