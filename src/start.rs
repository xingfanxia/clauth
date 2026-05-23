//! `clauth start <name>` — spawn `claude` against the profile's persistent
//! runtime directory. See [`crate::runtime`] for the shared-runtime design;
//! this module is just the thin wrapper that owns the lifetime guard.

use std::process::Command;

use anyhow::{Context, Result};

use crate::profile::AppConfig;
use crate::runtime::ProfileRuntime;

pub(crate) fn run(config: &AppConfig, name: &str, claude_args: &[String]) -> Result<()> {
    let profile = config.find(name).context("profile not found")?;
    let runtime = ProfileRuntime::acquire(profile)?;

    let status = Command::new("claude")
        .env("CLAUDE_CONFIG_DIR", runtime.config_dir())
        .args(claude_args)
        .status()
        .context("failed to spawn claude")?;

    // Explicit drop so the runtime's final sync + reference-count
    // cleanup runs before the possible `process::exit` below.
    drop(runtime);

    if !status.success() {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            let code = status
                .code()
                .unwrap_or_else(|| status.signal().map(|s| 128 + s).unwrap_or(1));
            std::process::exit(code);
        }
        #[cfg(not(unix))]
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
