use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Choice for auto-resolving credential divergence without the modal prompt.
/// Persisted in `AppState.default_divergence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DivergenceChoice {
    Overwrite,
    NewProfile,
    Discard,
}

use crate::lock::with_state_lock;
use crate::providers::{Provider, ThirdPartyStats};
use crate::usage::{FetchStatus, UsageInfo};

/// Newtype over `String` (transparent on disk). Makes every name-list mutation
/// compiler-checked — a rename that misses a `Vec` or the active marker is a
/// type error, not silent data drift. Derefs to `str` for existing lookups.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ProfileName(String);

impl ProfileName {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for ProfileName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for ProfileName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ProfileName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProfileName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ProfileName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ProfileName {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl PartialEq<str> for ProfileName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ProfileName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<ProfileName> for str {
    fn eq(&self, other: &ProfileName) -> bool {
        self == other.0
    }
}

impl PartialEq<String> for ProfileName {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeCredentials {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) claude_ai_oauth: Option<OAuthToken>,
}

impl ClaudeCredentials {
    pub(crate) fn refresh_token(&self) -> Option<&str> {
        self.claude_ai_oauth.as_ref()?.refresh_token.as_deref()
    }

    pub(crate) fn access_token(&self) -> Option<&str> {
        Some(self.claude_ai_oauth.as_ref()?.access_token.as_str())
    }

    /// Epoch-ms the access token expires at, when known. Gates the auto-start
    /// kick's rotate-on-429: only a clock-expired token is worth rotating.
    pub(crate) fn access_token_expires_at(&self) -> Option<i64> {
        self.claude_ai_oauth.as_ref()?.expires_at
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OAuthToken {
    pub(crate) access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) scopes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) subscription_type: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Profile {
    pub(crate) name: ProfileName,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    /// Fires a 1-token Haiku ping each 30s tick while no 5h window is active.
    pub(crate) auto_start: bool,
    /// Extra env vars merged into `settings.json`'s `env` block while active; cleared on switch.
    pub(crate) env: BTreeMap<String, String>,
    /// Utilization % to auto-switch off at (fallback chain only). None = use default.
    pub(crate) fallback_threshold: Option<f64>,
    /// Utilization % at/above which a bell toast fires in the overview tab.
    /// None = no bell for this profile.
    pub(crate) bell_threshold: Option<f64>,
    pub(crate) credentials: Option<ClaudeCredentials>,
    pub(crate) usage: Option<UsageInfo>,
    pub(crate) fetch_status: Option<FetchStatus>,
    /// Recognised third-party provider (derived from base_url).
    pub(crate) provider: Option<Provider>,
    /// Provider-specific usage data (e.g. DeepSeek balance).
    pub(crate) third_party_usage: Option<ThirdPartyStats>,
}

impl Profile {
    pub(crate) fn new(name: String, base_url: Option<String>, api_key: Option<String>) -> Self {
        let provider = base_url.as_deref().and_then(Provider::from_base_url);
        Self {
            name: name.into(),
            base_url,
            api_key,
            auto_start: false,
            env: BTreeMap::new(),
            fallback_threshold: None,
            bell_threshold: None,
            credentials: None,
            usage: None,
            fetch_status: None,
            provider,
            third_party_usage: None,
        }
    }

    pub(crate) fn is_oauth(&self) -> bool {
        self.base_url.is_none()
    }

    pub(crate) fn is_third_party(&self) -> bool {
        self.provider.is_some()
    }

    pub(crate) fn refresh_token(&self) -> Option<&str> {
        self.credentials.as_ref()?.refresh_token()
    }

    pub(crate) fn access_token(&self) -> Option<&str> {
        self.credentials.as_ref()?.access_token()
    }

