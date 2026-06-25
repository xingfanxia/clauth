//! Local-read probes backing the Plugin tab: binary-on-`PATH` resolution, Claude
//! Code's plugin registry (`installed_plugins.json` / `known_marketplaces.json`),
//! the manual `mcpServers` wiring, the `claude --version` string, and the one
//! safe write the tab performs (wire `mcpServers.clauth`).
//!
//! Everything here is a cheap filesystem/`PATH` read except [`cc_version`], which
//! runs one short subprocess; the Plugin tab caches that result. Nothing spawns a
//! background thread. All path reads route through the test-overridable
//! `home_dir()` / `claude_dir()`, so the inline tests can sandbox `$HOME`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Result;
use serde_json::{Map, Value};

use crate::profile::{atomic_write, claude_dir, home_dir};

/// Plugin id in the registry (`<plugin>@<marketplace>`).
pub(crate) const PLUGIN_ID: &str = "clauth@clauth";
/// Marketplace key in `known_marketplaces.json`.
pub(crate) const MARKETPLACE_KEY: &str = "clauth";

/// Resolve `binary` against `PATH`, returning the first hit. The OS does this
/// implicitly when spawning, but a presence *check* needs it spelled out. On
/// Windows the usual executable extensions are tried too; on Unix the exec bit is
/// required so a non-executable file named `clauth` doesn't read as "resolved".
pub(crate) fn on_path(binary: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        exts.iter()
            .map(|ext| dir.join(format!("{binary}{ext}")))
            .find(|candidate| is_executable(candidate))
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// One install record from `plugins["clauth@clauth"]`. Every field is optional —
/// CC's schema is treated leniently so a shape change degrades to "unknown"
/// rather than a parse error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InstallRecord {
    pub(crate) scope: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) git_commit_sha: Option<String>,
    pub(crate) installed_at: Option<String>,
    pub(crate) install_path: Option<String>,
    pub(crate) project_path: Option<String>,
}

/// Marketplace source for `clauth`, from `known_marketplaces.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MarketplaceInfo {
    pub(crate) repo: Option<String>,
    pub(crate) install_location: Option<String>,
}

/// Where a manual `mcpServers.clauth` entry lives, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpWiring {
    /// `~/.claude.json` (user-global) carries it.
    GlobalConfig,
    /// A project `./.mcp.json` carries it.
    ProjectFile,
    /// No manual wiring found.
    None,
}

/// Install records for the clauth plugin; empty when the registry is absent,
/// unreadable, or carries no entry (all read as "not installed").
pub(crate) fn installed_records() -> Vec<InstallRecord> {
    let Some(root) = read_json(plugins_dir().map(|dir| dir.join("installed_plugins.json"))) else {
        return Vec::new();
    };
    root.get("plugins")
        .and_then(|plugins| plugins.get(PLUGIN_ID))
        .and_then(Value::as_array)
        .map(|records| records.iter().map(install_record_from).collect())
        .unwrap_or_default()
}

/// Marketplace record for clauth, when the marketplace is known to CC.
pub(crate) fn marketplace_known() -> Option<MarketplaceInfo> {
    let root = read_json(plugins_dir().map(|d| d.join("known_marketplaces.json")))?;
    let entry = root.get(MARKETPLACE_KEY)?;
    Some(MarketplaceInfo {
        repo: entry
            .get("source")
            .and_then(|source| source.get("repo"))
            .and_then(Value::as_str)
            .map(str::to_string),
        install_location: entry
            .get("installLocation")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Manual `mcpServers.clauth` wiring, preferring the user-global config over a
/// project file (the global is the one the fix writes).
pub(crate) fn manual_mcp_wiring() -> McpWiring {
    if global_claude_json_path().is_some_and(|path| json_has_clauth_mcp(&path)) {
        McpWiring::GlobalConfig
    } else if json_has_clauth_mcp(Path::new(".mcp.json")) {
        McpWiring::ProjectFile
    } else {
        McpWiring::None
    }
}

/// `claude --version`, trimmed to its first line. `None` when the binary is
/// missing or the call fails — the row then reports "unknown".
pub(crate) fn cc_version() -> Option<String> {
    let output = Command::new("claude")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// `~/.claude.json` (user-global), the file the wire fix edits.
pub(crate) fn global_claude_json_path() -> Option<PathBuf> {
    Some(home_dir().ok()?.join(".claude.json"))
}

/// Write `mcpServers.clauth` into `~/.claude.json`, preserving every other field
/// (key order is kept via serde_json's `preserve_order`). The entry mirrors the
/// plugin manifest so a manual wire matches a plugin install. Creates the file
/// when absent.
pub(crate) fn wire_mcp_server() -> Result<()> {
    let path = home_dir()?.join(".claude.json");
    let mut root: Map<String, Value> = match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        },
        Err(_) => Map::new(),
    };
    let entry = clauth_mcp_entry();
    match root
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
    {
        Value::Object(servers) => {
            servers.insert("clauth".to_string(), entry);
        }
        // `mcpServers` existed but wasn't an object — replace it with a fresh map.
        other => *other = Value::Object(Map::from_iter([("clauth".to_string(), entry)])),
    }
    atomic_write(&path, serde_json::to_vec_pretty(&Value::Object(root))?)?;
    Ok(())
}

/// The canonical stdio entry clauth registers (matches `plugin.json`).
fn clauth_mcp_entry() -> Value {
    serde_json::json!({ "type": "stdio", "command": "clauth", "args": ["mcp"] })
}

fn plugins_dir() -> Option<PathBuf> {
    Some(claude_dir().ok()?.join("plugins"))
}

/// Parse a JSON file into a `Value`, returning `None` on any missing/unreadable/
/// unparseable input (graceful — the registry files often don't exist).
fn read_json(path: Option<PathBuf>) -> Option<Value> {
    let bytes = std::fs::read(path?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn json_has_clauth_mcp(path: &Path) -> bool {
    read_json(Some(path.to_path_buf()))
        .and_then(|root| {
            root.get("mcpServers")
                .and_then(|servers| servers.get("clauth"))
                .cloned()
        })
        .is_some()
}

fn install_record_from(value: &Value) -> InstallRecord {
    let field = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    InstallRecord {
        scope: field("scope"),
        version: field("version"),
        git_commit_sha: field("gitCommitSha"),
        installed_at: field("installedAt"),
        install_path: field("installPath"),
        project_path: field("projectPath"),
    }
}

#[cfg(test)]
#[path = "../tests/inline/plugin_probe.rs"]
mod tests;
