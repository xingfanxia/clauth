use super::*;
use std::fs::{self, OpenOptions};
use std::time::{Duration, SystemTime};

// Minimal valid ClaudeCredentials JSON — has claudeAiOauth.accessToken.
const CREDS_V1: &[u8] = br#"{"claudeAiOauth":{"accessToken":"tok1"}}"#;
const CREDS_V2: &[u8] = br#"{"claudeAiOauth":{"accessToken":"tok2"}}"#;

fn set_mtime(path: &Path, when: SystemTime) {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for mtime");
    file.set_modified(when).expect("set_modified");
}

#[test]
fn sync_no_op_when_link_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert!(!canonical.exists());
}

#[cfg(unix)]
#[test]
fn sync_no_op_when_link_is_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    std::os::unix::fs::symlink(&canonical, &link_path).expect("symlink");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
    assert!(
        link_path
            .symlink_metadata()
            .expect("meta")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn sync_skips_invalid_json() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, b"not json").expect("write link");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
    // Link stayed a regular file — we wait for CC's write to complete.
    let meta = link_path.symlink_metadata().expect("meta");
    assert!(!meta.file_type().is_symlink());
}

#[test]
fn sync_skips_empty_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    // {} parses as ClaudeCredentials but has no OAuth token — treat as partial.
    fs::write(&link_path, b"{}").expect("write link");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

#[test]
fn sync_relinks_when_content_matches_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write link");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    assert!(!sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
    #[cfg(unix)]
    assert!(
        link_path
            .symlink_metadata()
            .expect("meta")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn sync_writes_canonical_when_differs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V2).expect("write link");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    assert!(sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V2);
    #[cfg(unix)]
    assert!(
        link_path
            .symlink_metadata()
            .expect("meta")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn sync_creates_canonical_when_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("nested").join("canonical.json");
    let link_path = tmp.path().join(".credentials.json");
    fs::write(&link_path, CREDS_V1).expect("write link");
    assert!(sync_credentials_unlocked(&link_path, &canonical).expect("sync"));
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
    #[cfg(unix)]
    assert!(
        link_path
            .symlink_metadata()
            .expect("meta")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn live_session_blocks_liveness_probe() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let pid_file = tmp.path().join("pid");
    let file = open_pid_file(&pid_file).expect("open");
    file.lock().expect("lock");
    assert!(is_session_alive(&pid_file));
    drop(file);
    assert!(!is_session_alive(&pid_file));
}

#[test]
fn prune_removes_dead_keeps_alive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let alive_path = tmp.path().join("alive");
    let dead_path = tmp.path().join("dead");
    let alive = open_pid_file(&alive_path).expect("open alive");
    alive.lock().expect("lock alive");
    fs::write(&dead_path, b"").expect("write dead");

    let count = prune_stale_sessions(tmp.path()).expect("prune");
    assert_eq!(count, 1);
    assert!(alive_path.exists());
    assert!(!dead_path.exists());
    drop(alive);
}

#[test]
fn copy_tree_replicates_files_and_subdirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("nested")).expect("mkdir");
    fs::write(src.join("a.txt"), b"hello").expect("write a");
    fs::write(src.join("nested").join("b.txt"), b"world").expect("write b");

    let dst = tmp.path().join("dst");
    copy_tree(&src, &dst).expect("copy_tree");

    assert_eq!(fs::read(dst.join("a.txt")).expect("read a"), b"hello");
    assert_eq!(
        fs::read(dst.join("nested").join("b.txt")).expect("read b"),
        b"world"
    );
}

#[test]
fn mirror_credentials_newer_runtime_wins() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    fs::write(&runtime, CREDS_V2).expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&canonical, past);
    set_mtime(&runtime, now);

    mirror_credentials(&runtime, &canonical).expect("mirror");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V2);
}

#[test]
fn mirror_credentials_newer_canonical_wins() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&canonical, CREDS_V2).expect("write canonical");
    fs::write(&runtime, CREDS_V1).expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&runtime, past);
    set_mtime(&canonical, now);

    mirror_credentials(&runtime, &canonical).expect("mirror");
    assert_eq!(fs::read(&runtime).expect("read"), CREDS_V2);
}

#[test]
fn mirror_credentials_skips_invalid_json() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    fs::write(&runtime, b"partial write").expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&canonical, past);
    set_mtime(&runtime, now);

    mirror_credentials(&runtime, &canonical).expect("mirror");
    // Canonical untouched; partial JSON ignored.
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

#[test]
fn mirror_credentials_skips_empty_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&canonical, CREDS_V1).expect("write canonical");
    // {} parses as valid JSON and as ClaudeCredentials, but has no OAuth token.
    fs::write(&runtime, b"{}").expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&canonical, past);
    set_mtime(&runtime, now);

    mirror_credentials(&runtime, &canonical).expect("mirror");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