    pub(crate) fn access_token_expires_at(&self) -> Option<i64> {
        self.credentials.as_ref()?.access_token_expires_at()
    }
}

/// Theme tier stored in `profiles.toml`. Serialized as a lowercase string so
/// the file stays human-readable: `theme = "full"` / `theme = "compatible"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ThemeName {
    Full,
    Compatible,
}

/// Stored at ~/.clauth/profiles.toml — ordering and active marker only.
/// Credentials and endpoint config live in per-profile subdirectories.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AppState {
    pub(crate) active_profile: Option<ProfileName>,
    pub(crate) profiles: Vec<ProfileName>,
    #[serde(default)]
    pub(crate) fallback_chain: Vec<ProfileName>,
    /// When true and the whole chain is exhausted, auto-switch clears live
    /// credentials and unsets the active profile instead of staying put.
    #[serde(default)]
    pub(crate) wrap_off: bool,
    /// Config-file theme override. CLI `--theme` flag takes priority; auto-
    /// detect applies when this is `None` and no flag was passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) theme: Option<ThemeName>,
    /// When false, burn-rate estimates ("34.4 %/h · 1h 56m left") are hidden
    /// in the Usage tab even when data is available.
    #[serde(default = "default_show_estimates", skip_serializing_if = "is_true")]
    pub(crate) show_estimates: bool,
    /// When true, the Usage tab overlays an ideal-pace `│` marker on each window
    /// bar (off by default). Toggled from the Usage action menu.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) show_pace: bool,
    #[serde(default = "default_refresh_interval")]
    pub(crate) refresh_interval_ms: u64,
    /// Default action when credential divergence is detected. `None` = show the
    /// Divergence modal (current behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_divergence: Option<DivergenceChoice>,
}

fn default_show_estimates() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Default interval new profiles.toml uses. Kept in one place — everything else
/// references this constant.
pub(crate) const DEFAULT_REFRESH_INTERVAL_MS: u64 = 90_000;

/// Minimum allowed `refresh_interval_ms`. The Anthropic API is rate-limited;
/// sub-10 s intervals serve no purpose and can trigger 429s.
pub(crate) const MIN_REFRESH_INTERVAL_MS: u64 = 10_000;

/// Maximum settable `refresh_interval_ms` (1 h). Past this the background usage
/// view is effectively stale; the Config-tab custom-value editor caps here.
pub(crate) const MAX_REFRESH_INTERVAL_MS: u64 = 3_600_000;

fn default_refresh_interval() -> u64 {
    DEFAULT_REFRESH_INTERVAL_MS
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            active_profile: None,
            profiles: Vec::new(),
            fallback_chain: Vec::new(),
            wrap_off: false,
            theme: None,
            show_estimates: true,
            show_pace: false,
            refresh_interval_ms: default_refresh_interval(),
            default_divergence: None,
        }
    }
}

pub(crate) struct AppConfig {
    pub(crate) state: AppState,
    pub(crate) profiles: Vec<Profile>,
}

/// Ranked in lock order: inner of `usage_store`, outer of the state flock.
pub(crate) type ConfigHandle =
    std::sync::Arc<crate::lockorder::RankedMutex<AppConfig, crate::lockorder::rank::Config>>;

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

    /// Case-insensitive name lookup; returns the canonical-cased name on match.
    pub(crate) fn canonical_name(&self, query: &str) -> Option<String> {
        self.names()
            .into_iter()
            .find(|n| n.eq_ignore_ascii_case(query))
            .map(str::to_string)
    }

    pub(crate) fn add(&mut self, profile: Profile) {
        self.state.profiles.push(profile.name.clone());
        self.profiles.push(profile);
    }

    pub(crate) fn remove(&mut self, name: &str) {
        self.profiles.retain(|p| p.name != name);
        self.state.profiles.retain(|n| n.as_str() != name);
        self.state.fallback_chain.retain(|n| n.as_str() != name);
        if self.is_active(name) {
            self.state.active_profile = None;
        }
    }

    /// Resync `state.profiles` from in-memory list to fix length drift from partial saves.
    pub(crate) fn sync_state_profiles(&mut self) {
        self.state.profiles = self.profiles.iter().map(|p| p.name.clone()).collect();
    }

    /// Replace `old` with `new` in every name list and the active marker.
    pub(crate) fn rename_all_occurrences(&mut self, old: &str, new: &str) {
        if let Some(profile) = self.find_mut(old) {
            profile.name = new.into();
        }
        if let Some(slot) = self.state.profiles.iter_mut().find(|n| n.as_str() == old) {
            *slot = new.into();
        }
        if let Some(slot) = self
            .state
            .fallback_chain
            .iter_mut()
            .find(|n| n.as_str() == old)
        {
            *slot = new.into();
        }
        if self.is_active(old) {
            self.state.active_profile = Some(new.into());
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Default, PartialEq)]
struct ProfileConfig {
    base_url: Option<String>,
    api_key: Option<String>,
    #[serde(default, alias = "kick_timer")]
    auto_start: bool,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    fallback_threshold: Option<f64>,
    #[serde(default)]
    bell_threshold: Option<f64>,
}

