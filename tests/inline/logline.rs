use super::*;

// 2026-05-17T14:20:00 UTC == 1779027600 epoch seconds (same fixture instant
// as the fetch ISO tests).
const BASE_UTC: i64 = 1_779_027_600;

#[test]
fn daemon_mode_prefixes_an_iso_utc_stamp() {
    assert_eq!(
        render(true, BASE_UTC, "clauth daemon: switched to 'b'"),
        "2026-05-17T14:20:00+00:00 clauth daemon: switched to 'b'",
        "daemon.log lines must be self-dating — incident forensics depend on it"
    );
}

#[test]
fn daemon_mode_stays_bare_on_stderr() {
    assert_eq!(
        render(false, BASE_UTC, "clauth: 'a' re-authenticated"),
        "clauth: 'a' re-authenticated",
        "the daemon's redirected stderr keeps the historical bare format"
    );
}

#[test]
fn only_a_non_daemon_line_on_a_terminal_diverts_to_the_log_file() {
    // The 2026-07-14 corruption: a background scheduler thread's stderr line
    // paints over the TUI's alternate screen. Diverting to the log file is the
    // fix, and it must fire in exactly one context.
    assert_eq!(
        route(false, true),
        Sink::LogFile,
        "interactive TUI/CLI on a tty"
    );
    assert_eq!(route(false, false), Sink::Stderr, "piped/redirected stderr");
    assert_eq!(
        route(true, true),
        Sink::Stderr,
        "daemon in a foreground console"
    );
    assert_eq!(
        route(true, false),
        Sink::Stderr,
        "daemon under a supervisor"
    );
}

#[test]
fn write_log_line_appends_each_call() {
    let path = std::env::temp_dir().join(format!("clauth-logline-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&path);
    write_log_line(&path, "first");
    write_log_line(&path, "second");
    let body = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        body, "first\nsecond\n",
        "each line appends, none clobbers the prior"
    );
    let _ = std::fs::remove_file(&path);
}

/// The log lands in `~/.clauth` and carries whatever an event line names
/// (profiles, endpoints, failure bodies), so it rides the same owner-only rule
/// as the rest of the tree.
#[cfg(unix)]
#[test]
fn write_log_line_creates_an_owner_only_file() {
    use std::os::unix::fs::PermissionsExt;

    let path = std::env::temp_dir().join(format!("clauth-logline-perm-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&path);
    write_log_line(&path, "first");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        mode, 0o600,
        "clauth.log mode should be 0o600, got {mode:#o}"
    );
}
