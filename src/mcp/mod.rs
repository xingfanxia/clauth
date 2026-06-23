//! `clauth mcp` — MCP JSON-RPC 2.0 server over stdio (rmcp).
//!
//! Exposes clauth profiles to a live Claude Code session: list/usage, switch,
//! and delegate. The rest of the binary stays synchronous; [`serve`] builds a
//! scoped current-thread tokio runtime and blocks on the stdio server.
//!
//! All logging MUST go to stderr — stdout carries the JSON-RPC frame.

mod render;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::profile::{AppConfig, Profile, load_config};
use crate::profile_cache::{
    THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache, profile_cache_mtime_ms,
};
use crate::providers::ThirdPartyStats;
use crate::runtime::ProfileRuntime;
use crate::usage::{UsageInfo, UsageWindow, humanize_duration, now_epoch_secs, now_ms};
use render::ProfileSnapshot;

/// Display provider for a profile: a recognised third-party name, else
/// `anthropic` for an OAuth profile.
fn provider_label(profile: &Profile) -> String {
    profile
        .provider
        .map(|p| p.display_name().to_string())
        .unwrap_or_else(|| "anthropic".to_string())
}

/// OAuth subscription tier from the stored token (`max`/`pro`/…), when present.
fn subscription_type(profile: &Profile) -> Option<String> {
    profile
        .credentials
        .as_ref()?
        .claude_ai_oauth
        .as_ref()?
        .subscription_type
        .clone()
}

/// Fresh-from-cache 5h/7d windows for a profile. Each call re-reads the disk
/// cache (no caching across tool calls per the design).
fn load_windows(name: &str) -> (Option<UsageWindow>, Option<UsageWindow>) {
    match load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE) {
        Some(u) => (u.five_hour, u.seven_day),
        None => (None, None),
    }
}

/// Compact "Nm ago" / "Nh ago" age label for the active profile's usage cache
/// mtime, or `unknown` when no cache has been written yet.
fn cache_age_label(active: Option<&str>) -> String {
    let age_secs = active
        .and_then(|n| profile_cache_mtime_ms(n, USAGE_CACHE_FILE))
        .map(|ms| (now_ms().saturating_sub(ms) / 1000) as i64);
    match age_secs {
        Some(s) => format!("{} ago", humanize_duration(s)),
        None => "unknown (no cached usage yet)".to_string(),
    }
}

/// Live footer for the current active profile, read fresh from cache.
fn active_footer(config: &AppConfig) -> String {
    let active = config.state.active_profile.as_deref();
    let (five_h, seven_d) = match active {
        Some(name) => load_windows(name),
        None => (None, None),
    };
    render::live_footer(active, five_h.as_ref(), seven_d.as_ref())
}

/// Append the live footer to a JSON text payload as a second content block.
fn with_footer(json: serde_json::Value, footer: String) -> Vec<Content> {
    vec![Content::text(json.to_string()), Content::text(footer)]
}

