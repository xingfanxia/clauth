use super::*;
use std::fs;

#[test]
fn sync_returns_false_when_tempdir_creds_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let live = tmp.path().join(".credentials.json");
    let target = tmp.path().join("profile.json");
    assert!(!sync_relogged_credentials(&live, &target));
    assert!(!target.exists());
}

#[cfg(unix)]
#[test]
fn sync_returns_false_when_tempdir_creds_still_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let live = tmp.path().join(".credentials.json");
    let target = tmp.path().join("profile.json");
    fs::write(&target, b"stored").expect("write target");
    std::os::unix::fs::symlink(&target, &live).expect("symlink");
    assert!(!sync_relogged_credentials(&live, &target));
    assert_eq!(fs::read(&target).expect("read"), b"stored");
}

#[test]
fn sync_returns_false_when_content_matches_target() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let live = tmp.path().join(".credentials.json");
    let target = tmp.path().join("profile.json");
    fs::write(&live, b"same").expect("write live");
    fs::write(&target, b"same").expect("write target");
    assert!(!sync_relogged_credentials(&live, &target));
    assert_eq!(fs::read(&target).expect("read"), b"same");
}

#[test]
fn sync_writes_target_when_content_differs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let live = tmp.path().join(".credentials.json");
    let target = tmp.path().join("profile.json");
    fs::write(&live, b"relogged").expect("write live");
    fs::write(&target, b"stale").expect("write target");
    assert!(sync_relogged_credentials(&live, &target));
    assert_eq!(fs::read(&target).expect("read"), b"relogged");
}

#[test]
fn sync_creates_target_when_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let live = tmp.path().join(".credentials.json");
    let target = tmp
        .path()
        .join("profiles")
        .join("foo")
        .join("credentials.json");
    fs::write(&live, b"new").expect("write live");
    assert!(sync_relogged_credentials(&live, &target));
    assert_eq!(fs::read(&target).expect("read"), b"new");
}