/// Test-only home-dir override. Redirects all reads/writes away from real `~/.clauth`.
/// Never compiled into the binary.
#[cfg(test)]
static HOME_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Every test redirecting `home_dir()` (via `HOME_OVERRIDE` or `$HOME`) must hold
/// this lock for its whole duration — both paths feed the same resolution.
#[cfg(test)]
pub(crate) static HOME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn set_home_override(path: PathBuf) {
    if let Ok(mut guard) = HOME_OVERRIDE.lock() {
        *guard = Some(path);
    }
}

/// Clear the test home override. Must be called on teardown to avoid shadowing `$HOME`.
#[cfg(test)]
pub(crate) fn clear_home_override() {
    if let Ok(mut guard) = HOME_OVERRIDE.lock() {
        *guard = None;
    }
}

pub(crate) fn home_dir() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(path) = HOME_OVERRIDE.lock().ok().and_then(|g| g.clone()) {
        return Ok(path);
    }
    dirs::home_dir().context("Cannot determine home directory")
}

pub(crate) fn clauth_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".clauth"))
}

pub(crate) fn claude_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude"))
}

pub(crate) fn app_state_mtime() -> Option<SystemTime> {
    let path = app_state_path().ok()?;
    std::fs::metadata(&path).ok()?.modified().ok()
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

pub(crate) fn profile_subpath(name: &str, sub: &str) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join(sub))
}

fn profile_config_path(name: &str) -> Result<PathBuf> {
    profile_subpath(name, "config.toml")
}

fn profile_credentials_path(name: &str) -> Result<PathBuf> {
    profile_subpath(name, "credentials.json")
}

pub(crate) fn profile_history_path(name: &str) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join("usage_history.jsonl"))
}

/// One line from usage_history.jsonl.
#[derive(Deserialize)]
struct HistoryLine {
    ts: u64,
    #[serde(rename = "name")]
    _name: String,
    usage: UsageInfo,
}

