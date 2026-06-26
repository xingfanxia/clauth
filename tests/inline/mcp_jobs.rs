#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Disk job-store coverage: atomic write/read roundtrip, id safety, eviction,
//! and GC of expired / orphaned / oversized state. Home-sandboxed so files land
//! in a tempdir, never the real `~/.clauth/jobs`.

use super::*;
use crate::testutil::HomeSandbox;

#[test]
fn write_read_roundtrip_running_then_done() {
    let _home = HomeSandbox::new();
    let id = new_job_id(1000);
    write_running(&id, "work", 1000, true).unwrap();

    let r = read(&id).expect("running record");
    assert_eq!(r.state, JobState::Running);
    assert_eq!(r.profile, "work");
    assert!(r.monitor, "monitor flag round-trips");
    assert!(r.envelope.is_none());

    let env = serde_json::json!({ "is_error": false, "result": "ok" });
    write_done(&id, "work", 1000, env.clone()).unwrap();
    let r = read(&id).expect("done record");
    assert_eq!(r.state, JobState::Done);
    assert_eq!(r.envelope, Some(env));

    // an atomic write leaves no .tmp behind.
    let tmp_left = std::fs::read_dir(jobs_dir().unwrap())
        .unwrap()
        .flatten()
        .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("tmp"));
    assert!(!tmp_left, "atomic write leaves no .tmp");

    remove(&id);
    assert!(read(&id).is_none(), "removed job is gone");
}

#[test]
fn unknown_job_reads_none() {
    let _home = HomeSandbox::new();
    assert!(read("d-1-999").is_none());
}

#[test]
fn job_id_safety_rejects_traversal_and_separators() {
    assert!(is_safe_job_id("d-123-4"));
    assert!(is_safe_job_id("abc_DEF-9"));
    assert!(!is_safe_job_id(""));
    assert!(!is_safe_job_id("../escape"));
    assert!(!is_safe_job_id("a/b"));
    assert!(!is_safe_job_id("a.json"));
    assert!(!is_safe_job_id(&"x".repeat(200)));
}

#[test]
fn new_job_id_is_unique_and_safe() {
    let a = new_job_id(5);
    let b = new_job_id(5);
    assert_ne!(a, b, "same-ms ids differ via the counter");
    assert!(is_safe_job_id(&a) && is_safe_job_id(&b));
}

#[test]
fn gc_reaps_expired_running_and_done_keeps_fresh() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64; // far-future ms so "fresh" entries read as recent

    write_done("d-fresh-done", "p", now, serde_json::json!({ "ok": true })).unwrap();
    write_running("d-fresh-run", "p", now, false).unwrap();
    write_done(
        "d-old-done",
        "p",
        now - DONE_TTL_MS - 1,
        serde_json::json!({}),
    )
    .unwrap();
    write_running("d-old-run", "p", now - RUNNING_TTL_MS - 1, false).unwrap();

    gc(now);

    assert!(read("d-fresh-done").is_some());
    assert!(read("d-fresh-run").is_some());
    assert!(read("d-old-done").is_none(), "expired done reaped");
    assert!(read("d-old-run").is_none(), "orphaned running reaped");
}

#[test]
fn gc_sweeps_stray_tmp_files() {
    let _home = HomeSandbox::new();
    let dir = jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("d-1-0.json.tmp"), b"partial").unwrap();
    gc(0);
    assert!(!dir.join("d-1-0.json.tmp").exists(), "stray tmp swept");
}

#[test]
fn gc_caps_retained_to_newest() {
    let _home = HomeSandbox::new();
    let now = 10_000_000_000u64;
    let total = MAX_RETAINED + 5;
    for i in 0..total {
        // started_at rises with i, so low i are the oldest.
        let started = now - (total as u64 - i as u64);
        write_done(&format!("d-cap-{i}"), "p", started, serde_json::json!({})).unwrap();
    }
    gc(now);

    let remaining = std::fs::read_dir(jobs_dir().unwrap())
        .unwrap()
        .flatten()
        .count();
    assert_eq!(remaining, MAX_RETAINED, "capped to MAX_RETAINED newest");
    assert!(read("d-cap-0").is_none(), "oldest reaped");
    assert!(
        read(&format!("d-cap-{}", total - 1)).is_some(),
        "newest kept"
    );
}
