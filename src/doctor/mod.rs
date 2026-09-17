//! `clauth doctor` — a health diagnostic for the daemon + macOS wiring.
//!
//! Answers "why didn't it switch last night?" without the manual `launchctl
//! print` / lock-stat / socket-round-trip / Keychain-grant / version-compare
//! dance. It encodes that tribal knowledge as executable checks (the repo's own
//! agent-maintainability rule: make it executable, not prose).
//!
//! **Read-only except one throwaway Keychain write-probe.** No switch, no profile
//! edit, no write to the real `Claude Code-credentials` Keychain item. The only
//! write is a generic-password under the throwaway `clauth-doctor-probe` service,
//! added and immediately deleted, purely to prove this binary can write the login
//! Keychain at all — the real item is only ever read for presence (metadata, no
//! prompt).

mod core;

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use self::core::{Check, Status, exit_code, freshness, now_ms, read_status, skew};
use crate::daemon::SCHEMA_VERSION;
use crate::out::outln;
use crate::profile::clauth_dir;

// ── impure probes (each best-effort, never panics) ────────────────────────────

/// status.json presence + freshness → is the daemon actually writing? The single
/// most useful "is it alive" signal. Freshness is judged against the fixed 1s
/// write cadence (see `core::freshness`), NOT the usage refresh interval.
fn check_status_json(status_path: &Path) -> Check {
    let meta = match std::fs::metadata(status_path) {
        Ok(m) => m,
        Err(_) => {
            return Check::fail(
                "status.json",
                "absent — the daemon has never written it",
                "start the daemon: dist/macos/daemon-install.sh (macOS) or `clauth daemon`",
            );
        }
    };
    // Prefer the in-file generated_at; fall back to mtime if it won't parse.
    let (_, _, gen_at) = read_status(status_path).unwrap_or((None, None, None));
    let age_ms = gen_at
        .map(|g| now_ms().saturating_sub(g))
        .or_else(|| {
            meta.modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .map(|d| d.as_millis() as u64)
        })
        .unwrap_or(u64::MAX);
    let age = Duration::from_millis(age_ms);
    let detail = format!("last written {}s ago", age.as_secs());
    match freshness(age) {
        Status::Pass => Check::pass("status.json", detail),
        Status::Warn => Check::warn(
            "status.json",
            detail,
            "re-run `clauth doctor` in a few seconds; if still stale, restart the daemon",
        ),
        Status::Fail => Check::fail(
            "status.json",
            format!("{detail} — stale, the daemon is likely not running"),
            "restart it: launchctl kickstart -k gui/$(id -u)/com.clauth.daemon",
        ),
    }
}

/// Is a daemon holding the single-instance lock? `try_lock` succeeding means
/// NOTHING holds it → no daemon. We release immediately (advisory flock).
fn check_daemon_lock(lock_path: &Path) -> Check {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
    {
        Ok(f) => f,
        Err(e) => {
            return Check::warn(
                "daemon lock",
                format!("cannot open {}: {e}", lock_path.display()),
                "ensure ~/.clauth exists and is writable",
            );
        }
    };
    match file.try_lock() {
        // We got the lock → no live daemon holds it. Drop to release at once.
        Ok(()) => {
            drop(file);
            Check::fail(
                "daemon lock",
                "unheld — no clauth daemon is running",
                "launchctl kickstart -k gui/$(id -u)/com.clauth.daemon (or `clauth daemon`)",
            )
        }
        // Contended → a daemon owns it. That's the healthy state.
        Err(std::fs::TryLockError::WouldBlock) => {
            Check::pass("daemon lock", "held by a running daemon")
        }
        // A genuine lock error (unsupported fs / fd problem) is inconclusive, not
        // proof of a live daemon — surface it rather than a false PASS.
        Err(e) => Check::warn(
            "daemon lock",
            format!("lock probe errored: {e}"),
            "check ~/.clauth is on a normal local filesystem",
        ),
    }
}

