//! `clauth which [--json]` — identify which stored profile owns the OAuth
//! tokens in the credentials.json currently loaded by Claude Code.
//!
//! Resolution: matches the loaded file's `refreshToken` against each stored
//! profile's `refreshToken`. The clauth symlink layout means the live file
//! and the matching profile's file are usually the same bytes, so equality
//! holds across rotations done by either clauth or Claude Code itself.
//!
//! Path: honors `CLAUDE_CONFIG_DIR` (the same env var `clauth start` sets) so
//! a status line running inside an isolated session finds the right file.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::format::endpoint_label;
use crate::profile::{AppConfig, ClaudeCredentials, Profile, home_dir, load_config};

pub(crate) fn run(json: bool) -> Result<()> {
    let in_session = std::env::var_os("CLAUDE_CONFIG_DIR").is_some();
    let path = resolve_credentials_path()?;
    let creds = read_credentials(&path);
    let config = load_config()?;
    let matched = creds
        .as_ref()
        .and_then(|c| resolve_profile(&config, c, in_session));

    if json {
        emit_json(&config, matched);
    } else {
        emit_plain(matched);
    }
    Ok(())
}

fn resolve_credentials_path() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join(".credentials.json"));
    }
    Ok(home_dir()?.join(".claude").join(".credentials.json"))
}

fn read_credentials(path: &Path) -> Option<ClaudeCredentials> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Resolve the loaded credentials to a stored profile. Prefers an exact
/// refresh-token match; falls back to the active profile when that profile is
/// credential-less and the loaded file is a real OAuth login.
///
/// `in_session` must be true when `CLAUDE_CONFIG_DIR` is set — i.e. the
/// caller is running inside a `clauth start` runtime. In that case the
/// credential-less fallback is suppressed: the runtime creds belong to the
/// started profile, not necessarily the global active profile.
fn resolve_profile<'a>(
    config: &'a AppConfig,
    creds: &ClaudeCredentials,
    in_session: bool,
) -> Option<&'a str> {
    creds
        .refresh_token()
        .and_then(|rt| match_by_refresh_token(config, rt))
        .or_else(|| {
            if in_session {
                None
            } else {
                match_credential_less_active(config, creds)
            }
        })
}

/// Read-time counterpart to first-login adoption: a freshly-activated blank
/// profile that Claude Code just logged into holds no stored token yet, so no
/// `refreshToken` match exists — but the live login is unambiguously the
/// active profile's. Only fires when the active profile owns no credentials
/// and the loaded file carries a completed OAuth login.
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

fn emit_json(config: &AppConfig, matched: Option<&str>) {
    let profile = matched.and_then(|n| config.find(n));
    let value = serde_json::json!({
        "profile": profile.map(|p| &p.name),
        "tier": profile.map(endpoint_label),
        "oauth": profile.map(Profile::is_oauth),
        "active": profile.is_some_and(|p| config.is_active(&p.name)),
    });
    println!("{value}");
}

#[cfg(test)]
#[path = "../tests/inline/which.rs"]
mod tests;
