//! `clauth which [--json]` — identify which stored profile owns the OAuth
//! tokens in the credentials.json currently loaded by Claude Code.
//!
//! Resolution: (1) match the loaded file's `refreshToken` against each stored
//! profile's `refreshToken` — the clauth symlink layout keeps the live file
//! and the matching profile's file byte-identical across rotations. (2) Inside
//! a `clauth start` runtime, fall back to the profile named by
//! `CLAUDE_CONFIG_DIR` (`profiles/<name>/runtime`): the runtime tree is
//! per-profile, so that profile owns the session even before its first login
//! is stored. (3) Otherwise, attribute a fresh login to the credential-less
//! active profile.
//!
//! Path: honors `CLAUDE_CONFIG_DIR` (the same env var `clauth start` sets) so
//! a status line running inside an isolated session finds the right file.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::format::endpoint_label;
use crate::profile::{AppConfig, ClaudeCredentials, Profile, claude_dir, load_config};

/// Which resolution branch attributed the loaded credentials to a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// Exact `refreshToken` match against a stored profile.
    RefreshMatch,
    /// Profile named by a `clauth start` runtime `CLAUDE_CONFIG_DIR`.
    SessionDir,
    /// Fresh first-login attributed to the credential-less active profile.
    CredentialLessActive,
}

impl Source {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Source::RefreshMatch => "refresh_match",
            Source::SessionDir => "session_dir",
            Source::CredentialLessActive => "credential_less_active",
        }
    }
}

pub(crate) fn run(json: bool) -> Result<()> {
    let config = load_config()?;
    let resolved = resolve_active(&config);

    if json {
        emit_json(&config, resolved);
    } else {
        emit_plain(resolved.as_ref().map(|(name, _)| name.as_str()));
    }
    Ok(())
}

/// Gather the session env + loaded credentials and resolve them to the owning
/// profile, returning an owned name plus the branch that matched, or `None` when
/// nothing matched. Shared by `clauth which` and the MCP `which` tool.
pub(crate) fn resolve_active(config: &AppConfig) -> Option<(String, Source)> {
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    let session_profile = config_dir
        .as_deref()
        .and_then(session_profile_from_config_dir);
    let creds = credentials_path(config_dir.as_deref())
        .ok()
        .and_then(|path| read_credentials(&path));
    resolve_profile(
        config,
        creds.as_ref(),
        config_dir.is_some(),
        session_profile.as_deref(),
    )
    .map(|(name, source)| (name.to_string(), source))
}

/// How this session reads its credentials, used to explain what `switch` does to
/// *it*. The session's config dir is the discriminator: a `clauth start` runtime
/// and a custom `CLAUDE_CONFIG_DIR` each read their own `.credentials.json`, which
/// a global relink never touches; only a session on the global `~/.claude/` reads
/// the very file `switch` repoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionAuth {
    /// `clauth start <name>` runtime — pinned to its own creds; a global switch can't reach it.
    IsolatedRuntime(String),
    /// A non-clauth `CLAUDE_CONFIG_DIR` — reads its own creds; a global switch can't reach it.
    IsolatedCustom,
    /// No `CLAUDE_CONFIG_DIR` — reads the global `~/.claude/` creds that `switch` repoints.
    Global,
}

/// Classify the current session's credential source from `CLAUDE_CONFIG_DIR` (the
/// same env `clauth start` sets). An empty value is treated as unset.
pub(crate) fn session_auth() -> SessionAuth {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        Some(dir) => match session_profile_from_config_dir(Path::new(&dir)) {
            Some(name) => SessionAuth::IsolatedRuntime(name),
            None => SessionAuth::IsolatedCustom,
        },
        None => SessionAuth::Global,
    }
}

fn credentials_path(config_dir: Option<&Path>) -> Result<PathBuf> {
    match config_dir {
        Some(dir) => Ok(dir.join(".credentials.json")),
        None => Ok(claude_dir()?.join(".credentials.json")),
    }
}

/// Extract the `<name>` from a `clauth start` runtime path
/// (`~/.clauth/profiles/<name>/runtime`). Returns `None` for any other shape.
fn session_profile_from_config_dir(dir: &Path) -> Option<String> {
    if dir.file_name()? != "runtime" {
        return None;
    }
    let profile_dir = dir.parent()?;
    if profile_dir.parent()?.file_name()? != "profiles" {
        return None;
    }
    Some(profile_dir.file_name()?.to_str()?.to_string())
}

fn read_credentials(path: &Path) -> Option<ClaudeCredentials> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Resolve loaded credentials to a stored profile.
///
/// Order: (1) exact refresh-token match; (2) inside a `clauth start` runtime,
/// the profile named by `CLAUDE_CONFIG_DIR` owns the session even before its
/// first login is stored; (3) for a non-runtime caller, the credential-less
/// active profile a fresh login was just written into.
///
/// A `CLAUDE_CONFIG_DIR` that isn't a clauth runtime gets step 1 only — its
/// credentials don't belong to the global active profile.
fn resolve_profile<'a>(
    config: &'a AppConfig,
    creds: Option<&ClaudeCredentials>,
    in_session: bool,
    session_profile: Option<&str>,
) -> Option<(&'a str, Source)> {
    if let Some(name) = creds
        .and_then(ClaudeCredentials::refresh_token)
        .and_then(|rt| match_by_refresh_token(config, rt))
    {
        return Some((name, Source::RefreshMatch));
    }
    if let Some(profile) = session_profile.and_then(|n| config.find(n)) {
        return Some((profile.name.as_str(), Source::SessionDir));
    }
    if in_session {
        return None;
    }
    creds
        .and_then(|c| match_credential_less_active(config, c))
        .map(|name| (name, Source::CredentialLessActive))
}

/// Matches a fresh first-login to the credential-less active profile.
/// Fires only when the active profile has no stored token but the loaded file
/// has a completed OAuth login — no `refreshToken` match is possible yet.
fn match_credential_less_active<'a>(
    config: &'a AppConfig,
    creds: &ClaudeCredentials,
) -> Option<&'a str> {
    creds.refresh_token()?;
    let active = config.state.active_profile.as_deref()?;
    config
        .find(active)
        .filter(|p| p.credentials.is_none())
        .map(|p| p.name.as_str())
}

fn match_by_refresh_token<'a>(config: &'a AppConfig, refresh_token: &str) -> Option<&'a str> {
    let active = config.state.active_profile.as_deref();
    let mut fallback = None;
    for p in &config.profiles {
        if p.refresh_token() != Some(refresh_token) {
            continue;
        }
        if Some(p.name.as_str()) == active {
            return Some(p.name.as_str());
        }
        fallback.get_or_insert(p.name.as_str());
    }
    fallback
}

fn emit_plain(matched: Option<&str>) {
    match matched {
        Some(name) => println!("{name}"),
        None => println!("unknown"),
    }
}

fn emit_json(config: &AppConfig, resolved: Option<(String, Source)>) {
    let profile = resolved.as_ref().and_then(|(name, _)| config.find(name));
    let value = serde_json::json!({
        "profile": profile.map(|p| &p.name),
        "source": resolved.as_ref().map(|(_, s)| s.as_str()),
        "tier": profile.map(endpoint_label),
        "oauth": profile.map(Profile::is_oauth),
        "active": profile.is_some_and(|p| config.is_active(&p.name)),
    });
    println!("{value}");
}

#[cfg(test)]
#[path = "../tests/inline/which.rs"]
mod tests;
