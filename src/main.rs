use anyhow::{Context, Result, bail};
use inquire::{Confirm, InquireError, Select, Text};
use inquire::ui::{Attributes, Color, RenderConfig, StyleSheet, Styled};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ── Terminal color tokens (cloudy-ui CLI palette) ──────────────────────────

const C_RESET:   &str = "\x1b[0m";
const C_ACCENT:  &str = "\x1b[38;2;67;171;229m";
const C_SUCCESS: &str = "\x1b[38;2;166;227;161m";
const C_WARNING: &str = "\x1b[38;2;249;226;175m";
const C_DIM:     &str = "\x1b[38;2;166;173;200m";
const C_FAINT:   &str = "\x1b[38;2;127;132;156m";
const C_BOLD:    &str = "\x1b[1m";

// ── Data model ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Profile {
    name: String,
    base_url: Option<String>,
    api_key: Option<String>,
    credentials: Option<serde_json::Value>,
}

impl Profile {
    fn new(name: String, base_url: Option<String>, api_key: Option<String>) -> Self {
        Self { name, base_url, api_key, credentials: None }
    }
}

/// Stored at ~/.clauth/profiles.toml — ordering and active marker only.
/// Credentials and endpoint config live in per-profile subdirectories.
#[derive(Debug, Serialize, Deserialize, Default)]
struct AppState {
    active_profile: Option<String>,
    profiles: Vec<String>,
}

struct AppConfig {
    state: AppState,
    profiles: Vec<Profile>,
}

impl AppConfig {
    fn is_active(&self, name: &str) -> bool {
        self.state.active_profile.as_deref() == Some(name)
    }

    fn find_profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    fn find_profile_mut(&mut self, name: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.name == name)
    }
}

/// On-disk format for ~/.clauth/profiles/<name>/config.toml
#[derive(Debug, Serialize, Deserialize, Default)]
struct ProfileConfig {
    base_url: Option<String>,
    api_key: Option<String>,
}

// ── Path helpers ──────────────────────────────────────────────────────────────

fn home_dir() -> Result<PathBuf> {
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

fn profile_dir(name: &str) -> Result<PathBuf> {
    Ok(profiles_root()?.join(name))
}

fn profile_config_path(name: &str) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join("config.toml"))
}

fn profile_credentials_path(name: &str) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join("credentials.json"))
}

fn claude_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude"))
}

fn claude_credentials_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join(".credentials.json"))
}

fn claude_settings_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join("settings.json"))
}

// ── Profile persistence ───────────────────────────────────────────────────────

fn load_app_state() -> Result<AppState> {
    let path = app_state_path()?;
    if !path.exists() {
        return Ok(AppState::default());
    }
    let content = std::fs::read_to_string(&path).context("Failed to read profiles.toml")?;
    toml::from_str(&content).context("Failed to parse profiles.toml")
}

fn save_app_state(state: &AppState) -> Result<()> {
    let path = app_state_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, toml::to_string_pretty(state)?)
        .context("Failed to write profiles.toml")
}

fn load_profile(name: &str) -> Result<Profile> {
    let config_path = profile_config_path(name)?;
    let config: ProfileConfig = if config_path.exists() {
        let content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read {}/config.toml", name))?;
        toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}/config.toml", name))?
    } else {
        ProfileConfig::default()
    };

    let cred_path = profile_credentials_path(name)?;
    let credentials = if cred_path.exists() {
        let content = std::fs::read_to_string(&cred_path)
            .with_context(|| format!("Failed to read {}/credentials.json", name))?;
        Some(
            serde_json::from_str(&content)
                .with_context(|| format!("Failed to parse {}/credentials.json", name))?,
        )
    } else {
        None
    };

    Ok(Profile { name: name.to_string(), base_url: config.base_url, api_key: config.api_key, credentials })
}

