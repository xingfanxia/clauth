use super::*;
use std::fs;
use std::process::Command;

use crate::testutil::HomeSandbox;

#[cfg(unix)]
fn signal_status(signal: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;

    ExitStatus::from_raw(signal)
}

#[test]
fn status_code_preserves_plain_exit_code() {
    let status = Command::new("sh")
        .args(["-c", "exit 7"])
        .status()
        .expect("status");

    assert_eq!(status_code(status, None), 7);
}

#[cfg(unix)]
#[test]
fn status_code_preserves_child_signal_code() {
    assert_eq!(status_code(signal_status(15), None), 143);
}

#[cfg(unix)]
#[test]
fn status_code_reports_parent_signal_after_successful_child_exit() {
    let status = Command::new("sh")
        .args(["-c", "exit 0"])
        .status()
        .expect("status");

    assert_eq!(status_code(status, Some(SIGINT)), 130);
}

// ── apply_spawn_cwd: the resume-cwd primitive (part A) ──

/// `Some(workspace)` pins the child's cwd to that dir — the load-bearing resume
/// guarantee: the resumed `claude` runs in the session's recorded workspace.
#[test]
fn apply_spawn_cwd_pins_child_to_workspace() {
    let mut cmd = Command::new("true");
    let ws = std::path::Path::new("/tmp/clauth-resume-ws");
    let resolved = apply_spawn_cwd(&mut cmd, Some(ws));
    assert_eq!(
        cmd.get_current_dir(),
        Some(ws),
        "the child cwd must equal the recorded workspace"
    );
    assert_eq!(resolved.as_deref(), Some(ws));
}

/// `None` sets no explicit cwd, so the child inherits this process's — byte-for-
/// byte the pre-resume behavior. `get_current_dir` is `None` (unset) in that case.
#[test]
fn apply_spawn_cwd_none_inherits_process_cwd() {
    let mut cmd = Command::new("true");
    let resolved = apply_spawn_cwd(&mut cmd, None);
    assert_eq!(
        cmd.get_current_dir(),
        None,
        "None must not pin the child's cwd"
    );
    assert_eq!(resolved, std::env::current_dir().ok());
}

// ── auto-rescue: the effective-decision + isolated-store teardown ──

/// The pure effective-decision: a per-run `--rescue`/`--no-rescue` override
/// (`Some`) beats the persisted `auto_rescue` toggle; with no override the
/// toggle decides. This is the whole gate `run` composes with `isolation ==
/// Isolated` at teardown.
#[test]
fn rescue_effective_override_beats_toggle() {
    // No per-run flag → the persisted toggle decides.
    assert!(
        !rescue_effective(None, false),
        "default OFF stays off (discard)"
    );
    assert!(rescue_effective(None, true), "toggle ON rescues");
    // A per-run flag overrides the toggle either way.
    assert!(
        !rescue_effective(Some(false), true),
        "--no-rescue beats a true toggle"
    );
    assert!(
        rescue_effective(Some(true), false),
        "--rescue beats a false toggle"
    );
}

/// Bit-identical guard: with rescue OFF (default toggle, no flag) the teardown
/// gate never invokes the store move, so the isolated transcript is left for the
/// runtime GC to discard and the global store stays empty.
#[test]
fn rescue_off_leaves_isolated_store_to_discard() {
    let sb = HomeSandbox::new();
    let iso = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated/projects");
    let global = sb.home().join(".claude/projects");
    let src = iso.join("-w-iso/s1.jsonl");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "transcript").unwrap();

    // Mirror teardown exactly: decide via the real fn, only move when true.
    let moved = if rescue_effective(None, false) {
        crate::sessions::rescue_isolated_store(&iso, &global)
    } else {
        0
    };

    assert_eq!(moved, 0, "default OFF must not rescue");
    assert!(
        src.exists(),
        "the isolated transcript is left to be discarded"
    );
    assert!(
        !global.join("-w-iso/s1.jsonl").exists(),
        "the global store stays empty — stock discard behavior"
    );
}

/// Rescue ON (via the toggle) lifts the isolated transcript into the global
/// store: it becomes resumable (mirrored `<slug>/<id>.jsonl`) and the source is
/// moved, not copied.
#[test]
fn rescue_on_moves_isolated_transcript_into_global_store() {
    let sb = HomeSandbox::new();
    let iso = sb
        .home()
        .join(".clauth/profiles/iso/runtime-isolated/projects");
    let global = sb.home().join(".claude/projects");
    let src = iso.join("-w-iso/s1.jsonl");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "transcript").unwrap();

    let moved = if rescue_effective(None, true) {
        crate::sessions::rescue_isolated_store(&iso, &global)
    } else {
        0
    };

    assert_eq!(moved, 1, "toggle ON rescues the one transcript");
    let landed = global.join("-w-iso/s1.jsonl");
    assert!(
        landed.exists(),
        "the transcript lands in the resumable global store"
    );
    assert_eq!(fs::read(&landed).unwrap(), b"transcript");
    assert!(!src.exists(), "source moved, not copied");
}