#[derive(Clone)]
pub(crate) struct ClauthServer {
    // consumed by the `#[tool_handler]` macro at dispatch time; rustc's
    // dead-code pass can't see through the macro plumbing.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SwitchArgs {
    /// Profile name to relink the global active credentials to.
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct RunArgs {
    /// Profile name to run the headless delegate session under.
    profile: String,
    /// Prompt passed to the delegated `claude -p` session.
    prompt: String,
    /// Optional model override for the delegated session.
    model: Option<String>,
}

#[tool_router]
impl ClauthServer {
    pub(crate) fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "List all clauth profiles with cached 5h/7d usage and live-session state")]
    async fn list_profiles(&self) -> Result<CallToolResult, ErrorData> {
        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let profiles: Vec<serde_json::Value> = config
            .profiles
            .iter()
            .map(|p| {
                let name = p.name.as_str();
                let (five_h, seven_d) = load_windows(name);
                let third_party = if p.is_third_party() {
                    load_profile_cache::<ThirdPartyStats>(name, THIRD_PARTY_CACHE_FILE)
                        .as_ref()
                        .map(render::third_party_headline)
                } else {
                    None
                };
                let windows: Vec<serde_json::Value> = [("5h", &five_h), ("7d", &seven_d)]
                    .into_iter()
                    .filter_map(|(label, w)| {
                        w.as_ref().map(|w| {
                            serde_json::json!({
                                "label": label,
                                "utilization_pct": w.utilization,
                                "resets_at": w.resets_at,
                            })
                        })
                    })
                    .collect();
                serde_json::json!({
                    "name": name,
                    "active": config.is_active(name),
                    "provider": provider_label(p),
                    "base_url": p.base_url,
                    "subscription_type": subscription_type(p),
                    "has_live_session": crate::runtime::has_live_session(name),
                    "windows": windows,
                    "third_party": third_party,
                })
            })
            .collect();

        let payload = serde_json::json!({ "profiles": profiles });
        Ok(CallToolResult::success(vec![Content::text(
            payload.to_string(),
        )]))
    }

    #[tool(description = "Report which profile owns the credentials loaded by this session")]
    async fn which(&self) -> Result<CallToolResult, ErrorData> {
        let config = load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let resolved = crate::which::resolve_active(&config);
        let payload = serde_json::json!({
            "profile": resolved.as_ref().map(|(name, _)| name),
            "source": resolved.as_ref().map(|(_, source)| source.as_str()),
        });
        Ok(CallToolResult::success(with_footer(
            payload,
            active_footer(&config),
        )))
    }

    #[tool(
        description = "Relink the global active profile (`~/.claude` credentials). A `clauth start` session is pinned to its own runtime and unaffected; a session on the global credentials adopts the change on its next token refresh"
    )]
    async fn switch(
        &self,
        Parameters(SwitchArgs { name }): Parameters<SwitchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut config =
            load_config().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        // Resolve the raw tool argument to a stored profile (case-insensitive)
        // BEFORE any mutation — the same guard the CLI applies. Skipping it lets an
        // unknown/wrong-case name reach `link_profile_credentials`, which strips the
        // live `.credentials.json` symlink and creates no replacement (it only errors
        // later at `finish_switch`), leaving the global session credential-less.
        let Some(name) = config.canonical_name(&name) else {
            let payload =
                serde_json::json!({ "ok": false, "reason": format!("profile not found: {name}") });
            return Ok(CallToolResult::error(with_footer(
                payload,
                active_footer(&config),
            )));
        };
        let on_divergence = config.state.default_divergence;

        match crate::actions::switch_profile_noninteractive(&mut config, &name, on_divergence) {
            Ok((previous, active)) => {
                let payload = serde_json::json!({
                    "ok": true,
                    "previous": previous,
                    "active": active,
                });
                Ok(CallToolResult::success(with_footer(
                    payload,
                    active_footer(&config),
                )))
            }
            Err(e) => {
                let payload = serde_json::json!({ "ok": false, "reason": e.to_string() });
                Ok(CallToolResult::error(with_footer(
                    payload,
                    active_footer(&config),
                )))
            }
        }
    }

    #[tool(
        description = "Delegate a headless task to a profile; spends that account's window. Returns the run envelope"
    )]
    async fn run(
        &self,
        Parameters(RunArgs {
            profile,
            prompt,
            model,
        }): Parameters<RunArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Fail closed: a present-but-unparseable value is treated as max depth
        // (refuse), so a corrupt env can never re-enable delegation. Only a truly
        // absent var is depth 0.
        let depth: u32 = match std::env::var(MCP_DEPTH_ENV) {
            Ok(v) => v.trim().parse().unwrap_or(u32::MAX),
            Err(_) => 0,
        };
        if depth >= 1 {
            let payload = serde_json::json!({
                "profile": profile,
                "is_error": true,
                "result": "delegation depth exceeded (max 1)",
            });
            return Ok(CallToolResult::error(vec![Content::text(
                payload.to_string(),
            )]));
        }

        let target = profile.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            run_delegate(&target, &prompt, model.as_deref(), depth)
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("delegate task panicked: {e}"), None))?;

        let envelope = match outcome {
            Ok(v) => v,
            Err(reason) => serde_json::json!({
                "profile": profile,
                "is_error": true,
                "result": reason,
            }),
        };

        let (five_h, seven_d) = load_windows(&profile);
        let footer = render::live_footer(Some(profile.as_str()), five_h.as_ref(), seven_d.as_ref());
        let is_error = envelope
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let content = with_footer(envelope, footer);
        if is_error {
            Ok(CallToolResult::error(content))
        } else {
            Ok(CallToolResult::success(content))
        }
    }
}

/// Env var carrying the MCP delegation depth; the child `claude` inherits
/// `depth+1` so a delegate cannot itself delegate (hard cap at 1).
const MCP_DEPTH_ENV: &str = "CLAUTH_MCP_DEPTH";

/// Fixed internal timeout for a delegated `run` (not caller-tunable).
const RUN_TIMEOUT: Duration = Duration::from_secs(300);

