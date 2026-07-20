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

/// The gate `run` applies plus the production teardown, so a change to either
/// shows up here. A fresh sessions dir holds one live marker: this session's.
fn teardown(
    rescue_override: Option<bool>,
    auto_rescue: bool,
    iso_root: &std::path::Path,
    claude_home: &std::path::Path,
) -> (usize, usize) {
    if !rescue_effective(rescue_override, auto_rescue) {
        return (0, 0);
    }
    let sessions = iso_root.with_file_name("sessions-isolated");
    fs::create_dir_all(&sessions).unwrap();
    let _self_marker = live_marker(&sessions.join("1234-0"));
    rescue_teardown(iso_root, &sessions, claude_home)
}

/// A live session's liveness marker: an open file holding the same exclusive
/// flock `ProfileRuntime::acquire` takes, so `live_sessions_at` counts it alive
/// (a second fd's `try_lock` conflicts even within one process). The returned
/// handle must stay in scope.
fn live_marker(path: &std::path::Path) -> fs::File {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    file.lock().unwrap();
    file
}

/// The refcount gate: the isolated runtime tree is SHARED by every session of
/// that profile+flavor (overlapping `delegate`s hold several), and only the last
/// one out sees it discarded. An exit while a sibling is still live must move
/// nothing — rescuing `shell-snapshots/` out from under a running Claude Code
/// would break its Bash tool mid-session.
#[test]
fn rescue_moves_nothing_while_a_sibling_session_is_live() {
    let sb = HomeSandbox::new();
    let iso = sb.home().join(".clauth/profiles/iso/runtime-isolated");
    let claude_home = sb.home().join(".claude");
    let sessions = sb.home().join(".clauth/profiles/iso/sessions-isolated");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(iso.join("shell-snapshots")).unwrap();
    fs::write(iso.join("shell-snapshots/snap.sh"), "live shell").unwrap();
    fs::create_dir_all(iso.join("projects/-w-iso")).unwrap();
    fs::write(iso.join("projects/-w-iso/s1.jsonl"), "transcript").unwrap();
    let _me = live_marker(&sessions.join("1234-0"));
    let _sibling = live_marker(&sessions.join("5678-0"));

    assert_eq!(
        rescue_teardown(&iso, &sessions, &claude_home),
        (0, 0),
        "a live sibling blocks both legs"
    );
    assert_eq!(
        fs::read_to_string(iso.join("shell-snapshots/snap.sh")).unwrap(),
        "live shell",
        "the live session's state stays where it is reading it"
    );
    assert!(iso.join("projects/-w-iso/s1.jsonl").exists());
    assert!(!claude_home.join("shell-snapshots").exists());
    assert!(!claude_home.join("projects").exists());

    // The sibling exits: the last session out rescues both legs.
    drop(_sibling);
    fs::remove_file(sessions.join("5678-0")).unwrap();
    assert_eq!(rescue_teardown(&iso, &sessions, &claude_home), (1, 1));
}

/// Bit-identical guard, sidecar half: with rescue OFF nothing at all leaves the
/// isolated tree — the sidecars are left for the runtime GC alongside the
/// transcripts.
#[test]
fn rescue_off_leaves_sidecars_to_discard() {
    let sb = HomeSandbox::new();
    let iso = sb.home().join(".clauth/profiles/iso/runtime-isolated");
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(iso.join("shell-snapshots")).unwrap();
    fs::write(iso.join("shell-snapshots/snap.sh"), "iso shell").unwrap();
    fs::create_dir_all(iso.join("projects/-w-iso")).unwrap();
    fs::write(iso.join("projects/-w-iso/s1.jsonl"), "transcript").unwrap();

    assert_eq!(
        teardown(None, false, &iso, &claude_home),
        (0, 0),
        "default OFF rescues neither leg"
    );
    assert!(iso.join("shell-snapshots/snap.sh").exists());
    assert!(
        !claude_home.join("shell-snapshots").exists(),
        "the global store stays untouched — stock discard behavior"
    );
}

/// A sidecar entry that cannot move (its global parent is occupied by a FILE)
/// is logged and skipped: teardown still completes, the rest of the sidecars
/// move, and the transcript leg's result is untouched.
#[test]
fn sidecar_failure_leaves_teardown_and_transcript_rescue_intact() {
    let sb = HomeSandbox::new();
    let iso = sb.home().join(".clauth/profiles/iso/runtime-isolated");
    let claude_home = sb.home().join(".claude");
    fs::create_dir_all(iso.join("projects/-w-iso")).unwrap();
    fs::write(iso.join("projects/-w-iso/s1.jsonl"), "transcript").unwrap();
    fs::create_dir_all(iso.join("file-history/sess-a")).unwrap();
    fs::write(iso.join("file-history/sess-a/edit-1.json"), "blocked").unwrap();
    fs::write(iso.join("file-history/ok.json"), "moves").unwrap();
    // A regular file where the rescue needs a directory: every move under it
    // fails at `create_dir_all`.
    fs::create_dir_all(claude_home.join("file-history")).unwrap();
    fs::write(claude_home.join("file-history/sess-a"), "in the way").unwrap();

    let (transcripts, sidecars) = teardown(None, true, &iso, &claude_home);

    assert_eq!(
        (transcripts, sidecars),
        (1, 1),
        "only the blocked entry fails"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("projects/-w-iso/s1.jsonl")).unwrap(),
        "transcript",
        "the transcript rescue's result stands"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("file-history/ok.json")).unwrap(),
        "moves"
    );
    assert_eq!(
        fs::read_to_string(claude_home.join("file-history/sess-a")).unwrap(),
        "in the way",
        "the blocking file is never replaced"
    );
    assert!(
        iso.join("file-history/sess-a/edit-1.json").exists(),
        "a failed move leaves its source in place, to be discarded"
    );
}