/// Round-trip the control socket: connect, `snapshot`, expect `ok:true`.
#[cfg(unix)]
fn check_socket(sock_path: &Path) -> Check {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    if !sock_path.exists() {
        return Check::fail(
            "control socket",
            "clauthd.sock absent — the daemon isn't listening",
            "start/restart the daemon",
        );
    }
    let mut stream = match UnixStream::connect(sock_path) {
        Ok(s) => s,
        Err(e) => {
            return Check::fail(
                "control socket",
                format!("connect failed: {e}"),
                "restart the daemon: launchctl kickstart -k gui/$(id -u)/com.clauth.daemon",
            );
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    if stream.write_all(b"{\"cmd\":\"snapshot\"}\n").is_err() {
        return Check::fail("control socket", "write failed", "restart the daemon");
    }
    let mut reply = String::new();
    match BufReader::new(&stream).read_line(&mut reply) {
        Ok(_) if reply.contains("\"ok\":true") => {
            Check::pass("control socket", "clauthd.sock responds to snapshot")
        }
        Ok(_) => Check::warn(
            "control socket",
            format!("unexpected reply: {}", reply.trim()),
            "restart the daemon",
        ),
        Err(e) => Check::fail(
            "control socket",
            format!("no reply within timeout: {e}"),
            "restart the daemon",
        ),
    }
}

/// Version/schema skew between this CLI binary and the daemon's status.json.
fn check_version(status_path: &Path) -> Check {
    let (ver, schema, _) = read_status(status_path).unwrap_or((None, None, None));
    let (status, detail) = skew(
        env!("CARGO_PKG_VERSION"),
        SCHEMA_VERSION,
        ver.as_deref(),
        schema,
    );
    match status {
        Status::Pass => Check::pass("version", detail),
        Status::Warn => Check::warn(
            "version",
            detail,
            "restart the daemon onto the new binary: dist/macos/signed-install.sh",
        ),
        Status::Fail => Check::fail(
            "version",
            detail,
            "rebuild + restart both sides: dist/macos/signed-install.sh",
        ),
    }
}

/// macOS: is the LaunchAgent installed and running?
#[cfg(target_os = "macos")]
fn check_launchagent() -> Check {
    let out = std::process::Command::new("/bin/launchctl")
        .args(["list", "com.clauth.daemon"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let body = String::from_utf8_lossy(&o.stdout);
            // A loaded-but-not-running job shows `"PID" = ...` only while alive.
            if body.contains("\"PID\"") {
                Check::pass("LaunchAgent", "com.clauth.daemon loaded and running")
            } else {
                Check::warn(
                    "LaunchAgent",
                    "loaded but not currently running",
                    "launchctl kickstart -k gui/$(id -u)/com.clauth.daemon",
                )
            }
        }
        _ => Check::fail(
            "LaunchAgent",
            "com.clauth.daemon not installed",
            "dist/macos/daemon-install.sh",
        ),
    }
}

/// macOS: is the binary signed with a STABLE identity? An ad-hoc signature makes
/// the Keychain "Always Allow" grant re-prompt on every rebuild — the root cause
/// of the unattended 3am switch blocking on a prompt nobody clicks.
#[cfg(target_os = "macos")]
fn check_signing() -> Check {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return Check::warn(
                "code signature",
                format!("cannot resolve binary: {e}"),
                "n/a",
            );
        }
    };
    let out = std::process::Command::new("/usr/bin/codesign")
        .args(["-dvv", &exe.to_string_lossy()])
        .output();
    match out {
        Ok(o) => {
            // codesign writes to stderr.
            let info = String::from_utf8_lossy(&o.stderr);
            if info.contains("Signature=adhoc") || info.contains("linker-signed") {
                Check::warn(
                    "code signature",
                    "ad-hoc — Keychain 'Always Allow' will re-prompt on every rebuild",
                    "sign with a stable identity: dist/macos/signed-install.sh",
                )
            } else if let Some(line) = info.lines().find(|l| l.starts_with("Authority=")) {
                Check::pass(
                    "code signature",
                    format!("stable ({})", line.trim_start_matches("Authority=")),
                )
            } else {
                Check::pass("code signature", "signed")
            }
        }
        Err(e) => Check::warn("code signature", format!("codesign failed: {e}"), "n/a"),
    }
}

