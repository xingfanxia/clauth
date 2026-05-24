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
fn mirror_tree_skips_identical_files_with_different_mtimes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let claude = tmp.path().join("claude");
    let runtime = tmp.path().join("runtime");
    fs::create_dir_all(&claude).expect("mkdir claude");
    fs::create_dir_all(&runtime).expect("mkdir runtime");
    let canonical_file = claude.join("state.json");
    let runtime_file = runtime.join("state.json");
    fs::write(&canonical_file, br#"{"same":true}"#).expect("write canonical");
    fs::write(&runtime_file, br#"{"same":true}"#).expect("write runtime");
    let past = SystemTime::now() - Duration::from_secs(60);
    let now = SystemTime::now();
    set_mtime(&canonical_file, past);
    set_mtime(&runtime_file, now);

    mirror_tree(&claude, &runtime).expect("mirror");

    assert_eq!(
        canonical_file
            .metadata()
            .expect("canonical meta")
            .modified()
            .ok(),
        Some(past)
    );
    assert_eq!(
        runtime_file
            .metadata()
            .expect("runtime meta")
            .modified()
            .ok(),
        Some(now)
    );
    assert_eq!(
        fs::read(&canonical_file).expect("read canonical"),
        br#"{"same":true}"#
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

// ── HOME-mutating tests ────────────────────────────────────────────────────────
//
// These tests override $HOME so profile_dir / clauth_dir / home_dir all
// resolve under a tempdir. The global clauth state lock (lock.rs LOCK static)
// and the in-process HOME variable must not be touched concurrently, so every
// test here must hold HOME_MUTEX for its entire duration.

use std::sync::{LazyLock, Mutex};

static HOME_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Set $HOME to `root` for the duration of `f`. Callers must hold `HOME_MUTEX`
/// before invoking so no two tests race on the env var.
fn with_fake_home<T>(root: &Path, f: impl FnOnce() -> T) -> T {
    let prev = std::env::var_os("HOME");
    // SAFETY: single-threaded access ensured by the caller holding HOME_MUTEX.
    unsafe { std::env::set_var("HOME", root) };
    let result = f();
    match prev {
        Some(v) => unsafe { std::env::set_var("HOME", v) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    result
}

/// Build a fake home with `~/.claude/` present (required by `acquire`).
fn fake_claude_home(root: &Path) -> PathBuf {
    let claude = root.join(".claude");
    fs::create_dir_all(&claude).expect("mkdir .claude");
    claude
}

fn make_profile(name: &str) -> crate::profile::Profile {
    crate::profile::Profile::new(name.to_string(), None, None)
}

// ── build_runtime_dir ─────────────────────────────────────────────────────────

#[test]
fn build_runtime_dir_writes_settings_not_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = HOME_MUTEX.lock().expect("home mutex");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(
            claude_home.join("settings.json"),
            br#"{"env":{"EXISTING":"1"}}"#,
        )
        .expect("write settings");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(&runtime, &claude_home, &profile, &canonical, LinkMode::Fake)
            .expect("build");

        let settings_dst = runtime.join("settings.json");
        // Must be a regular file, not a symlink.
        let meta = settings_dst.symlink_metadata().expect("settings present");
        assert!(
            !meta.file_type().is_symlink(),
            "settings.json must not be a symlink"
        );

        // Content must match what build_claude_settings_json would produce.
        let expected =
            build_claude_settings_json(&claude_home.join("settings.json"), &profile, &[])
                .expect("build_claude_settings_json");
        let actual = fs::read_to_string(&settings_dst).expect("read settings");
        assert_eq!(actual, expected);
    });
}

#[test]
fn build_runtime_dir_credentials_not_from_claude_home() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = HOME_MUTEX.lock().expect("home mutex");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        // Put a .credentials.json in ~/.claude/ — it must NOT appear in runtime.
        fs::write(claude_home.join(".credentials.json"), CREDS_V1).expect("write creds");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        // No canonical creds — so runtime/.credentials.json should be absent.
        let canonical = tmp.path().join("profile-creds.json");

        build_runtime_dir(&runtime, &claude_home, &profile, &canonical, LinkMode::Fake)
            .expect("build");

        // The ~/.claude/.credentials.json entry was skipped; runtime has none.
        let runtime_creds = runtime.join(".credentials.json");
        assert!(
            !runtime_creds.exists(),
            ".credentials.json from ~/.claude/ must not be copied into runtime"
        );
    });
}