/// Prune a profile's usage_history.jsonl to keep at most 2 days of entries.
/// Called at startup only — rewrites the file in-place when there's anything
/// to remove. No-op when the file is missing, unparseable, or already within
/// the retention window.
pub(crate) fn prune_usage_history(name: &str) {
    let Ok(path) = profile_history_path(name) else {
        return;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let cutoff = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
        - 2 * 24 * 60 * 60 * 1000;

    let mut kept: Vec<&str> = Vec::new();
    let mut pruned: usize = 0;
    for line in content.lines() {
        if let Ok(entry) = serde_json::from_str::<HistoryLine>(line) {
            if entry.ts >= cutoff {
                kept.push(line);
            } else {
                pruned += 1;
            }
        }
    }

    if pruned > 0 {
        let body = kept.join("\n");
        let body = if body.is_empty() { body } else { body + "\n" };
        if let Err(e) = atomic_write(&path, body) {
            eprintln!("clauth: failed to prune usage history for {name}: {e}");
        }
    }
}

/// Load all parsed entries from a profile's usage_history.jsonl.
/// Returns chronological (timestamp_ms, UsageInfo) pairs.
pub(crate) fn load_usage_history(name: &str) -> Vec<(u64, UsageInfo)> {
    let Ok(path) = profile_history_path(name) else {
        return vec![];
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return vec![];
    };
    let mut entries: Vec<(u64, UsageInfo)> = content
        .lines()
        .filter_map(|line| {
            let entry: HistoryLine = serde_json::from_str(line).ok()?;
            Some((entry.ts, entry.usage))
        })
        .collect();
    entries.sort_by_key(|(ts, _)| *ts);
    entries
}

fn profile_credentials_pending_path(name: &str) -> Result<PathBuf> {
    profile_subpath(name, "credentials.json.pending")
}

/// Tempfile + rename write; readers always see old or new, never partial.
pub(crate) fn atomic_write(path: &Path, content: impl AsRef<[u8]>) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = dir.join(format!(".{file_name}.tmp.{}", std::process::id()));
    std::fs::write(&tmp, content)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Like [`atomic_write`] but creates the temp file with mode 0o600 (Unix only)
/// so the file is never world-readable even for the instant before the rename.
/// On non-Unix this is identical to [`atomic_write`].
pub(crate) fn atomic_write_600(path: &Path, content: impl AsRef<[u8]>) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    if !dir.exists() {
        // A 0o600 file under a world-readable dir still leaks via the dir entry;
        // any dir this helper must create is 0o700 to keep the secret contained.
        mkdir_700(dir)?;
    }
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = dir.join(format!(".{file_name}.tmp.{}", std::process::id()));
    // Clear any stale temp so `create_new` lands on a fresh inode — guarantees
    // the 0o600 mode is applied at creation, never inherited from a looser file.
    if tmp.exists() {
        std::fs::remove_file(&tmp)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(content.as_ref())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, content)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Create `path` as a directory (recursively) with mode 0o700 on Unix,
/// or the default mode on non-Unix.
fn mkdir_700(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path)
}

pub(crate) fn read_json_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))
}

pub(crate) fn read_toml_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))
}

fn load_app_state() -> Result<AppState> {
    let path = app_state_path()?;
    if !path.exists() {
        return Ok(AppState::default());
    }
    let mut state: AppState = read_toml_file(&path)?;
    state.refresh_interval_ms = state.refresh_interval_ms.max(MIN_REFRESH_INTERVAL_MS);
    Ok(state)
}

pub(crate) fn save_app_state(state: &AppState) -> Result<()> {
    with_state_lock(|| {
        std::fs::create_dir_all(clauth_dir()?)?;
        atomic_write_600(&app_state_path()?, toml::to_string_pretty(state)?)
            .context("Failed to write profiles.toml")
    })
}

fn load_profile(name: &str) -> Result<Profile> {
    let config_path = profile_config_path(name)?;
    let raw_config = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("Failed to read {name}/config.toml")),
    };
    let config: ProfileConfig = if raw_config.trim().is_empty() {
        ProfileConfig::default()
    } else {
        toml::from_str(&raw_config)
            .with_context(|| format!("Failed to parse {name}/config.toml"))?
    };

    let cred_path = profile_credentials_path(name)?;
    let credentials = if cred_path.exists() {
        Some(read_json_file(&cred_path)?)
    } else {
        None
    };
    // Adopt a staged rotation that never committed (crash/failed write between OAuth response and save).
    let credentials = recover_pending_credentials(name, credentials);

    let provider = config.base_url.as_deref().and_then(Provider::from_base_url);
    let third_party_usage = provider
        .as_ref()
        .and_then(|_| crate::providers::load_third_party_disk_cache(name));

    let profile = Profile {
        name: name.into(),
        base_url: config.base_url,
        api_key: config.api_key,
        auto_start: config.auto_start,
        env: config.env,
        fallback_threshold: config.fallback_threshold.map(|v| v.clamp(0.0, 100.0)),
        bell_threshold: config.bell_threshold.map(|v| v.clamp(0.0, 100.0)),
        credentials,
        usage: None,
        fetch_status: None,
        provider,
        third_party_usage,
    };

    maybe_rewrite_config_toml(&config_path, &raw_config, &profile);

    Ok(profile)
}

