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
    let path = resolve_credentials_path()?;
    let creds = read_credentials(&path);
    let config = load_config()?;
    let matched = creds
        .as_ref()
        .and_then(ClaudeCredentials::refresh_token)
        .and_then(|rt| match_by_refresh_token(&config, rt));

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