#[test]
fn build_runtime_dir_fake_preserves_live_runtime_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = HOME_MUTEX.lock().expect("home mutex");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("profile-creds.json");
        let runtime_creds = runtime.join(".credentials.json");
        fs::write(&canonical, CREDS_V1).expect("write canonical");
        fs::write(&runtime_creds, CREDS_V2).expect("write runtime credentials");
        let past = SystemTime::now() - Duration::from_secs(60);
        let now = SystemTime::now();
        set_mtime(&canonical, past);
        set_mtime(&runtime_creds, now);

        build_runtime_dir(&runtime, &claude_home, &profile, &canonical, LinkMode::Fake)
            .expect("build");

        assert_eq!(fs::read(&canonical).expect("read canonical"), CREDS_V2);
        assert_eq!(fs::read(&runtime_creds).expect("read runtime"), CREDS_V2);
    });
}

#[cfg(unix)]
#[test]
fn build_runtime_dir_real_preserves_live_runtime_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = HOME_MUTEX.lock().expect("home mutex");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("profile-creds.json");
        let runtime_creds = runtime.join(".credentials.json");
        fs::write(&canonical, CREDS_V1).expect("write canonical");
        fs::write(&runtime_creds, CREDS_V2).expect("write runtime credentials");

        build_runtime_dir(&runtime, &claude_home, &profile, &canonical, LinkMode::Real)
            .expect("build");

        assert_eq!(fs::read(&canonical).expect("read canonical"), CREDS_V2);
        assert!(
            runtime_creds
                .symlink_metadata()
                .expect("runtime credentials meta")
                .file_type()
                .is_symlink()
        );
    });
}

#[cfg(unix)]
#[test]
fn build_runtime_dir_real_keeps_invalid_runtime_credentials_for_retry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = HOME_MUTEX.lock().expect("home mutex");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("profile-creds.json");
        let runtime_creds = runtime.join(".credentials.json");
        fs::write(&canonical, CREDS_V1).expect("write canonical");
        fs::write(&runtime_creds, b"partial write").expect("write runtime credentials");

        build_runtime_dir(&runtime, &claude_home, &profile, &canonical, LinkMode::Real)
            .expect("build");

        assert_eq!(fs::read(&canonical).expect("read canonical"), CREDS_V1);
        assert_eq!(
            fs::read(&runtime_creds).expect("read runtime"),
            b"partial write"
        );
    });
}

#[test]
fn build_runtime_dir_other_entries_materialized() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = HOME_MUTEX.lock().expect("home mutex");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        // A few ordinary entries that should be mirrored.
        fs::create_dir_all(claude_home.join("projects")).expect("mkdir projects");
        fs::write(claude_home.join("history.jsonl"), b"{}").expect("write history");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(&runtime, &claude_home, &profile, &canonical, LinkMode::Fake)
            .expect("build");

        // Fake mode: entries are copied, not symlinked.
        assert!(runtime.join("projects").is_dir(), "projects dir copied");
        assert!(
            runtime.join("history.jsonl").exists(),
            "history.jsonl copied"
        );
    });
}

#[cfg(unix)]
#[test]
fn build_runtime_dir_other_entries_symlinked_on_unix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = HOME_MUTEX.lock().expect("home mutex");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(claude_home.join("todos.json"), b"[]").expect("write todos");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(&runtime, &claude_home, &profile, &canonical, LinkMode::Real)
            .expect("build");

        let dst = runtime.join("todos.json");
        assert!(
            dst.symlink_metadata()
                .expect("todos present")
                .file_type()
                .is_symlink(),
            "todos.json should be a symlink in Real mode"
        );
    });
}

#[test]
fn build_runtime_dir_links_claude_json_from_parent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = HOME_MUTEX.lock().expect("home mutex");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        // ~/.claude.json sits next to ~/.claude/
        fs::write(tmp.path().join(".claude.json"), br#"{"userId":"u1"}"#)
            .expect("write .claude.json");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("test");
        let canonical = tmp.path().join("creds.json");

        build_runtime_dir(&runtime, &claude_home, &profile, &canonical, LinkMode::Fake)
            .expect("build");

        let dst = runtime.join(".claude.json");
        assert!(dst.exists(), ".claude.json must appear in runtime");
        assert_eq!(
            fs::read(&dst).expect("read"),
            br#"{"userId":"u1"}"#,
            "content must match source"
        );
    });
}

// ── has_live_session ──────────────────────────────────────────────────────────

#[test]
fn has_live_session_false_when_no_sessions_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = HOME_MUTEX.lock().expect("home mutex");
    with_fake_home(tmp.path(), || {
        // No sessions dir created — must return false, not error.
        assert!(!has_live_session("ghost"));
    });
}

