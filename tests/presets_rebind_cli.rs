//! `clauth presets rebind --yes` against the real binary: the partial-failure
//! stdout, stderr and exit code under the keep-going rule. Spawning is the only
//! way to see what a caller captures.
//!
//! Unix only, for the reason `tests/oauth_token_cli.rs` gives: the child
//! resolves its home through `$HOME`, which only Unix lets a test point at a
//! sandbox.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Register `name` as an empty DeepSeek account: profile dir + `config.toml`.
fn seed_empty(home: &Path, name: &str) {
    let dir = home.join(".clauth").join("profiles").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "base_url = \"https://api.deepseek.com/anthropic\"\n",
    )
    .unwrap();
}

fn run_rebind(home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clauth"))
        .args(["presets", "rebind", "--yes"])
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("CLAUTH_NO_UPDATE", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .expect("run clauth presets rebind --yes")
}

/// The active `D0` saves its rows then fails re-applying a live `settings.json`
/// that is a directory; `D2` (later) still moves. stdout carries the moved
/// line, stderr one failure line naming `D0`, exit 1.
#[test]
fn a_partial_failure_keeps_going_and_reports_stdout_stderr_and_exit_1() {
    let home = tempfile::tempdir().unwrap();
    seed_empty(home.path(), "D0");
    seed_empty(home.path(), "D2");
    std::fs::write(
        home.path().join(".clauth").join("profiles.toml"),
        "active_profile = \"D0\"\nprofiles = [\"D0\", \"D2\"]\n",
    )
    .unwrap();
    let settings = home.path().join(".claude").join("settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::create_dir(&settings).unwrap();

    let out = run_rebind(home.path());
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "moved 10 rows on 2 accounts onto the built-in preset models.\n",
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "re-bind failed for 'D0': failed to read {}: Is a directory (os error 21)\n",
            settings.display()
        ),
    );
    // D2's rows landed after the failure: parse its config.toml and compare the
    // `[models]` table to the literal DeepSeek binds.
    #[derive(Default, serde::Deserialize)]
    struct Models {
        #[serde(default)]
        default: Option<String>,
        #[serde(default)]
        opus: Option<String>,
        #[serde(default)]
        sonnet: Option<String>,
        #[serde(default)]
        haiku: Option<String>,
        #[serde(default)]
        fable: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Config {
        #[serde(default)]
        models: Models,
    }
    let d2: Config = toml::from_str(
        &std::fs::read_to_string(
            home.path()
                .join(".clauth")
                .join("profiles")
                .join("D2")
                .join("config.toml"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (
            d2.models.default.as_deref(),
            d2.models.opus.as_deref(),
            d2.models.sonnet.as_deref(),
            d2.models.haiku.as_deref(),
            d2.models.fable.as_deref(),
        ),
        (
            Some("opus"),
            Some("deepseek-v4-pro[1m]"),
            Some("deepseek-flash[1m]"),
            Some("deepseek-flash[1m]"),
            Some("deepseek-v4-pro[1m]"),
        ),
        "D2's [models] rows equal the DeepSeek binds",
    );
}
