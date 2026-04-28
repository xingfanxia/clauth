use anyhow::{Context, Result, bail};
use inquire::ui::{Attributes, Color, RenderConfig, StyleSheet, Styled};
use inquire::{Confirm, InquireError, Select, Text};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ── Terminal palette (cloudy-ui CLI) ──────────────────────────────────────────

const C_RESET:   &str = "\x1b[0m";
const C_BOLD:    &str = "\x1b[1m";
// Targeted resets — used so inquire's "selected = bold" wrapper does not
// either leak through the whole label or get killed by an early full reset.
const C_NOBOLD:  &str = "\x1b[22m"; // normal intensity, keeps current color
const C_FG_OFF:  &str = "\x1b[39m"; // default foreground, keeps current attrs
const C_ACCENT:  &str = "\x1b[38;2;67;171;229m";   // sapphire
const C_ORANGE:  &str = "\x1b[38;2;217;119;87m";   // claude orange
const C_SUCCESS: &str = "\x1b[38;2;166;227;161m";
const C_WARNING: &str = "\x1b[38;2;249;226;175m";
const C_DANGER:  &str = "\x1b[38;2;243;139;168m";
const C_DIM:     &str = "\x1b[38;2;166;173;200m";
const C_FAINT:   &str = "\x1b[38;2;127;132;156m";

const ENDPOINT_DEFAULT: &str = "Claude Pro / OAuth";

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

    fn find(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    fn find_mut(&mut self, name: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.name == name)
    }

    fn names(&self) -> Vec<&str> {
        self.profiles.iter().map(|p| p.name.as_str()).collect()
    }

    fn add(&mut self, profile: Profile) {
        self.state.profiles.push(profile.name.clone());
        self.profiles.push(profile);
    }

    fn remove(&mut self, name: &str) {
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

fn claude_credentials_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude").join(".credentials.json"))
}

fn claude_settings_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude").join("settings.json"))
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