#[test]
fn has_live_session_false_when_sessions_dir_empty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = HOME_MUTEX.lock().expect("home mutex");
    with_fake_home(tmp.path(), || {
        // Create an empty sessions dir.
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("empty")
            .join("sessions");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        assert!(!has_live_session("empty"));
    });
}

#[test]
fn has_live_session_false_when_all_sessions_dead() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = HOME_MUTEX.lock().expect("home mutex");
    with_fake_home(tmp.path(), || {
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("dead")
            .join("sessions");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        // Unlocked file = dead session.
        fs::write(sessions.join("99999"), b"").expect("write dead pid");
        assert!(!has_live_session("dead"));
    });
}

#[test]
fn has_live_session_true_when_any_session_alive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = HOME_MUTEX.lock().expect("home mutex");
    with_fake_home(tmp.path(), || {
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("alive")
            .join("sessions");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        let pid_path = sessions.join("12345");
        let file = open_pid_file(&pid_path).expect("open pid");
        file.lock().expect("lock pid");
        assert!(has_live_session("alive"));
        drop(file);
        assert!(!has_live_session("alive"));
    });
}

#[test]
fn has_live_session_true_with_mixed_alive_and_dead() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = HOME_MUTEX.lock().expect("home mutex");
    with_fake_home(tmp.path(), || {
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("mixed")
            .join("sessions");
        fs::create_dir_all(&sessions).expect("mkdir sessions");
        // One dead entry.
        fs::write(sessions.join("11111"), b"").expect("write dead pid");
        // One live entry.
        let live_path = sessions.join("22222");
        let file = open_pid_file(&live_path).expect("open live pid");
        file.lock().expect("lock live pid");
        assert!(has_live_session("mixed"));
        drop(file);
    });
}

// ── ProfileRuntime acquire / drop lifecycle ───────────────────────────────────

#[test]
fn acquire_creates_runtime_and_pid_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = HOME_MUTEX.lock().expect("home mutex");
    with_fake_home(tmp.path(), || {
        fake_claude_home(tmp.path());
        let profile = make_profile("lifecycle");

        let rt = ProfileRuntime::acquire(&profile).expect("acquire");

        // Runtime dir exists.
        assert!(
            rt.config_dir().is_dir(),
            "runtime dir must exist after acquire"
        );

        // PID file exists under sessions/<pid>.
        let pid = std::process::id().to_string();
        let sessions = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("lifecycle")
            .join("sessions");
        assert!(sessions.join(&pid).exists(), "PID file must exist");

        // PID file is flock-held while the runtime is alive.
        assert!(
            is_session_alive(&sessions.join(&pid)),
            "PID file must be flock-held while runtime is alive"
        );

        // config_dir() points into ~/.clauth/profiles/<name>/runtime/.
        let expected_runtime = tmp
            .path()
            .join(".clauth")
            .join("profiles")
            .join("lifecycle")
            .join("runtime");
        assert_eq!(rt.config_dir(), expected_runtime);

        // settings.json written into the runtime.
        assert!(
            rt.config_dir().join("settings.json").exists(),
            "settings.json must be written"
        );

        drop(rt);

        // After drop: PID file removed, runtime torn down, sessions dir removed.
        assert!(!sessions.join(&pid).exists(), "PID file removed on drop");
        assert!(
            !expected_runtime.exists(),
            "runtime dir torn down on last-session drop"
        );
        assert!(
            !sessions.exists(),
            "sessions dir removed when no live siblings remain"
        );
    });
}

// ── Multi-session ref-count ───────────────────────────────────────────────────

/// Test that `build_runtime_dir` re-walk picks up entries added between two
/// acquires. Uses the underlying functions directly because two
/// `ProfileRuntime::acquire` calls in the same process would deadlock on the
/// PID flock (`flock(LOCK_EX)` blocks when the same process opens the same
/// path a second time via a new fd).
#[test]
fn build_runtime_dir_rewalk_picks_up_late_entries() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _guard = HOME_MUTEX.lock().expect("home mutex");
    with_fake_home(tmp.path(), || {
        let claude_home = fake_claude_home(tmp.path());
        fs::write(claude_home.join("existing.txt"), b"v1").expect("write existing");
        let runtime = tmp.path().join("runtime");
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        let profile = make_profile("rewalk");
        let canonical = tmp.path().join("creds.json");

        // First build — existing.txt is materialized.
        build_runtime_dir(&runtime, &claude_home, &profile, &canonical, LinkMode::Fake)
            .expect("first build");
        assert!(
            runtime.join("existing.txt").exists(),
            "first build: existing.txt present"
        );

        // Simulate a new file added to ~/.claude/ after the first build.
        fs::write(claude_home.join("late_entry.txt"), b"new").expect("write late entry");

        // Second build (simulating a second session's acquire) — late entry picked up.
        build_runtime_dir(&runtime, &claude_home, &profile, &canonical, LinkMode::Fake)
            .expect("second build");
        assert!(
            runtime.join("late_entry.txt").exists(),
            "second build must pick up late_entry.txt"
        );
        // Existing entry still present — re-walk is additive, not destructive.
        assert!(
            runtime.join("existing.txt").exists(),
            "second build must preserve existing.txt"
        );
    });
}

