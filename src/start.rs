//! `clauth start <name>` — spawn `claude` against the profile's persistent
//! runtime directory. See [`crate::runtime`] for the shared-runtime design;
//! this module is just the thin wrapper that owns the lifetime guard.

use std::process::ExitStatus;
#[cfg(unix)]
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
#[cfg(unix)]
use std::thread::JoinHandle;
#[cfg(unix)]
use std::time::Duration;

use anyhow::{Context, Result};
#[cfg(unix)]
use signal_hook::consts::signal::{SIGINT, SIGTERM};
#[cfg(unix)]
use signal_hook::iterator::{Handle as SignalHandle, Signals};

#[cfg(unix)]
use crate::logline::logline;
use crate::profile::AppConfig;
use crate::runtime::{Isolation, ProfileRuntime};
use crate::spinner::Spinner;

#[cfg(unix)]
const CHILD_WAIT_INTERVAL: Duration = Duration::from_millis(50);

struct ChildOutcome {
    status: ExitStatus,
    signal: Option<i32>,
}

pub(crate) fn run(
    config: &AppConfig,
    name: &str,
    claude_args: &[String],
    isolation: Isolation,
) -> Result<()> {
    let profile = config.find(name).context("profile not found")?;

    // Strip the active profile's custom env from the inherited base so a
    // `clauth start <other>` session doesn't inherit it. The live
    // `settings.json` is owned by whoever is active; starting that same profile
    // passes its own keys, which the merge re-inserts (no-op).
    let active_env_keys: Vec<String> = config
        .state
        .active_profile
        .as_deref()
        .and_then(|n| config.find(n))
        .map(|p| p.env.keys().cloned().collect())
        .unwrap_or_default();

    let runtime = {
        let _spinner = Spinner::start("clauth: preparing runtime");
        ProfileRuntime::acquire(profile, isolation, &active_env_keys)?
    };

    #[cfg(unix)]
    let signal_watcher = SignalWatcher::new()?;

    let mut command = crate::runtime::claude_command();
    // Scrub clauth-managed + active custom env so a session started under
    // profile B doesn't inherit profile A's endpoint/auth/model overrides from
    // the parent process env. The target's runtime settings.json re-supplies
    // whichever it defines. Mirrors the delegate path (run_delegate).
    crate::runtime::scrub_profile_env(&mut command, &active_env_keys);
    // `claude` inherits this process's cwd (no `.current_dir()` call here); if
    // that's the real `$HOME`, its project-tier settings lookup would hit the
    // real `~/.claude/settings.json` and re-leak the globally active profile's
    // env, this time outranking the runtime settings.json below.
    if let Ok(cwd) = std::env::current_dir() {
        crate::runtime::guard_home_project_settings(&mut command, &cwd);
    }
    command.env("CLAUDE_CONFIG_DIR", runtime.config_dir());
    // Isolated: also suppress global/project MCP servers wired through
    // `.claude.json`, so the only extension surface is what the caller passes.
    // Deliberately NOT `--safe-mode`. The cross-account leak (the operator's
    // `~/.claude/plugins`) is already gone under the empty config dir. What
    // remains is a cwd `.claude/skills/*` plugin: project-local and trust-gated,
    // loading the same regardless of active account (like project CLAUDE.md).
    // `--safe-mode` would also nuke cwd CLAUDE.md + skills, so it stays off.
    if isolation == Isolation::Isolated {
        command.arg("--strict-mcp-config");
    }
    let child = command
        .args(claude_args)
        .spawn()
        .context("failed to spawn claude")?;

    #[cfg(unix)]
    let outcome = supervise_child(child, &signal_watcher)?;
    #[cfg(not(unix))]
    let outcome = supervise_child(child)?;

    // Drop runtime before process::exit so final sync + refcount cleanup runs.
    drop(runtime);

    let code = status_code(outcome.status, outcome.signal);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// `clauth start <codex-profile>` (CDX-1b): spawn `codex` against the
/// profile's isolated `CODEX_HOME` — its own auth.json (the profile chain),
/// a copied config.toml, isolated session history. The runtime guard owns
/// the lease + adopt-back watchdog; see [`crate::runtime::CodexRuntime`].
pub(crate) fn run_codex(name: &str, codex_args: &[String]) -> Result<()> {
    let runtime = {
        let _spinner = Spinner::start("clauth: preparing codex runtime");
        crate::runtime::CodexRuntime::acquire(name)?
    };

    #[cfg(unix)]
    let signal_watcher = SignalWatcher::new()?;

    // codex canonicalizes CODEX_HOME and hard-errors on a missing dir —
    // acquire just created it; canonicalize so a symlinked ~/.clauth works.
    let home = runtime
        .codex_home()
        .canonicalize()
        .unwrap_or_else(|_| runtime.codex_home().to_path_buf());
    let mut command = std::process::Command::new("codex");
    // Scrub an inherited CODEX_HOME (a parent isolated session must never
    // leak its home into a different profile's session) before pinning ours.
    command.env_remove("CODEX_HOME");
    command.env("CODEX_HOME", &home);
    let child = command
        .args(codex_args)
        .spawn()
        .context("failed to spawn codex — is the codex CLI installed?")?;

    #[cfg(unix)]
    let outcome = supervise_child(child, &signal_watcher)?;
    #[cfg(not(unix))]
    let outcome = supervise_child(child)?;

    // Drop before exit: final adopt-back + lease release + teardown.
    drop(runtime);

    let code = status_code(outcome.status, outcome.signal);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Wait a spawned child out (forwarding signals on unix) — the shared tail of
/// the claude and codex start paths.
#[cfg(unix)]
fn supervise_child(
    mut child: std::process::Child,
    signal_watcher: &SignalWatcher,
) -> Result<ChildOutcome> {
    wait_for_child(&mut child, signal_watcher.receiver())
}

#[cfg(not(unix))]
fn supervise_child(mut child: std::process::Child) -> Result<ChildOutcome> {
    Ok(ChildOutcome {
        status: child.wait().context("failed to wait for the child")?,
        signal: None,
    })
}

fn status_code(status: ExitStatus, signal: Option<i32>) -> i32 {
    if status.success() {
        return signal.map_or(0, |s| 128 + s);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| status.signal().map(|s| 128 + s).unwrap_or(1))
    }
    #[cfg(not(unix))]
    status.code().unwrap_or(1)
}

#[cfg(unix)]
struct SignalWatcher {
    handle: SignalHandle,
    thread: Option<JoinHandle<()>>,
    rx: Receiver<i32>,
}

#[cfg(unix)]
impl SignalWatcher {
    fn new() -> Result<Self> {
        let mut signals =
            Signals::new([SIGINT, SIGTERM]).context("failed to install signal handlers")?;
        let handle = signals.handle();
        let (tx, rx) = channel();
        #[allow(clippy::expect_used, reason = "thread spawn failure is unrecoverable")]
        let thread = std::thread::Builder::new()
            .name("clauth-sig".into())
            .spawn(move || {
                for signal in signals.forever() {
                    if tx.send(signal).is_err() {
                        break;
                    }
                }
            })
            .expect("failed to spawn signal watcher thread");
        Ok(Self {
            handle,
            thread: Some(thread),
            rx,
        })
    }

    fn receiver(&self) -> &Receiver<i32> {
        &self.rx
    }
}

#[cfg(unix)]
impl Drop for SignalWatcher {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(unix)]
fn wait_for_child(
    child: &mut std::process::Child,
    signals: &Receiver<i32>,
) -> Result<ChildOutcome> {
    loop {
        if let Some(status) = child.try_wait().context("failed to wait for claude")? {
            return Ok(ChildOutcome {
                status,
                signal: next_signal(signals),
            });
        }

        match signals.recv_timeout(CHILD_WAIT_INTERVAL) {
            Ok(signal) => {
                forward_signal_or_warn(child, signal);
                return wait_after_signal(child, signals, signal);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(CHILD_WAIT_INTERVAL),
        }
    }
}

#[cfg(unix)]
fn wait_after_signal(
    child: &mut std::process::Child,
    signals: &Receiver<i32>,
    first_signal: i32,
) -> Result<ChildOutcome> {
    let mut signal = first_signal;
    loop {
        match child.try_wait().context("failed to wait for claude")? {
            Some(status) => {
                return Ok(ChildOutcome {
                    status,
                    signal: Some(signal),
                });
            }
            None => match signals.recv_timeout(CHILD_WAIT_INTERVAL) {
                Ok(next) => {
                    signal = next;
                    forward_signal_or_warn(child, next);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => std::thread::sleep(CHILD_WAIT_INTERVAL),
            },
        }
    }
}

#[cfg(unix)]
fn next_signal(signals: &Receiver<i32>) -> Option<i32> {
    signals.try_recv().ok()
}

#[cfg(unix)]
fn forward_signal_or_warn(child: &std::process::Child, signal: i32) {
    if let Err(e) = forward_signal(child, signal)
        && e.raw_os_error() != Some(libc::ESRCH)
    {
        logline!("clauth: failed to forward signal to claude: {e}");
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn forward_signal(child: &std::process::Child, signal: i32) -> std::io::Result<()> {
    // SAFETY: `child.id()` is the OS pid for this live child; `signal` comes from signal-hook.
    let result = unsafe { libc::kill(child.id() as libc::pid_t, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
#[path = "../tests/inline/start.rs"]
mod tests;
