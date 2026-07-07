//! macOS Keychain access for the `Claude Code-credentials` login item.
//!
//! Claude Code on macOS stores its OAuth login in the login Keychain (a generic
//! password: service `Claude Code-credentials`, account = the OS login name), NOT
//! in `~/.claude/.credentials.json`. So clauth's symlink swap is cosmetic on
//! macOS unless the switched account is also written here — Claude Code keeps
//! reading the Keychain.
//!
//! **This integration is WRITE-ONLY.** clauth stores every profile as a file
//! (`~/.clauth/profiles/<name>/credentials.json`); the Keychain is touched ONLY
//! to make a switch real — a write on switch/link, a delete on clear. clauth does
//! NOT read the Keychain in any automatic path: reading Claude Code's own item
//! raises a macOS access prompt on every call, and clauth maintains its own token
//! chain (via `oauth.rs` rotation) so it never needs to read Claude's. `claude.rs`
//! wires the write/delete behind `#[cfg(target_os = "macos")]` + [`enabled`].
//!
//! **Why the `/usr/bin/security` CLI and not the `security-framework` crate.**
//! macOS binds a Keychain "Always Allow" grant to the *calling binary's code
//! signature*. A `cargo build`/`cargo install` binary carries an ad-hoc signature
//! whose identity changes on every rebuild, so a grant against clauth never
//! persists — every switch re-prompts. Routing the write through Apple's stable,
//! code-signed `/usr/bin/security` makes the one-time "Always Allow" bind to
//! *that* (unchanging) binary, so it sticks permanently and survives clauth
//! rebuilds — no code-signing dance required. (This is CCSwitcher's approach.)

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::profile::ClaudeCredentials;

/// Apple's Keychain CLI. Absolute path so a hostile `PATH` can't shim it.
const SECURITY_BIN: &str = "/usr/bin/security";

/// Wall-clock ceiling for a single `security` invocation before it is killed
/// (TECH-3). A stuck keychain — an unanswered "Always Allow" ACL prompt, a
/// locked keychain, or a hung home volume — must NOT pin the state flock forever:
/// the daemon runs the switch (hence this subprocess) inside `with_state_lock` on
/// its single-threaded run loop, so an unbounded child would wedge auto-switch,
/// the exact failure the daemon exists to prevent. Generous enough for a real
/// `add-generic-password`; the watchdog is the coarser backstop above this.
const SECURITY_TIMEOUT: Duration = Duration::from_secs(20);