/// Refresh config.toml when its semantic content drifts from what we'd render
/// today. Comment-only or whitespace-only differences shouldn't trigger a
/// rewrite — the TUI reloads on every state-file change and we don't want to
/// thrash disk on every reload.
fn maybe_rewrite_config_toml(config_path: &Path, raw_config: &str, profile: &Profile) {
    let rendered = render_config_toml(profile);
    let needs_rewrite = match toml::from_str::<ProfileConfig>(&rendered) {
        Ok(canonical) => {
            let on_disk = ProfileConfig {
                base_url: profile.base_url.clone(),
                api_key: profile.api_key.clone(),
                auto_start: profile.auto_start,
                env: profile.env.clone(),
                fallback_threshold: profile.fallback_threshold,
                bell_threshold: profile.bell_threshold,
            };
            canonical != on_disk
        }
        Err(_) => raw_config != rendered,
    };
    if needs_rewrite {
        let _ = with_state_lock(|| {
            let _ = atomic_write(config_path, &rendered);
            Ok(())
        });
    }
}

pub(crate) fn save_profile(profile: &Profile) -> Result<()> {
    with_state_lock(|| {
        mkdir_700(&profile_dir(&profile.name)?)?;

        // credentials.json BEFORE config.toml: single-use refresh token must
        // not be lost to a config.toml write failure.
        let cred_path = profile_credentials_path(&profile.name)?;
        match &profile.credentials {
            Some(creds) => atomic_write_600(&cred_path, serde_json::to_string_pretty(creds)?)
                .context("Failed to write credentials.json")?,
            None if cred_path.exists() => {
                std::fs::remove_file(&cred_path).context("Failed to remove credentials.json")?
            }
            None => {}
        }

        atomic_write_600(
            &profile_config_path(&profile.name)?,
            render_config_toml(profile),
        )
        .context("Failed to write config.toml")?;

        Ok(())
    })
}

/// Write rotated credentials to a sidecar BEFORE `save_profile`. Single-use
/// refresh tokens can't be lost to a crash mid-save; `load_profile` adopts
/// this sidecar on next start if the commit never landed.
pub(crate) fn stage_rotated_credentials(name: &str, creds: &ClaudeCredentials) -> Result<()> {
    with_state_lock(|| {
        mkdir_700(&profile_dir(name)?)?;
        atomic_write_600(
            &profile_credentials_pending_path(name)?,
            serde_json::to_string_pretty(creds)?,
        )
        .context("Failed to stage rotated credentials")
    })
}

pub(crate) fn clear_staged_credentials(name: &str) {
    if let Ok(path) = profile_credentials_pending_path(name) {
        let _ = std::fs::remove_file(path);
    }
}

/// Adopt the rotation sidecar when it's at least as new as `credentials.json`
/// (commit failed or process died mid-save). A stale sidecar is discarded.
fn recover_pending_credentials(
    name: &str,
    loaded: Option<ClaudeCredentials>,
) -> Option<ClaudeCredentials> {
    let Ok(pending_path) = profile_credentials_pending_path(name) else {
        return loaded;
    };
    let Ok(pending_meta) = pending_path.symlink_metadata() else {
        return loaded; // no sidecar — the common case
    };
    let recovered = (|| -> Option<ClaudeCredentials> {
        let bytes = std::fs::read(&pending_path).ok()?;
        let pending: ClaudeCredentials = serde_json::from_slice(&bytes).ok()?;
        pending.claude_ai_oauth.as_ref()?; // must carry an oauth block to matter
        let cred_path = profile_credentials_path(name).ok()?;
        // Clean success → credentials.json at least as new → discard.
        // Failed/interrupted commit → older or absent → adopt.
        let adopt = match cred_path.metadata().and_then(|m| m.modified()) {
            Ok(cred_mtime) => pending_meta
                .modified()
                .map(|p| p >= cred_mtime)
                .unwrap_or(true),
            Err(_) => true,
        };
        if !adopt {
            return None;
        }
        let _ = with_state_lock(|| atomic_write_600(&cred_path, &bytes).map_err(Into::into));
        Some(pending)
    })();
    let _ = std::fs::remove_file(&pending_path);
    recovered.or(loaded)
}