fn save_profile(profile: &Profile) -> Result<()> {
    let dir = profile_dir(&profile.name)?;
    std::fs::create_dir_all(&dir)?;

    let config_toml = toml::to_string_pretty(&ProfileConfig {
        base_url: profile.base_url.clone(),
        api_key: profile.api_key.clone(),
    })?;
    std::fs::write(profile_config_path(&profile.name)?, config_toml)
        .context("Failed to write config.toml")?;

    let cred_path = profile_credentials_path(&profile.name)?;
    match &profile.credentials {
        Some(creds) => {
            std::fs::write(&cred_path, serde_json::to_string_pretty(creds)?)
                .context("Failed to write credentials.json")?;
        }
        None => {
            if cred_path.exists() {
                std::fs::remove_file(&cred_path).context("Failed to remove credentials.json")?;
            }
        }
    }

    Ok(())
}

fn load_config() -> Result<AppConfig> {
    std::fs::create_dir_all(profiles_root()?)?;
    let state = load_app_state()?;
    let profiles = state
        .profiles
        .iter()
        .map(|name| load_profile(name))
        .collect::<Result<Vec<_>>>()?;
    Ok(AppConfig { state, profiles })
}

// ── Claude file operations ────────────────────────────────────────────────────

fn read_claude_credentials() -> Result<Option<serde_json::Value>> {
    let path = claude_credentials_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path).context("Failed to read .credentials.json")?;
    serde_json::from_str(&content)
        .context("Failed to parse .credentials.json")
        .map(Some)
}