#[test]
fn mirror_credentials_seeds_missing_side() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().join("nested").join("canonical.json");
    let runtime = tmp.path().join(".credentials.json");
    fs::write(&runtime, CREDS_V1).expect("write runtime");

    mirror_credentials(&runtime, &canonical).expect("mirror");
    assert_eq!(fs::read(&canonical).expect("read"), CREDS_V1);
}

#[test]
fn mirror_tree_propagates_runtime_edit_to_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(claude.join("todos.json"), b"[]").expect("write canonical");

    // Bootstrap the runtime copy so both sides hold the same byte stream.
    copy_tree(&claude, &runtime).expect("copy");

    // Simulate Claude Code rewriting the runtime copy.
    fs::write(runtime.join("todos.json"), br#"[{"id":1}]"#).expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&claude.join("todos.json"), past);
    set_mtime(&runtime.join("todos.json"), now);

    mirror_tree(&claude, &runtime).expect("mirror");
    assert_eq!(
        fs::read(claude.join("todos.json")).expect("read canonical"),
        br#"[{"id":1}]"#
    );
}

#[test]
fn mirror_tree_skips_top_level_settings_and_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(claude.join("settings.json"), br#"{"home":true}"#).expect("write h settings");
    fs::write(runtime.join("settings.json"), br#"{"runtime":true}"#).expect("write r settings");
    fs::write(claude.join(".credentials.json"), CREDS_V1).expect("write h creds");
    fs::write(runtime.join(".credentials.json"), CREDS_V2).expect("write r creds");

    mirror_tree(&claude, &runtime).expect("mirror");

    // Both sides keep their own settings.json and .credentials.json.
    assert_eq!(
        fs::read(claude.join("settings.json")).expect("read"),
        br#"{"home":true}"#
    );
    assert_eq!(
        fs::read(runtime.join("settings.json")).expect("read"),
        br#"{"runtime":true}"#
    );
    assert_eq!(
        fs::read(claude.join(".credentials.json")).expect("read"),
        CREDS_V1
    );
    assert_eq!(
        fs::read(runtime.join(".credentials.json")).expect("read"),
        CREDS_V2
    );
}

#[test]
fn mirror_tree_seeds_runtime_only_file_to_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(runtime.join("runtime-only.json"), br#"{"who":"cc"}"#).expect("write runtime");

    mirror_tree(&claude, &runtime).expect("mirror");
    assert_eq!(
        fs::read(claude.join("runtime-only.json")).expect("read"),
        br#"{"who":"cc"}"#
    );
    // Runtime side is preserved — no data loss.
    assert!(runtime.join("runtime-only.json").exists());
}

#[test]
fn mirror_tree_seeds_canonical_only_file_to_runtime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(claude.join("user-edit.json"), br#"{"who":"user"}"#).expect("write canonical");

    mirror_tree(&claude, &runtime).expect("mirror");
    assert_eq!(
        fs::read(runtime.join("user-edit.json")).expect("read"),
        br#"{"who":"user"}"#
    );
}

#[test]
fn mirror_tree_seeds_runtime_only_nested_to_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(claude.join("projects")).expect("mkdir claude/projects");
    fs::create_dir_all(runtime.join("projects").join("new")).expect("mkdir runtime nested");
    fs::write(
        runtime.join("projects").join("new").join("state.json"),
        br#"{"step":1}"#,
    )
    .expect("write runtime");

    mirror_tree(&claude, &runtime).expect("mirror");
    assert_eq!(
        fs::read(claude.join("projects").join("new").join("state.json")).expect("read"),
        br#"{"step":1}"#
    );
    assert!(
        runtime
            .join("projects")
            .join("new")
            .join("state.json")
            .exists()
    );
}

#[test]
fn mirror_tree_seeds_canonical_only_nested_to_runtime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(claude.join("projects").join("alpha")).expect("mkdir canonical nested");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    fs::write(
        claude.join("projects").join("alpha").join("notes.json"),
        br#"{"note":"hi"}"#,
    )
    .expect("write canonical");

    mirror_tree(&claude, &runtime).expect("mirror");
    assert_eq!(
        fs::read(runtime.join("projects").join("alpha").join("notes.json")).expect("read"),
        br#"{"note":"hi"}"#
    );
}

#[test]
fn detect_link_mode_returns_real_on_unix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mode = detect_link_mode(tmp.path()).expect("detect");
    // Unix CI always grants symlinks; Windows depends on dev mode.
    #[cfg(unix)]
    assert_eq!(mode, LinkMode::Real);
    #[cfg(not(unix))]
    let _ = mode;
}