/// macOS: (a) is the real `Claude Code-credentials` login item present (metadata
/// read only — no prompt, no secret), and (b) can this binary write the login
/// Keychain at all? (b) is proven with a THROWAWAY `clauth-doctor-probe` item,
/// added then deleted — the real item is NEVER written.
#[cfg(target_os = "macos")]
fn check_keychain() -> Check {
    const SECURITY: &str = "/usr/bin/security";
    const REAL_SERVICE: &str = "Claude Code-credentials";
    const PROBE_SERVICE: &str = "clauth-doctor-probe";
    let account = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();

    let delete_probe = || {
        let _ = std::process::Command::new(SECURITY)
            .args([
                "delete-generic-password",
                "-s",
                PROBE_SERVICE,
                "-a",
                &account,
            ])
            .output();
    };

    // (a) presence of the real item — no `-w`, so no secret is read and no ACL
    // prompt is raised (only reading the password DATA would prompt).
    let present = std::process::Command::new(SECURITY)
        .args(["find-generic-password", "-s", REAL_SERVICE, "-a", &account])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    // (b) write-grant probe on a THROWAWAY service. Pre-delete self-heals a probe
    // orphaned by a prior mid-run kill; `-U` then adds; delete cleans up. Creating
    // a brand-new item never prompts. The real item is never touched here.
    delete_probe();
    let add = std::process::Command::new(SECURITY)
        .args([
            "add-generic-password",
            "-U",
            "-s",
            PROBE_SERVICE,
            "-a",
            &account,
            "-w",
            "probe",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    delete_probe();

    match (present, add) {
        (true, true) => Check::pass(
            "keychain",
            "login item present; write-grant OK (probe added+removed)",
        ),
        (false, true) => Check::warn(
            "keychain",
            "write-grant OK but no 'Claude Code-credentials' item yet",
            "run one switch so the login item is created (operator, live)",
        ),
        (_, false) => Check::fail(
            "keychain",
            "cannot write the login Keychain (a switch would fail)",
            "grant access / sign stably: dist/macos/signed-install.sh, then one live switch",
        ),
    }
}

/// Codex wiring health — `None` (no line at all) when the codex roster is
/// empty. WARN-only by design: a broken codex side must never fail doctor on a
/// machine whose claude side is healthy.
///
/// Two questions, both answered from the engine's own evidence rather than
/// from a second opinion: a chain the server declared dead (the quarantine
/// beside its store), and a chain whose standby refresh has stopped landing.
/// The managed-config and store-mode checks the fork carried here are gone —
/// upstream refuses that spawn outright, naming the file, which is a better
/// place to learn it than a doctor line you have to go read.
fn check_codex() -> Option<Check> {
    let codex = crate::codex_profiles::CodexState::load().ok()?;
    if codex.profiles().is_empty() {
        return None;
    }

    let quarantined: Vec<&str> = codex
        .profiles()
        .iter()
        .filter(|n| crate::codex_auth::read_quarantine(n.as_str()).is_some())
        .map(|n| n.as_str())
        .collect();
    if let Some(first) = quarantined.first() {
        let rest = quarantined.len().saturating_sub(1);
        let who = if rest == 0 {
            format!("'{first}'")
        } else {
            format!("'{first}' and {rest} more")
        };
        return Some(Check::warn(
            "codex",
            format!("{who}: the server rejected the stored chain"),
            "re-authenticate it with `clauth login <name> --codex --browser`",
        ));
    }

    // A refresh-capable chain whose last refresh is older than the standby
    // schedule's own window has stopped being kept alive — either the daemon
    // is down or the refresh is failing.
    let stale = codex.profiles().iter().find_map(|name| {
        let auth = crate::codex_auth::read_store_auth(name.as_str())?;
        auth.refresh_token()?;
        let age_days =
            (crate::usage::now_ms() as i64).saturating_sub(auth.last_refresh_ms()?) / (86_400 * 1000);
        (age_days > 8).then(|| (name.as_str().to_string(), age_days))
    });
    if let Some((name, days)) = stale {
        return Some(Check::warn(
            "codex",
            format!("'{name}' last refreshed {days} days ago — standby keep-alive not landing"),
            "check the daemon is running (the daemon lines above) and daemon.log for codex \
             standby errors",
        ));
    }

    Some(Check::pass(
        "codex",
        format!("{} profile(s), chains healthy", codex.profiles().len()),
    ))
}

/// CDX-5: the injection proxy's health, when a heartbeat file exists (the
/// machine has run `clauth proxy` at least once). WARN-only — the proxy is
/// opt-in and its absence is never a failure. Reports whether the heartbeat
/// is fresh (a proxy is serving) and whether the live `~/.codex/config.toml`
/// is pointed at a clauth provider (a read-only sniff — never edited).
fn check_codex_proxy() -> Option<Check> {
    let path = crate::proxy::heartbeat_path().ok()?;
    if !path.exists() {
        return None;
    }
    // Freshness against a generous window (the daemon's default interval x2
    // is the passive-leg standdown gate; use a fixed 5 min here — a proxy
    // touches the heartbeat every connection, so a 5-min-old one is stopped).
    let fresh = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|m| m.elapsed().ok())
        .is_some_and(|age| age.as_secs() < 300);

    // Read-only config sniff: does codex point at a clauth provider?
    let config_points_at_clauth = crate::actions::default_codex_operator_home()
        .ok()
        .and_then(|d| std::fs::read_to_string(d.join("config.toml")).ok())
        .is_some_and(|c| {
            c.contains("model_provider = \"clauth\"") || c.contains("[model_providers.clauth]")
        });

    if fresh {
        Some(Check::pass(
            "codex proxy",
            if config_points_at_clauth {
                "serving; codex config points at it".to_string()
            } else {
                "serving, but ~/.codex/config.toml is NOT pointed at it \
                 (run: clauth proxy --print-config)"
                    .to_string()
            },
        ))
    } else {
        Some(Check::warn(
            "codex proxy",
            "heartbeat is stale — no proxy is running",
            "start it with `clauth proxy`, or the codex feed falls back to passive reads",
        ))
    }
}

/// Run every check, print each line, and exit non-zero if any FAILed.
// vec_init_then_push misfires here: on non-macOS the cfg block below is erased,
// leaving Vec::new() + push, but the conditional pushes make vec![] unusable.
#[allow(clippy::vec_init_then_push)]
pub(crate) fn run() -> Result<()> {
    let dir = clauth_dir()?;
    let mut checks: Vec<Check> = Vec::new();

    #[cfg(target_os = "macos")]
    {
        checks.push(check_launchagent());
        checks.push(check_signing());
        checks.push(check_keychain());
    }
    checks.push(check_daemon_lock(&dir.join("clauthd.lock")));
    #[cfg(unix)]
    checks.push(check_socket(&dir.join("clauthd.sock")));
    checks.push(check_status_json(&dir.join("status.json")));
    checks.push(check_version(&dir.join("status.json")));
    // CDX-1 T9: only on installs that actually use codex profiles — a
    // claude-only machine gets no codex noise (and can never fail on it).
    if let Some(check) = check_codex() {
        checks.push(check);
    }
    // CDX-5: proxy status, only when a heartbeat exists (never on a machine
    // that has never run `clauth proxy`).
    if let Some(check) = check_codex_proxy() {
        checks.push(check);
    }

    outln!("clauth doctor {}\n", env!("CARGO_PKG_VERSION"));
    for c in &checks {
        outln!("{}", c.render());
    }
    let code = exit_code(&checks);
    outln!(
        "\n{} check(s), {} failing.",
        checks.len(),
        checks.iter().filter(|c| c.status == Status::Fail).count()
    );
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}