/// Poll interval mirroring `start.rs`'s `wait_for_child` cadence.
const RUN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Blocking delegate: acquire the target profile's runtime, spawn a headless
/// `claude -p` with piped stdio, enforce a fixed timeout, and parse its JSON
/// envelope. Returns `Ok(envelope)` on a clean parse, or `Err(reason)` for a
/// timeout, non-zero exit, or unparseable output (the caller wraps it in an
/// `is_error` envelope). Never bubbles a transport-level error.
fn run_delegate(
    profile: &str,
    prompt: &str,
    model: Option<&str>,
    depth: u32,
) -> std::result::Result<serde_json::Value, String> {
    let config = load_config().map_err(|e| format!("failed to load config: {e}"))?;
    let target = config
        .find(profile)
        .ok_or_else(|| format!("profile not found: {profile}"))?;

    // Guard kept alive across spawn+wait; dropped on return for RAII teardown.
    let runtime =
        ProfileRuntime::acquire(target).map_err(|e| format!("failed to acquire runtime: {e}"))?;

    let mut command = Command::new("claude");
    command
        .env("CLAUDE_CONFIG_DIR", runtime.config_dir())
        .env(MCP_DEPTH_ENV, (depth + 1).to_string())
        .args([
            "-p",
            prompt,
            "--output-format",
            "json",
            "--strict-mcp-config",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    if let Some(m) = model {
        command.args(["--model", m]);
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to spawn claude: {e}"))?;

    // Drain both pipes on their own threads from the moment of spawn. A bare
    // try_wait loop never reads, so a >~64KiB result blocks the child on a full
    // pipe and it never exits — a false timeout that drops a valid result. Killing
    // the child closes the write ends, the readers hit EOF, and the joins return.
    let stdout_reader = child
        .stdout
        .take()
        .map(|mut h| std::thread::spawn(move || drain_pipe(&mut h)));
    let stderr_reader = child
        .stderr
        .take()
        .map(|mut h| std::thread::spawn(move || drain_pipe(&mut h)));

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= RUN_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "delegate timed out after {}s",
                        RUN_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(RUN_POLL_INTERVAL);
            }
            Err(e) => return Err(format!("failed to wait for claude: {e}")),
        }
    };

    let stdout_bytes = join_reader(stdout_reader);
    let stderr_bytes = join_reader(stderr_reader);
    let stdout = String::from_utf8_lossy(&stdout_bytes);
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        return Err(format!(
            "claude exited with {}: {}",
            status
                .code()
                .map_or_else(|| "signal".to_string(), |c| c.to_string()),
            truncate(stderr.trim(), 2000)
        ));
    }
    serde_json::from_str::<serde_json::Value>(stdout.trim()).map_err(|e| {
        format!(
            "failed to parse claude output: {e}: {}",
            truncate(stdout.trim(), 2000)
        )
    })
}

/// Read a child pipe to EOF into a buffer, swallowing read errors (a partial
/// buffer is more useful than a hard failure for an error envelope).
fn drain_pipe<R: std::io::Read>(reader: &mut R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    buf
}

/// Join a reader thread, returning its drained bytes (empty on a join panic or
/// an absent pipe).
fn join_reader(handle: Option<std::thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle.and_then(|h| h.join().ok()).unwrap_or_default()
}

/// Truncate a string to `max` bytes (on a char boundary) for an error payload,
/// appending an ellipsis when clipped.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[tool_handler]
impl ServerHandler for ClauthServer {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo is #[non_exhaustive]; build from default then set fields.
        // Tools capability must be advertised explicitly: ServerInfo::default() leaves
        // capabilities empty, so a spec-compliant client (Claude Code) exposes no tools
        // at all even though the server can answer tools/list.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(build_instructions());
        info
    }
}

/// Build the init-time `instructions` block once from the on-demand config and
/// usage disk cache. Best-effort: a config load failure degrades to a prose-only
/// block rather than failing the handshake.
fn build_instructions() -> String {
    let Ok(config) = load_config() else {
        return "clauth manages multiple Claude Code accounts (\"profiles\"). \
            Call `list_profiles` for live usage figures."
            .to_string();
    };
    let now = now_epoch_secs();
    let active = config.state.active_profile.as_deref();
    let age = cache_age_label(active);

    let snapshots: Vec<ProfileSnapshot> = config
        .profiles
        .iter()
        .map(|p| {
            let name = p.name.as_str();
            let (five_h, seven_d) = load_windows(name);
            let third_party = if p.is_third_party() {
                load_profile_cache::<ThirdPartyStats>(name, THIRD_PARTY_CACHE_FILE)
                    .as_ref()
                    .map(render::third_party_headline)
            } else {
                None
            };
            ProfileSnapshot {
                name: name.to_string(),
                active: config.is_active(name),
                provider: provider_label(p),
                base_url: p.base_url.clone(),
                sub_type: subscription_type(p),
                five_h,
                seven_d,
                third_party,
            }
        })
        .collect();

    render::instructions_block(&snapshots, &crate::which::session_auth(), &age, now)
}

pub(crate) fn serve() -> Result<()> {
    // rmcp's service loop arms a Tokio timer (needs `enable_time`), so a bare
    // current-thread runtime panics right after the initialize reply. `enable_all`
    // also turns on the I/O driver, covering a future transport that polls a real
    // fd or any added tokio net/process path.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_server())
}

async fn run_server() -> Result<()> {
    use rmcp::{ServiceExt, transport::stdio};
    let service = ClauthServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/inline/mcp_run.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/inline/mcp_switch_tool.rs"]
mod switch_tool_tests;