/// Run `cmd` with a wall-clock deadline, killing (and reaping) the child if it
/// outlives `timeout`. Returns the collected [`Output`] on a normal exit, or an
/// error on spawn failure / timeout. Extracted so the deadline is unit-testable
/// with a benign hanging command (`sleep`) — no real Keychain is touched.
///
/// `stdin_payload`, when given, is written to the child's stdin which is then
/// closed (EOF) — the transport for `security -i`'s command line, keeping the
/// secret out of argv. The payload is a few KB and the write happens before the
/// poll loop; a macOS pipe buffer is 64 KB, so the single write cannot block.
///
/// `security` produces only a few bytes of output, so buffering it in the pipe
/// while we poll cannot deadlock on a full pipe buffer.
fn run_with_deadline(
    mut cmd: Command,
    timeout: Duration,
    stdin_payload: Option<&str>,
) -> Result<Output> {
    let mut child = cmd
        .stdin(if stdin_payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {SECURITY_BIN}"))?;
    if let Some(payload) = stdin_payload {
        use std::io::Write;
        // Write the payload, then close the pipe (drop of `stdin`) so the child
        // sees EOF and runs. On any write failure (e.g. EPIPE if it died early)
        // kill/wait the child before returning: a bare `?` would leak it as a
        // zombie, unlike the timeout and normal-exit paths below.
        let write_result: Result<()> = child
            .stdin
            .take()
            .context("child stdin unexpectedly absent")
            .and_then(|mut stdin| {
                stdin
                    .write_all(payload.as_bytes())
                    .with_context(|| format!("failed to write {SECURITY_BIN} stdin"))
            });
        if let Err(e) = write_result {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child
            .try_wait()
            .with_context(|| format!("failed to poll {SECURITY_BIN}"))?
        {
            Some(_status) => {
                return child
                    .wait_with_output()
                    .with_context(|| format!("failed to collect {SECURITY_BIN} output"));
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!(
                        "{SECURITY_BIN} exceeded its {}s deadline and was killed \
                         (keychain locked or an ACL prompt left unanswered?)",
                        timeout.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

/// Keychain generic-password service Claude Code reads/writes for its login.
const SERVICE: &str = "Claude Code-credentials";

/// `security(1)` exit status for `errSecItemNotFound` (-25300). Returned when no
/// matching item exists; treated as "absent" (`None`) on read and a no-op on delete.
const EXIT_ITEM_NOT_FOUND: i32 = 44;

/// Whether the live-credential paths in `claude.rs` route through the Keychain.
/// `true` in the shipped binary; `false` under `cfg(test)` so the test suite
/// keeps the file/symlink model and NEVER touches the operator's real
/// `Claude Code-credentials` item. The CLI plumbing itself is covered by the KC-1
/// round-trip test, which drives `read_at`/`write_at`/`delete_at` on a throwaway
/// service directly.
#[cfg(not(test))]
pub(crate) fn enabled() -> bool {
    true
}

#[cfg(test)]
pub(crate) fn enabled() -> bool {
    false
}

/// The Keychain `account` Claude Code stores the login under: the OS login name.
/// Empirically the login item is at `account = $USER`; a *separate* item at
/// `account = "unknown"` holds MCP tokens (`mcpOAuth`), not the login — so the
/// account must be pinned. A bare service-only lookup can return the wrong item.
fn account() -> Result<String> {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .context("cannot determine macOS login name ($USER/$LOGNAME unset) for Keychain access")
}

/// Read the credentials JSON stored at `(service, account)` via
/// `security find-generic-password -w`. `Ok(None)` when the item is absent
/// (exit 44); any other failure is an error. Test-only: the shipped integration
/// is write-only (reading Claude's item would prompt on every call). Covered by
/// the KC-1 round-trip test on a temp service.
#[cfg(test)]
fn read_at(service: &str, account: &str) -> Result<Option<ClaudeCredentials>> {
    let mut cmd = Command::new(SECURITY_BIN);
    cmd.args(["find-generic-password", "-s", service, "-a", account, "-w"]);
    let output = run_with_deadline(cmd, SECURITY_TIMEOUT, None)
        .with_context(|| format!("failed to run {SECURITY_BIN} find-generic-password"))?;
    if output.status.success() {
        // `-w` prints only the password (our JSON) followed by a trailing newline.
        let json = String::from_utf8(output.stdout).context("Keychain password is not UTF-8")?;
        let creds: ClaudeCredentials = serde_json::from_str(json.trim_end())
            .context("Keychain item is not valid Claude credentials JSON")?;
        Ok(Some(creds))
    } else if output.status.code() == Some(EXIT_ITEM_NOT_FOUND) {
        Ok(None)
    } else {
        Err(security_error("read", &output))
    }
}

/// Quote `s` for `security -i`'s line tokenizer: wrap in `"…"` with `\` → `\\`
/// and `"` → `\"`. Verified empirically (macOS 15 / Darwin 25): an escaped
/// quoted string round-trips byte-identical through `add-generic-password -w`,
/// including embedded spaces, double quotes, and backslashes; an UNquoted value
/// containing whitespace is split into separate argv words (usage error).
/// Embedded newlines are refused — `-i` is a line protocol, and a `\n` inside a
/// value would be parsed as a second command.
fn security_quote(s: &str) -> Result<String> {
    if s.contains('\n') || s.contains('\r') {
        anyhow::bail!("refusing to pass a value with an embedded newline to `security -i`");
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// Add-or-update the item at `(service, account)` with `creds` serialized as the
/// `{"claudeAiOauth":{…}}` JSON Claude Code expects, via
/// `security add-generic-password -U`. `-U` updates the item in place when it
/// already exists (created by Claude Code) and adds it otherwise.
///
/// The command line is fed to `security -i` over **stdin**, not argv, so the
/// token never appears in this process's own argv — keeping it out of
/// process-exec logging (Endpoint Security `es_event_exec_t`, i.e. most EDR
/// agents), which captures full command lines at exec time but not pipe
/// contents. (Plain same-UID `ps` exposure was already an accepted tradeoff —
/// TECH-9 #17: argv is readable only by the same UID or root on macOS, and a
/// same-UID process already owns the 0o600 credential files — but the EDR log
/// store was the one residual argv-only sink, and `-i` closes it.) `-i`'s
/// tokenizer needs the [`security_quote`] escaping for values with whitespace;
/// the inner command's exit code propagates as `security -i`'s own exit code
/// (verified: 0 on success, 44 for `errSecItemNotFound`, 2 on usage error).
/// The no-value `-w` prompt form is still unusable here — it reads from the
/// controlling *tty* (`readpassphrase`), not stdin, so a pipe can't feed it.
fn write_at(service: &str, account: &str, creds: &ClaudeCredentials) -> Result<()> {
    let json = serde_json::to_string(creds).context("failed to serialize Claude credentials")?;
    let line = format!(
        "add-generic-password -U -s {} -a {} -w {}\n",
        security_quote(service)?,
        security_quote(account)?,
        security_quote(&json)?,
    );
    let mut cmd = Command::new(SECURITY_BIN);
    cmd.arg("-i");
    let output = run_with_deadline(cmd, SECURITY_TIMEOUT, Some(&line))
        .with_context(|| format!("failed to run {SECURITY_BIN} add-generic-password"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(security_error("write", &output))
    }
}

/// Delete the item at `(service, account)` via `security delete-generic-password`.
/// Idempotent — a missing item (exit 44) is `Ok`.
fn delete_at(service: &str, account: &str) -> Result<()> {
    let mut cmd = Command::new(SECURITY_BIN);
    cmd.args(["delete-generic-password", "-s", service, "-a", account]);
    let output = run_with_deadline(cmd, SECURITY_TIMEOUT, None)
        .with_context(|| format!("failed to run {SECURITY_BIN} delete-generic-password"))?;
    if output.status.success() || output.status.code() == Some(EXIT_ITEM_NOT_FOUND) {
        Ok(())
    } else {
        Err(security_error("delete", &output))
    }
}

/// Build an error from a failed `security` invocation, including its stderr and
/// exit code (never the password, which travels only on the child's stdin —
/// `write_at` via `security -i` — and is never echoed to stderr).
fn security_error(op: &str, output: &std::process::Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output
        .status
        .code()
        .map_or_else(|| "signal".to_string(), |c| c.to_string());
    anyhow::anyhow!(
        "Keychain {op} failed (security exit {code}): {}",
        stderr.trim()
    )
}

/// Write `creds` as Claude Code's live OAuth login (add-or-update). This is what
/// makes an account switch real on macOS: Claude Code reads this on next launch.
pub(crate) fn keychain_write(creds: &ClaudeCredentials) -> Result<()> {
    write_at(SERVICE, &account()?, creds)
}

/// Remove Claude Code's live OAuth login from the Keychain (idempotent).
pub(crate) fn keychain_delete() -> Result<()> {
    delete_at(SERVICE, &account()?)
}

#[cfg(test)]
#[path = "../tests/inline/keychain.rs"]
mod tests;