/// Test that `prune_stale_sessions` correctly counts live vs dead sessions
/// so a second live session prevents teardown. Uses direct calls to
/// `open_pid_file` / `prune_stale_sessions` rather than two `ProfileRuntime`
/// acquires (which would deadlock via `flock` on the shared PID path).
#[test]
fn prune_with_two_live_sessions_returns_two() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let sessions = tmp.path().join("sessions");
    fs::create_dir_all(&sessions).expect("mkdir sessions");

    let pid1 = sessions.join("100001");
    let pid2 = sessions.join("100002");
    let f1 = open_pid_file(&pid1).expect("open pid1");
    f1.lock().expect("lock pid1");
    let f2 = open_pid_file(&pid2).expect("open pid2");
    f2.lock().expect("lock pid2");

    let count = prune_stale_sessions(&sessions).expect("prune");
    assert_eq!(count, 2, "both live sessions must be counted");

    drop(f2);
    let count = prune_stale_sessions(&sessions).expect("prune after drop f2");
    assert_eq!(count, 1, "one live session after f2 dropped");
    assert!(!pid2.exists(), "dead session file removed");

    drop(f1);
    let count = prune_stale_sessions(&sessions).expect("prune after drop f1");
    assert_eq!(count, 0, "no live sessions after both dropped");
    assert!(!pid1.exists(), "dead session file removed");
}

// ── sync_credentials_unlocked concurrent contention (Unix) ───────────────────
//
// Two threads call sync_credentials_unlocked on the same link_path
// simultaneously (barrier-synchronized). Both share the same PID-suffixed tmp
// name. The requirement: regardless of which thread wins the rename race, the
// final state is consistent — link_path is a valid symlink and canonical holds
// the expected bytes. Neither thread should leave a dangling tmp or corrupt the
// canonical file.

#[cfg(unix)]
#[test]
fn sync_credentials_unlocked_concurrent_same_link_consistent_end_state() {
    use std::sync::{Arc, Barrier};

    let tmp = tempfile::tempdir().expect("tempdir");
    let canonical = Arc::new(tmp.path().join("canonical.json"));
    let link_path = Arc::new(tmp.path().join(".credentials.json"));

    fs::write(link_path.as_ref(), CREDS_V1).expect("write link");

    let barrier = Arc::new(Barrier::new(2));

    let b1 = barrier.clone();
    let ca1 = canonical.clone();
    let lp1 = link_path.clone();
    let t1 = std::thread::spawn(move || {
        b1.wait();
        sync_credentials_unlocked(&lp1, &ca1)
    });

    let b2 = barrier.clone();
    let ca2 = canonical.clone();
    let lp2 = link_path.clone();
    let t2 = std::thread::spawn(move || {
        b2.wait();
        sync_credentials_unlocked(&lp2, &ca2)
    });

    // One or both may error (same-PID tmp collision); what matters is end state.
    let _ = t1.join().expect("thread 1 panicked");
    let _ = t2.join().expect("thread 2 panicked");

    // link_path must be a symlink — not a dangling regular file — after both
    // threads complete. The rename is atomic on POSIX so at least one wins.
    assert!(
        link_path
            .symlink_metadata()
            .expect("link_path must exist")
            .file_type()
            .is_symlink(),
        "link_path must be a symlink after concurrent sync"
    );

    // canonical must hold the content that was in link_path.
    assert_eq!(
        fs::read(canonical.as_ref()).expect("read canonical"),
        CREDS_V1,
        "canonical must hold link content"
    );

    // The PID-suffixed tmp must be cleaned up — no leftover temp file.
    let tmp_name =
        link_path.with_file_name(format!(".credentials.json.tmp.{}", std::process::id()));
    assert!(
        !tmp_name.exists(),
        "PID-suffixed tmp must not persist after sync completes"
    );
}
