//! `clauth __complete --resumable` against the real binary: the words a shell
//! completion script captures after `resume` and `info`. Spawning is the only
//! way to see that the hidden flag reaches the session-id printer.
//!
//! Unix only, for the reason `tests/closed_reader.rs` gives: the child resolves
//! its home through `$HOME`, which only Unix lets a test point at a sandbox.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn run_complete(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clauth"))
        .arg("__complete")
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("CLAUTH_NO_UPDATE", "1")
        .env("CLAUTH_NO_COMPLETIONS", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .expect("run clauth __complete")
}

/// `--resumable` prints `latest`, then the store's top-level session ids, one
/// per line, and exits 0; a nested subagent transcript is not printed.
#[test]
fn resumable_prints_latest_then_the_top_level_session_ids() {
    let home = tempfile::tempdir().unwrap();
    let project = home
        .path()
        .join(".claude")
        .join("projects")
        .join("-home-u-a");
    let id = "0b6f2c1e-1111-4a4a-8a8a-000000000001";
    let nested = project.join(id).join("subagents");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(project.join(format!("{id}.jsonl")), "{}\n").unwrap();
    std::fs::write(nested.join("agent-a1b2c3d4e5f6.jsonl"), "{}\n").unwrap();

    let out = run_complete(home.path(), &["--resumable"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("latest\n{id}\n")
    );
}
