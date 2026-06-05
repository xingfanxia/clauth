use super::*;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::time::Duration;

use serde_json::json;

fn set_mtime(path: &Path, when: SystemTime) {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for mtime");
    file.set_modified(when).expect("set_modified");
}

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).expect("serialize")).expect("write");
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("read")).expect("parse")
}

/// Deterministic timestamps so mtime ordering is unambiguous in tests.
fn t(offset: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + offset)
}

#[test]
fn shared_fields_propagate_from_newest_to_others() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    write_json(
        &a,
        &json!({"numStartups": 2, "mcpServers": {"x": 1}, "oauthAccount": {"emailAddress": "a@x"}}),
    );
    write_json(
        &b,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "b@x"}}),
    );
    set_mtime(&a, t(10));
    set_mtime(&b, t(5));

    sync_paths(&[a.clone(), b.clone()]).expect("sync");

    let bj = read_json(&b);
    assert_eq!(bj["numStartups"], json!(2));
    assert_eq!(bj["mcpServers"], json!({"x": 1}));
    assert_eq!(bj["oauthAccount"]["emailAddress"], json!("b@x")); // per-profile identity kept
    assert_eq!(read_json(&a)["oauthAccount"]["emailAddress"], json!("a@x")); // winner not rewritten
}

#[test]
fn per_profile_fields_never_propagate() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    write_json(
        &a,
        &json!({
            "shared": 1,
            "oauthAccount": {"emailAddress": "a@x"},
            "passesLastSeenRemaining": 99,
            "overageCreditGrantCache": {"a": true}
        }),
    );
    write_json(
        &b,
        &json!({
            "shared": 0,
            "oauthAccount": {"emailAddress": "b@x"},
            "passesLastSeenRemaining": 0,
            "overageCreditGrantCache": {"b": true}
        }),
    );
    set_mtime(&a, t(10));
    set_mtime(&b, t(5));

    sync_paths(&[a, b.clone()]).expect("sync");

    let bj = read_json(&b);
    assert_eq!(bj["shared"], json!(1));
    assert_eq!(bj["oauthAccount"]["emailAddress"], json!("b@x"));
    assert_eq!(bj["passesLastSeenRemaining"], json!(0));
    assert_eq!(bj["overageCreditGrantCache"], json!({"b": true}));
}

#[test]
fn shared_key_absent_in_winner_is_removed_from_target() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    write_json(&a, &json!({"numStartups": 2}));
    write_json(
        &b,
        &json!({"numStartups": 1, "staleFeature": true, "oauthAccount": {"e": "b"}}),
    );
    set_mtime(&a, t(10));
    set_mtime(&b, t(5));

    sync_paths(&[a, b.clone()]).expect("sync");

    let bj = read_json(&b);
    assert_eq!(bj["numStartups"], json!(2));
    assert!(
        bj.get("staleFeature").is_none(),
        "a shared key the winner dropped must be removed from the target"
    );
    assert_eq!(bj["oauthAccount"]["e"], json!("b"));
}

#[test]
fn unparseable_file_is_skipped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    let c = tmp.path().join("c.json");
    write_json(&a, &json!({"numStartups": 2, "oauthAccount": {"e": "a"}}));
    fs::write(&b, b"{ partial truncated write").expect("write garbage");
    write_json(&c, &json!({"numStartups": 1, "oauthAccount": {"e": "c"}}));
    set_mtime(&a, t(10));
    set_mtime(&b, t(20)); // newest by mtime but unparseable → skipped
    set_mtime(&c, t(5));

    let before_b = fs::read(&b).expect("read b");
    sync_paths(&[a, b.clone(), c.clone()]).expect("sync");

    // mid-write file never read from nor written to
    assert_eq!(fs::read(&b).expect("read b"), before_b);
    // `a` is the newest parseable member → `c` takes its shared field
    let cj = read_json(&c);
    assert_eq!(cj["numStartups"], json!(2));
    assert_eq!(cj["oauthAccount"]["e"], json!("c"));
}

#[test]
fn newest_mtime_wins_regardless_of_argument_order() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    write_json(&a, &json!({"v": "old"}));
    write_json(&b, &json!({"v": "new"}));
    set_mtime(&a, t(5));
    set_mtime(&b, t(10)); // b newer even though `a` is listed first

    sync_paths(&[a.clone(), b.clone()]).expect("sync");

    assert_eq!(read_json(&a)["v"], json!("new"));
    assert_eq!(read_json(&b)["v"], json!("new"));
}

#[test]
fn converged_target_is_not_rewritten() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    write_json(&a, &json!({"numStartups": 5, "oauthAccount": {"e": "a"}}));
    write_json(&b, &json!({"numStartups": 5, "oauthAccount": {"e": "b"}}));
    set_mtime(&a, t(10));
    set_mtime(&b, t(5));
    let before = fs::metadata(&b).unwrap().modified().unwrap();

    sync_paths(&[a, b.clone()]).expect("sync");

    assert_eq!(
        before,
        fs::metadata(&b).unwrap().modified().unwrap(),
        "a target already converged on shared fields must not be rewritten"
    );
}

#[test]
fn single_file_is_noop() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    write_json(&a, &json!({"numStartups": 1}));
    let before = fs::read(&a).expect("read");
    sync_paths(std::slice::from_ref(&a)).expect("sync");
    assert_eq!(fs::read(&a).expect("read"), before);
}
