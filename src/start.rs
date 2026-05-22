//! `clauth start <name>` — spawn a `claude` instance isolated to a per-call
//! `CLAUDE_CONFIG_DIR`. The temp dir mirrors `~/.claude` via symlinks for
//! every entry except `settings.json` (regular file with the profile's
//! merged env) and `.credentials.json` (symlink to the profile's stored
//! credentials, when present).

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::claude::{build_claude_settings_json, create_symlink};
use crate::profile::{AppConfig, atomic_write, home_dir, profile_dir};

pub(crate) fn run(config: &AppConfig, name: &str, claude_args: &[String]) -> Result<()> {
    let home = home_dir()?;
    let claude_dir = home.join(".claude");
    if !claude_dir.exists() {
        bail!("~/.claude not found; install Claude Code first");
    }

    let profile = config.find(name).context("profile not found")?;

    let tmp = tempfile::Builder::new()
        .prefix("clauth-")
        .tempdir()
        .context("failed to create temp dir")?;

    for entry in std::fs::read_dir(&claude_dir)
        .with_context(|| format!("failed to read {}", claude_dir.display()))?
    {
        let entry = entry?;
        let file_name = entry.file_name();
        if file_name == "settings.json" || file_name == ".credentials.json" {
            continue;
        }
        link_entry(&entry.path(), &tmp.path().join(&file_name))?;
    }

    let settings_src = claude_dir.join("settings.json");
    let merged = build_claude_settings_json(&settings_src, profile, &[])?;
    atomic_write(&tmp.path().join("settings.json"), merged)
        .context("failed to write tempdir settings.json")?;

    let creds = profile_dir(&profile.name)?.join("credentials.json");
    if creds.exists() {
        create_symlink(&creds, &tmp.path().join(".credentials.json"))?;
    }

    // claude reads its per-user state from `<CLAUDE_CONFIG_DIR>/.claude.json`
    // (sibling of the .claude/ tree). Share the real one so project history,
    // onboarding state, etc. stay in sync with the user's normal sessions.
    let claude_json = home.join(".claude.json");
    if claude_json.exists() {
        link_entry(&claude_json, &tmp.path().join(".claude.json"))?;
    }

    let status = Command::new("claude")
        .env("CLAUDE_CONFIG_DIR", tmp.path())
        .args(claude_args)
        .status()
        .context("failed to spawn claude")?;

    sync_relogged_credentials(&profile.name, &tmp.path().join(".credentials.json"));

    drop(tmp);

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// When CC re-logs inside the isolated session it `unlink+write`s the
/// `.credentials.json` we linked in, replacing the symlink with a fresh
/// regular file. Copy those bytes into the profile's stored creds so the
/// new identity survives the tempdir cleanup.
fn sync_relogged_credentials(name: &str, tempdir_creds: &Path) {
    let Ok(meta) = tempdir_creds.symlink_metadata() else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let Ok(bytes) = std::fs::read(tempdir_creds) else {
        return;
    };
    let Ok(target) = profile_dir(name).map(|dir| dir.join("credentials.json")) else {
        return;
    };
    if std::fs::read(&target).ok().as_deref() == Some(bytes.as_slice()) {
        return;
    }
    if atomic_write(&target, &bytes).is_err() {
        return;
    }
    eprintln!("clauth: re-login detected; updated credentials for profile '{name}'");
}

#[cfg(unix)]
fn link_entry(src: &Path, dst: &Path) -> Result<()> {
    std::os::unix::fs::symlink(src, dst)
        .with_context(|| format!("failed to symlink {} -> {}", dst.display(), src.display()))
}

#[cfg(windows)]
fn link_entry(src: &Path, dst: &Path) -> Result<()> {
    let result = if src.is_dir() {
        std::os::windows::fs::symlink_dir(src, dst)
    } else {
        std::os::windows::fs::symlink_file(src, dst)
    };
    result.with_context(|| {
        format!(
            "failed to symlink {} -> {} (enable developer mode or run as admin)",
            dst.display(),
            src.display()
        )
    })
}

#[cfg(not(any(unix, windows)))]
fn link_entry(_src: &Path, _dst: &Path) -> Result<()> {
    bail!("clauth start requires symlink support");
}