fn save_profile(profile: &Profile) -> Result<()> {
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

fn load_config() -> Result<AppConfig> {
    std::fs::create_dir_all(profiles_root()?)?;
    let state = load_app_state()?;
    let profiles = state.profiles.iter()
        .map(|n| load_profile(n))
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

fn write_claude_credentials(credentials: Option<&serde_json::Value>) -> Result<()> {
    let path = claude_credentials_path()?;
    match credentials {
        Some(creds) => std::fs::write(&path, serde_json::to_string_pretty(creds)?)
            .context("Failed to write .credentials.json"),
        None if path.exists() => std::fs::remove_file(&path)
            .context("Failed to remove .credentials.json"),
        None => Ok(()),
    }
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

/// Reads the live .credentials.json and saves it to the active profile.
fn snapshot_active_credentials(config: &mut AppConfig) -> Result<()> {
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

// ── Display helpers ───────────────────────────────────────────────────────────

fn format_profile_entry(profile: &Profile, is_active: bool, name_width: usize) -> String {
    let endpoint = profile.base_url.as_deref().unwrap_or(ENDPOINT_DEFAULT);
    let key_hint = if profile.base_url.is_some() && profile.api_key.is_some() {
        format!("{C_FAINT} · API key set{C_RESET}")
    } else {
        String::new()
    };
    let cred_warn = if profile.credentials.is_none() {
        format!("{C_WARNING} · no credentials{C_RESET}")
    } else {
        String::new()
    };
    let name = &profile.name;

    if is_active {
        format!("{C_ACCENT}● {name:<name_width$}{C_NOBOLD}  {C_DIM}{endpoint}{C_RESET}{key_hint}{cred_warn}")
    } else {
        format!("  {name:<name_width$}{C_NOBOLD}  {C_DIM}{endpoint}{C_RESET}{key_hint}{cred_warn}")
    }
}

fn format_submenu_title(profile: &Profile) -> String {
    let url = profile.base_url.as_deref().unwrap_or(ENDPOINT_DEFAULT);
    let (cred_color, creds) = if profile.credentials.is_some() {
        (C_SUCCESS, "credentials saved")
    } else {
        (C_WARNING, "no credentials")
    };
    let name = &profile.name;
    format!(
        "{C_BOLD}{name}{C_RESET}{C_FAINT} · {C_RESET}{C_DIM}{url}{C_FAINT} · {C_RESET}{cred_color}{creds}{C_RESET}"
    )
}

// ── Prompts ───────────────────────────────────────────────────────────────────

fn prompt_optional(label: &str, current: Option<&str>) -> Result<Option<String>> {
    let value = Text::new(label)
        .with_default(current.unwrap_or(""))
        .with_help_message("Leave empty to unset")
        .prompt()?;
    Ok((!value.trim().is_empty()).then_some(value))
}

fn prompt_profile_name(existing: &[&str], exclude: Option<&str>) -> Result<String> {
    let name = Text::new("Profile name:").prompt()?.trim().to_string();
    if name.is_empty() {
        bail!("Name cannot be empty.");
    }
    if existing.iter().any(|&n| n == name && Some(n) != exclude) {
        bail!("A profile named '{name}' already exists.");
    }
    Ok(name)
}

// ── Cancellation helpers ──────────────────────────────────────────────────────

fn is_cancelled(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<InquireError>(),
        Some(InquireError::OperationCanceled | InquireError::OperationInterrupted),
    )
}

// ── Individual actions ────────────────────────────────────────────────────────

fn do_switch(config: &mut AppConfig, name: &str) -> Result<()> {
    if config.is_active(name) {
        return Ok(());
    }

    snapshot_active_credentials(config)?;

    let (creds, base_url, api_key) = {
        let profile = config.find(name).context("Profile not found")?;
        (profile.credentials.clone(), profile.base_url.clone(), profile.api_key.clone())
    };

    write_claude_credentials(creds.as_ref())?;
    apply_endpoint_to_claude_settings(base_url.as_deref(), api_key.as_deref())?;
    config.state.active_profile = Some(name.to_string());
    save_app_state(&config.state)
}

fn do_edit(config: &mut AppConfig, name: &str) -> Result<()> {
    let (current_url, current_key) = {
        let profile = config.find(name).context("Profile not found")?;
        (profile.base_url.clone(), profile.api_key.clone())
    };

    let base_url = prompt_optional("Base URL:", current_url.as_deref())?;
    let api_key = prompt_optional("API key:", current_key.as_deref())?;

    let profile = config.find_mut(name).context("Profile not found")?;
    profile.base_url = base_url.clone();
    profile.api_key = api_key.clone();
    save_profile(profile)?;

    if config.is_active(name) {
        apply_endpoint_to_claude_settings(base_url.as_deref(), api_key.as_deref())?;
    }
    Ok(())
}

/// Returns true if the profile was renamed (submenu should exit to refresh list).
fn do_rename(config: &mut AppConfig, old: &str) -> Result<bool> {
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
    if config.is_active(old) {
        config.state.active_profile = Some(new);
    }

    save_app_state(&config.state)?;
    Ok(true)
}

/// Returns true if the profile was deleted (submenu should exit).
fn do_delete(config: &mut AppConfig, name: &str) -> Result<bool> {
    let confirmed = match Confirm::new(&format!("Delete '{name}'?"))
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
            .with_context(|| format!("Failed to delete profile directory for '{name}'"))?;
    }

    config.remove(name);
    save_app_state(&config.state)?;
    Ok(true)
}

fn action_new_blank(config: &mut AppConfig) -> Result<()> {
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

fn action_capture_current(config: &mut AppConfig) -> Result<()> {
    let credentials = read_claude_credentials()?;
    let (base_url, api_key) = read_claude_endpoint_config()?;
    let name = prompt_profile_name(&config.names(), None)?;

    let mut profile = Profile::new(name.clone(), base_url, api_key);
    profile.credentials = credentials;
    save_profile(&profile)?;
    config.add(profile);

    if config.state.active_profile.is_none() {
        config.state.active_profile = Some(name);
    }
    save_app_state(&config.state)
}

// ── Profile submenu ───────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum SubmenuAction { Switch, Edit, Rename, Delete, Back }

impl SubmenuAction {
    fn label(self, is_active: bool) -> String {
        match self {
            Self::Switch if is_active =>
                format!("Switch to this profile{C_NOBOLD}  {C_FAINT}(already active){C_RESET}"),
            Self::Switch => "Switch to this profile".to_string(),
            Self::Edit   => format!("Edit{C_NOBOLD}  {C_DIM}(URL / API key){C_RESET}"),
            Self::Rename => "Rename".to_string(),
            Self::Delete => format!("{C_DANGER}Delete{C_RESET}"),
            Self::Back   => format!("{C_FAINT}← Back{C_RESET}"),
        }
    }
}

fn profile_submenu(config: &mut AppConfig, profile_name: &str) -> Result<()> {
    use SubmenuAction::*;
    const ACTIONS: [SubmenuAction; 5] = [Switch, Edit, Rename, Delete, Back];

    loop {
        let (title, is_active) = match config.find(profile_name) {
            Some(p) => (format_submenu_title(p), config.is_active(profile_name)),
            None => return Ok(()),
        };

        let labels: Vec<String> = ACTIONS.iter().map(|a| a.label(is_active)).collect();

        let idx = match Select::new(&title, labels).without_filtering().raw_prompt() {
            Ok(opt) => opt.index,
            Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => return Ok(()),
            Err(e) => return Err(e.into()),
        };

        let result: Result<bool> = match ACTIONS[idx] {
            Switch => do_switch(config, profile_name).map(|_| true),
            Edit   => do_edit(config, profile_name).map(|_| false),
            Rename => do_rename(config, profile_name),
            Delete => do_delete(config, profile_name),
            Back   => return Ok(()),
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

#[derive(Clone, Copy)]
enum MainAction {
    Profile(usize),
    NewBlank,
    Capture,
    Quit,
}

fn build_main_menu(config: &AppConfig) -> (Vec<String>, Vec<MainAction>) {
    let name_width = config.profiles.iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(0)
        .max(4);

    let mut labels = Vec::with_capacity(config.profiles.len() + 3);
    let mut actions = Vec::with_capacity(config.profiles.len() + 3);

    for (i, p) in config.profiles.iter().enumerate() {
        labels.push(format_profile_entry(p, config.is_active(&p.name), name_width));
        actions.push(MainAction::Profile(i));
    }
    labels.push(format!("{C_ORANGE}+{C_FG_OFF} New profile"));
    actions.push(MainAction::NewBlank);
    labels.push(format!("{C_ORANGE}+{C_FG_OFF} New from current profile"));
    actions.push(MainAction::Capture);
    labels.push(format!("{C_FAINT}Quit{C_RESET}"));
    actions.push(MainAction::Quit);

    (labels, actions)
}

fn build_render_config() -> RenderConfig<'static> {
    let orange = Color::Rgb { r: 217, g: 119, b: 87 };
    let blue   = Color::Rgb { r: 67,  g: 171, b: 229 };
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
        let (labels, actions) = build_main_menu(&config);

        let idx = match Select::new("clauth", labels).without_filtering().raw_prompt() {
            Ok(opt) => opt.index,
            Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => break,
            Err(e) => return Err(e.into()),
        };

        let result = match actions[idx] {
            MainAction::Quit => break,
            MainAction::NewBlank => action_new_blank(&mut config),
            MainAction::Capture => action_capture_current(&mut config),
            MainAction::Profile(i) => {
                let name = config.profiles[i].name.clone();
                profile_submenu(&mut config, &name)
            }
        };

        if let Err(e) = result
            && !is_cancelled(&e)
        {
            return Err(e);
        }
    }

    Ok(())
}