pub(crate) fn load_config() -> Result<AppConfig> {
    std::fs::create_dir_all(profiles_root()?)?;
    let state = load_app_state()?;
    let profiles = state
        .profiles
        .iter()
        .map(|n| load_profile(n))
        .collect::<Result<Vec<_>>>()?;
    Ok(AppConfig { state, profiles })
}

/// Renders config.toml with set values uncommented and unset ones as commented examples.
fn render_config_toml(profile: &Profile) -> String {
    fn toml_str(s: &str) -> String {
        toml::Value::String(s.to_string()).to_string()
    }

    let mut out = String::from("# clauth profile configuration\n\n");

    out.push_str("# Base URL for an API-endpoint profile. Leave commented for an OAuth\n");
    out.push_str("# (Pro / Max / Team / Enterprise) profile.\n");
    match profile.base_url.as_deref() {
        Some(v) => out.push_str(&format!("base_url = {}\n", toml_str(v))),
        None => out.push_str("# base_url = \"https://api.anthropic.com\"\n"),
    }
    out.push('\n');

    out.push_str("# API key for the endpoint. Only used when base_url is set.\n");
    match profile.api_key.as_deref() {
        Some(v) => out.push_str(&format!("api_key = {}\n", toml_str(v))),
        None => out.push_str("# api_key = \"sk-ant-...\"\n"),
    }
    out.push('\n');

    out.push_str("# Auto-start the 5-hour usage window for this profile. clauth fires a\n");
    out.push_str("# 1-token Haiku ping at launch and on every 30s refresh while there's\n");
    out.push_str("# no running window. ~0.001¢ per ping. OAuth profiles only.\n");
    out.push_str("# Old name `kick_timer = true` is still accepted.\n");
    if profile.auto_start {
        out.push_str("auto_start = true\n");
    } else {
        out.push_str("# auto_start = true\n");
    }
    out.push('\n');

    out.push_str("# 5-hour utilization percentage at/above which clauth will auto-switch\n");
    out.push_str("# off this profile, provided the profile is also a member of the\n");
    out.push_str("# fallback chain configured in ~/.clauth/profiles.toml. Range 0..=100.\n");
    match profile.fallback_threshold {
        Some(v) => out.push_str(&format!("fallback_threshold = {v}\n")),
        None => out.push_str("# fallback_threshold = 95.0\n"),
    }
    out.push('\n');

    out.push_str("# 5-hour utilization percentage at/above which clauth fires a bell\n");
    out.push_str("# notification in the overview tab. Range 0..=100.\n");
    match profile.bell_threshold {
        Some(v) => out.push_str(&format!("bell_threshold = {v}\n")),
        None => out.push_str("# bell_threshold = 95.0\n"),
    }
    out.push('\n');

    out.push_str("# Extra env vars merged into ~/.claude/settings.json's env block while\n");
    out.push_str("# this profile is active. Cleared on switch to another profile.\n");
    if profile.env.is_empty() {
        out.push_str("# [env]\n");
        out.push_str("# HTTP_PROXY = \"http://localhost:8080\"\n");
    } else {
        out.push_str("[env]\n");
        for (k, v) in &profile.env {
            out.push_str(&format!("{k} = {}\n", toml_str(v)));
        }
    }

    out
}

#[cfg(test)]
#[path = "../tests/inline/profile.rs"]
mod tests;
