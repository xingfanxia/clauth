//! `clauth start <name>` — spawn `claude` against the profile's persistent
//! runtime directory. See [`crate::runtime`] for the shared-runtime design;
//! this module is just the thin wrapper that owns the lifetime guard.

use std::process::{Command, ExitStatus};
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

    let runtime = {
        let _spinner = Spinner::start("clauth: preparing runtime");
        ProfileRuntime::acquire(profile, isolation)?
    };

    #[cfg(unix)]
    let signal_watcher = SignalWatcher::new()?;

    let mut command = Command::new("claude");
    command.env("CLAUDE_CONFIG_DIR", runtime.config_dir());
    // Isolated: also suppress global/project MCP servers wired through
    // `.claude.json`, so the only extension surface is what the caller passes.
    if isolation == Isolation::Isolated {
        command.arg("--strict-mcp-config");
    }
    let mut child = command
        .args(claude_args)
        .spawn()
        .context("failed to spawn claude")?;

    #[cfg(unix)]
    let outcome = wait_for_child(&mut child, signal_watcher.receiver())?;

    #[cfg(not(unix))]
    let outcome = ChildOutcome {
        status: child.wait().context("failed to wait for claude")?,
        signal: None,
    };

    // Drop runtime before process::exit so final sync + refcount cleanup runs.
    drop(runtime);

    let code = status_code(outcome.status, outcome.signal);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
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
        eprintln!("clauth: failed to forward signal to claude: {e}");
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