fn write_claude_credentials(credentials: &Option<serde_json::Value>) -> Result<()> {
    let path = claude_credentials_path()?;
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

fn read_claude_endpoint_config() -> Result<(Option<String>, Option<String>)> {
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
fn apply_endpoint_to_claude_settings(base_url: Option<&str>, api_key: Option<&str>) -> Result<()> {
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

/// Reads the live .credentials.json and saves it to the active profile's directory.
fn snapshot_active_credentials(config: &mut AppConfig) -> Result<()> {
    let Some(active_name) = config.state.active_profile.clone() else {
        return Ok(());
    };
    let credentials = read_claude_credentials()?;
    if let Some(profile) = config.find_profile_mut(&active_name) {
        profile.credentials = credentials;
        save_profile(profile)?;
    }
    Ok(())
}

// ── Display helpers ───────────────────────────────────────────────────────────

fn format_profile_entry(profile: &Profile, is_active: bool, name_width: usize) -> String {
    let acc   = C_ACCENT;
    let dim   = C_DIM;
    let faint = C_FAINT;
    let warn  = C_WARNING;
    let rst   = C_RESET;

    let endpoint = match &profile.base_url {
        Some(url) => url.as_str(),
        None => "Claude Pro / OAuth",
    };

    let key_hint = if profile.base_url.is_some() && profile.api_key.is_some() {
        format!("{faint} · API key set{rst}")
    } else {
        String::new()
    };

    let cred_display = if profile.credentials.is_some() {
        String::new()
    } else {
        format!("{warn} · no credentials{rst}")
    };

    if is_active {
        format!(
            "{acc}● {name:<width$}{rst}  {dim}{endpoint}{rst}{key_hint}{cred_display}",
            name = profile.name,
            width = name_width,
        )
    } else {
        format!(
            "  {name:<width$}  {dim}{endpoint}{rst}{key_hint}{cred_display}",
            name = profile.name,
            width = name_width,
        )
    }
}

// ── Prompts ───────────────────────────────────────────────────────────────────

fn prompt_optional(label: &str, current: Option<&str>) -> Result<Option<String>> {
    let value = Text::new(label)
        .with_default(current.unwrap_or(""))
        .with_help_message("Leave empty to unset")
        .prompt()?;
    Ok(if value.trim().is_empty() { None } else { Some(value) })
}

fn prompt_profile_name(existing_names: &[&str], exclude: Option<&str>) -> Result<String> {
    let name = Text::new("Profile name:").prompt()?;
    let name = name.trim().to_string();

    if name.is_empty() {
        bail!("Name cannot be empty.");
    }
    if existing_names.iter().any(|&n| n == name && Some(n) != exclude) {
        bail!("A profile named '{}' already exists.", name);
    }
    Ok(name)
}

// ── Cancellation helpers ──────────────────────────────────────────────────────

fn is_cancelled(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<InquireError>()
        .map(|e| matches!(e, InquireError::OperationCanceled | InquireError::OperationInterrupted))
        .unwrap_or(false)
}

// ── Individual actions ────────────────────────────────────────────────────────

fn do_switch(config: &mut AppConfig, name: &str) -> Result<()> {
    if config.is_active(name) {
        return Ok(());
    }

    snapshot_active_credentials(config)?;

    let (target_credentials, target_base_url, target_api_key) = {
        let profile = config.find_profile(name).context("Profile not found")?;
        (profile.credentials.clone(), profile.base_url.clone(), profile.api_key.clone())
    };

    write_claude_credentials(&target_credentials)?;
    apply_endpoint_to_claude_settings(target_base_url.as_deref(), target_api_key.as_deref())?;
    config.state.active_profile = Some(name.to_string());
    save_app_state(&config.state)?;

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
        save_profile(profile)?;
    }

    if config.is_active(name) {
        apply_endpoint_to_claude_settings(base_url.as_deref(), api_key.as_deref())?;
    }

    Ok(())
}

/// Returns true if the profile was renamed (submenu should exit to refresh list).
fn do_rename(config: &mut AppConfig, old_name: &str) -> Result<bool> {
    let all_names: Vec<&str> = config.profiles.iter().map(|p| p.name.as_str()).collect();
    let new_name = match prompt_profile_name(&all_names, Some(old_name)) {
        Ok(n) => n,
        Err(e) if is_cancelled(&e) => return Ok(false),
        Err(e) => return Err(e),
    };

    let old_dir = profile_dir(old_name)?;
    let new_dir = profile_dir(&new_name)?;
    if old_dir.exists() {
        std::fs::rename(&old_dir, &new_dir)
            .with_context(|| format!("Failed to rename profile directory to '{}'", new_name))?;
    }

    if let Some(profile) = config.find_profile_mut(old_name) {
        profile.name = new_name.clone();
    }
    if let Some(slot) = config.state.profiles.iter_mut().find(|n| n.as_str() == old_name) {
        *slot = new_name.clone();
    }
    if config.state.active_profile.as_deref() == Some(old_name) {
        config.state.active_profile = Some(new_name.clone());
    }

    save_app_state(&config.state)?;
    Ok(true)
}

/// Returns true if the profile was deleted (submenu should exit).
fn do_delete(config: &mut AppConfig, name: &str) -> Result<bool> {
    let confirmed = match Confirm::new(&format!("Delete '{}'?", name))
        .with_default(false)
        .prompt()
    {
        Ok(c) => c,
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => return Ok(false),
        Err(e) => return Err(e.into()),
    };

    if !confirmed {
        return Ok(false);
    }

    let dir = profile_dir(name)?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("Failed to delete profile directory for '{}'", name))?;
    }

    let was_active = config.is_active(name);
    config.profiles.retain(|p| p.name != name);
    config.state.profiles.retain(|n| n != name);
    if was_active {
        config.state.active_profile = None;
    }

    save_app_state(&config.state)?;
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

    let profile = Profile::new(name.clone(), base_url, api_key);
    save_profile(&profile)?;
    config.profiles.push(profile);
    config.state.profiles.push(name);
    save_app_state(&config.state)?;

    Ok(())
}

fn action_capture_current(config: &mut AppConfig) -> Result<()> {
    let credentials = read_claude_credentials()?;
    let (base_url, api_key) = read_claude_endpoint_config()?;

    let all_names: Vec<&str> = config.profiles.iter().map(|p| p.name.as_str()).collect();
    let name = prompt_profile_name(&all_names, None)?;

    let mut profile = Profile::new(name.clone(), base_url, api_key);
    profile.credentials = credentials;
    save_profile(&profile)?;
    config.profiles.push(profile);
    config.state.profiles.push(name.clone());

    if config.state.active_profile.is_none() {
        config.state.active_profile = Some(name);
    }

    save_app_state(&config.state)?;
    Ok(())
}

