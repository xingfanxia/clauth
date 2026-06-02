use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::lock::with_state_lock;
use crate::usage::{FetchStatus, UsageInfo};

// ── Types ─────────────────────────────────────────────────────────────────────

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
    pub(crate) name: String,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    /// When true, clauth fires a 1-token Haiku ping at startup and on every
    /// 30s refresh tick if this profile has no active 5h window. Opt-in
    /// because every successful ping starts a fresh 5h usage window.
    pub(crate) auto_start: bool,
    /// Extra env vars merged into `~/.claude/settings.json`'s `env` block
    /// while this profile is active. Cleared on switch to another profile.
    pub(crate) env: BTreeMap<String, String>,
    /// 5-hour utilization percentage at/above which the auto-switch system
    /// will move off this profile. Only takes effect while the profile is a
    /// member of `AppState::fallback_chain`. None = use default.
    pub(crate) fallback_threshold: Option<f64>,
    pub(crate) credentials: Option<ClaudeCredentials>,
    pub(crate) usage: Option<UsageInfo>,
    pub(crate) fetch_status: Option<FetchStatus>,
}

impl Profile {
    pub(crate) fn new(name: String, base_url: Option<String>, api_key: Option<String>) -> Self {
        Self {
            name,
            base_url,
            api_key,
            auto_start: false,
            env: BTreeMap::new(),
            fallback_threshold: None,
            credentials: None,
            usage: None,
            fetch_status: None,
        }
    }

    pub(crate) fn is_oauth(&self) -> bool {
        self.base_url.is_none()
    }

    pub(crate) fn refresh_token(&self) -> Option<&str> {
        self.credentials.as_ref()?.refresh_token()
    }

    pub(crate) fn access_token(&self) -> Option<&str> {
        self.credentials.as_ref()?.access_token()
    }
}

/// Stored at ~/.clauth/profiles.toml — ordering and active marker only.
/// Credentials and endpoint config live in per-profile subdirectories.
#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct AppState {
    pub(crate) active_profile: Option<String>,
    pub(crate) profiles: Vec<String>,
    /// Epoch-ms of the last successful auto-start ping per profile. Used to
    /// skip re-pinging a profile whose previous ping should still be inside
    /// its 5-hour window. Field stays `last_kick_at` on disk for back-compat
    /// with older clauth versions; new state can be read by them and vice
    /// versa.
    #[serde(default, alias = "last_kick_at", rename = "last_kick_at")]
    pub(crate) last_auto_start_at: HashMap<String, u64>,
    /// Ordered list of profile names participating in the auto-switch chain.
    #[serde(default)]
    pub(crate) fallback_chain: Vec<String>,
    /// Wrap-off mode. When true and every chain member's fallback threshold is
    /// below 100% with the whole chain exhausted, auto-switch turns OFF all
    /// accounts (clears the live credentials, unsets the active profile)
    /// instead of staying on the spent profile — a hard stop on further token
    /// spend once the chain is dry. Defaults to false (the legacy "stay put"
    /// behaviour).
    #[serde(default)]
    pub(crate) wrap_off: bool,
    /// Per-profile learned refresh interval in ms. Updated by the AIMD cadence
    /// learner in response to 429s and consecutive-ok counts. Advisory — a
    /// missing entry means the profile uses `NORMAL_INTERVAL_MS`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) learned_intervals_ms: HashMap<String, u64>,
    /// How many consecutive non-429 fetches each profile has accumulated since
    /// the last backoff. Resets to 0 on every bump-up or bump-down.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) consecutive_ok_count: HashMap<String, u32>,
    /// How many consecutive Fresh+unchanged-util fetches each profile has
    /// seen. Only restored when `learned < SERVER_CACHE_TTL_ESTIMATE_MS` —
    /// above TTL the counter is irrelevant and is dropped on load.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) consecutive_cache_hit_count: HashMap<String, u32>,
    /// Epoch-ms of the most recent 429 seen for each profile. Used by the
    /// quiet-period reset: if now - last_429_at >= LEARNED_QUIET_RESET_MS and
    /// the learned interval is above NORMAL, it snaps back to NORMAL.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) last_429_at: HashMap<String, u64>,
}

pub(crate) struct AppConfig {
    pub(crate) state: AppState,
    pub(crate) profiles: Vec<Profile>,
}

/// Shared handle to the process-wide [`AppConfig`], ranked in the global lock
/// order (`config` is inner of `usage_store`, outer of the state flock).
pub(crate) type ConfigHandle =
    std::sync::Arc<crate::lockorder::RankedMutex<AppConfig, { crate::lockorder::rank::CONFIG }>>;

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
        self.state.profiles.retain(|n| n != name);
        self.state.fallback_chain.retain(|n| n != name);
        if self.is_active(name) {
            self.state.active_profile = None;
        }
    }
}

/// On-disk format for ~/.clauth/profiles/<name>/config.toml
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
}

// ── Path helpers ──────────────────────────────────────────────────────────────

/// Test-only home-dir override. The showcase fixture points this at a sandbox
/// tempdir so it can run the real, fully-interactive TUI — switching, editing,
/// toggling, deleting — with every read/write redirected away from the user's
/// real `~/.clauth` and `~/.claude`. Never compiled into the binary.
#[cfg(test)]
static HOME_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_home_override(path: PathBuf) {
    if let Ok(mut guard) = HOME_OVERRIDE.lock() {
        *guard = Some(path);
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

fn profile_config_path(name: &str) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join("config.toml"))
}

fn profile_credentials_path(name: &str) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join("credentials.json"))
}

fn profile_credentials_pending_path(name: &str) -> Result<PathBuf> {
    Ok(profile_dir(name)?.join("credentials.json.pending"))
}

/// Write `content` to `path` via tempfile + rename so concurrent readers see
/// either the old file or the new one, never a partial write.
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
    with_state_lock(|| {
        std::fs::create_dir_all(clauth_dir()?)?;
        atomic_write(&app_state_path()?, toml::to_string_pretty(state)?)
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
        let content = std::fs::read_to_string(&cred_path)
            .with_context(|| format!("Failed to read {name}/credentials.json"))?;
        Some(
            serde_json::from_str(&content)
                .with_context(|| format!("Failed to parse {name}/credentials.json"))?,
        )
    } else {
        None
    };
    // Adopt a rotation that was staged but never committed (failed save or a
    // crash between the OAuth response and the structured write).
    let credentials = recover_pending_credentials(name, credentials);

    let profile = Profile {
        name: name.to_string(),
        base_url: config.base_url,
        api_key: config.api_key,
        auto_start: config.auto_start,
        env: config.env,
        fallback_threshold: config.fallback_threshold,
        credentials,
        usage: None,
        fetch_status: None,
    };

    // Refresh config.toml when its semantic content drifts from what we'd
    // render today. Comment-only or whitespace-only differences shouldn't
    // trigger a rewrite — the TUI reloads on every state-file change and we
    // don't want to thrash disk on every reload.
    let rendered = render_config_toml(&profile);
    let needs_rewrite = match toml::from_str::<ProfileConfig>(&rendered) {
        Ok(canonical) => {
            let on_disk = ProfileConfig {
                base_url: profile.base_url.clone(),
                api_key: profile.api_key.clone(),
                auto_start: profile.auto_start,
                env: profile.env.clone(),
                fallback_threshold: profile.fallback_threshold,
            };
            canonical != on_disk
        }
        Err(_) => raw_config != rendered,
    };
    if needs_rewrite {
        let _ = with_state_lock(|| {
            let _ = atomic_write(&config_path, &rendered);
            Ok(())
        });
    }

    Ok(profile)
}

pub(crate) fn save_profile(profile: &Profile) -> Result<()> {
    with_state_lock(|| {
        std::fs::create_dir_all(profile_dir(&profile.name)?)?;

        // Persist credentials.json BEFORE config.toml: the OAuth token chain
        // lives here, and a single-use refresh token must not be lost to an
        // unrelated config.toml write failure. A failure here aborts before
        // config.toml is touched.
        let cred_path = profile_credentials_path(&profile.name)?;
        match &profile.credentials {
            Some(creds) => atomic_write(&cred_path, serde_json::to_string_pretty(creds)?)
                .context("Failed to write credentials.json")?,
            None if cred_path.exists() => {
                std::fs::remove_file(&cred_path).context("Failed to remove credentials.json")?
            }
            None => {}
        }

        atomic_write(
            &profile_config_path(&profile.name)?,
            render_config_toml(profile),
        )
        .context("Failed to write config.toml")?;

        Ok(())
    })
}

/// Durably stage a freshly-rotated credential blob to a sidecar BEFORE the
/// structured `save_profile`. The OAuth refresh token is single-use: once the
/// server returns a rotated pair the previous token is dead, so losing the new
/// pair (a failed write, a crash mid-save) permanently breaks the chain. If the
/// commit never lands, `load_profile` adopts this sidecar on the next start.
/// Callers clear it with [`clear_staged_credentials`] after a successful commit.
pub(crate) fn stage_rotated_credentials(name: &str, creds: &ClaudeCredentials) -> Result<()> {
    with_state_lock(|| {
        std::fs::create_dir_all(profile_dir(name)?)?;
        atomic_write(
            &profile_credentials_pending_path(name)?,
            serde_json::to_string_pretty(creds)?,
        )
        .context("Failed to stage rotated credentials")
    })
}

/// Remove the rotation sidecar after the structured save committed successfully.
pub(crate) fn clear_staged_credentials(name: &str) {
    if let Ok(path) = profile_credentials_pending_path(name) {
        let _ = std::fs::remove_file(path);
    }
}

/// Adopt a staged rotation that never committed. The sidecar is written before
/// the structured save during token rotation; if that save failed or the
/// process died between the OAuth response and the commit, `credentials.json`
/// may still hold the now-dead pre-rotation token while the sidecar holds the
/// live rotated one. Adopt the sidecar when it is at least as new as
/// `credentials.json` (or the latter is missing), persist it, and clear it. A
/// stale sidecar (commit succeeded but cleanup didn't) is simply discarded.
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
        // The sidecar is written before the commit, so a clean success leaves
        // credentials.json at least as new (discard); a failed or interrupted
        // commit leaves it older or absent (adopt).
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
        let _ = with_state_lock(|| atomic_write(&cred_path, &bytes).map_err(Into::into));
        Some(pending)
    })();
    // Whether adopted or discarded, the sidecar has served its purpose.
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

/// Hand-rolled TOML writer that keeps every option visible — set values are
/// uncommented, unset ones stay as commented examples so users can discover
/// what's available without consulting the README.
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