// ── Profile submenu ───────────────────────────────────────────────────────────

const SUB_SWITCH:        &str = "Switch to this profile";
const SUB_SWITCH_ACTIVE: &str = "Switch to this profile  \x1b[38;2;127;132;156m(already active)\x1b[0m";
const SUB_EDIT:          &str = "Edit  \x1b[38;2;166;173;200m(URL / API key)\x1b[0m";
const SUB_RENAME:        &str = "Rename";
const SUB_DELETE:        &str = "\x1b[38;2;243;139;168mDelete\x1b[0m";
const SUB_BACK:          &str = "\x1b[38;2;127;132;156m← Back\x1b[0m";

fn profile_submenu(config: &mut AppConfig, profile_name: &str) -> Result<()> {
    loop {
        let (is_active, title) = {
            let profile = match config.find_profile(profile_name) {
                Some(p) => p,
                None => return Ok(()),
            };
            let url = profile.base_url.as_deref().unwrap_or("Claude Pro / OAuth");
            let (cred_color, creds) = if profile.credentials.is_some() {
                (C_SUCCESS, "credentials saved")
            } else {
                (C_WARNING, "no credentials")
            };
            let bold  = C_BOLD;
            let faint = C_FAINT;
            let dim   = C_DIM;
            let rst   = C_RESET;
            let name  = &profile.name;
            let title = format!(
                "{bold}{name}{rst}{faint} · {rst}{dim}{url}{faint} · {rst}{cred_color}{creds}{rst}"
            );
            (config.is_active(profile_name), title)
        };

        let switch_label = if is_active { SUB_SWITCH_ACTIVE } else { SUB_SWITCH };
        let options = vec![switch_label, SUB_EDIT, SUB_RENAME, SUB_DELETE, SUB_BACK];

        let choice = match Select::new(&title, options).without_filtering().prompt() {
            Ok(c) => c,
            Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        if choice == SUB_BACK {
            return Ok(());
        }

        let result: Result<bool> = if choice == SUB_SWITCH || choice == SUB_SWITCH_ACTIVE {
            do_switch(config, profile_name).map(|_| true)
        } else if choice == SUB_EDIT {
            do_edit(config, profile_name).map(|_| false)
        } else if choice == SUB_RENAME {
            do_rename(config, profile_name)
        } else {
            do_delete(config, profile_name)
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

const MENU_NEW:     &str = "\x1b[38;2;217;119;87m+\x1b[0m New profile";
const MENU_CAPTURE: &str = "\x1b[38;2;217;119;87m+\x1b[0m New from current profile";
const MENU_QUIT:    &str = "\x1b[38;2;127;132;156mQuit\x1b[0m";

fn build_render_config() -> RenderConfig<'static> {
    let orange = Color::Rgb { r: 217, g: 119, b: 87 };
	let blue = Color::Rgb { r: 67, g: 171, b: 229 };
    let faint  = Color::Rgb { r: 127, g: 132, b: 156 };

    RenderConfig::default()
        .with_prompt_prefix(Styled::new("?").with_fg(blue))
        .with_answered_prompt_prefix(Styled::new("?").with_fg(faint))
        .with_highlighted_option_prefix(Styled::new("▶").with_fg(orange))
        .with_selected_option(Some(StyleSheet::new().with_attr(Attributes::BOLD)))
        .with_answer(StyleSheet::new().with_fg(blue))
        .with_help_message(StyleSheet::new().with_fg(blue))
}

fn main() -> Result<()> {
    inquire::set_global_render_config(build_render_config());
    let mut config = load_config()?;

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

        let result = if choice == MENU_NEW {
            action_new_blank(&mut config)
        } else if choice == MENU_CAPTURE {
            action_capture_current(&mut config)
        } else if let Some(idx) = profile_displays.iter().position(|d| d == choice) {
            let name = profile_names[idx].clone();
            profile_submenu(&mut config, &name)
        } else {
            Ok(())
        };

        if let Err(e) = result {
            if !is_cancelled(&e) {
                return Err(e);
            }
        }
    }

    Ok(())
}
