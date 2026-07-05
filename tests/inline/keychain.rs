//! KC-1 — Keychain read/write/delete round-trip. Uses a **throwaway service name**
//! unique to this process; it never touches the real `Claude Code-credentials`
//! item. It is the ONLY test that drives the real macOS Keychain (via the
//! `/usr/bin/security` CLI the shipped write/delete path uses), so it is
//! `#[ignore]`d: it still mutates the login Keychain (creates + deletes a
//! throwaway item) as a side effect. Run it on demand instead:
//!     cargo test keychain_round_trip -- --ignored
//! All other credential/divergence tests stay on the file model
//! (`keychain::enabled()` is false under `cfg(test)`), so `cargo test` never
//! touches the Keychain.

use super::{delete_at, read_at, run_with_deadline, write_at};
use crate::profile::{ClaudeCredentials, OAuthToken};

fn sample_creds(access: &str, refresh: &str) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(refresh.to_string()),
            expires_at: Some(1_900_000_000_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: Some("max".to_string()),
        }),
    }
}

#[test]
#[ignore = "touches the real login Keychain (throwaway service); macOS re-prompts each rebuild — run explicitly with --ignored"]
fn keychain_round_trip_on_temp_service() {
    let service = format!("clauth-test-{}", std::process::id());
    let account = "clauth-test-account";

    // Clean slate — delete is idempotent, read of an absent item is None.
    delete_at(&service, account).expect("pre-clean delete is idempotent");
    assert!(
        read_at(&service, account).expect("read absent").is_none(),
        "temp service should start empty"
    );

    // Write, then read back the same tokens.
    let creds = sample_creds("sk-ant-oat01-TESTACCESS", "sk-ant-ort01-TESTREFRESH");
    write_at(&service, account, &creds).expect("write");
    let oauth = read_at(&service, account)
        .expect("read present")
        .expect("some")
        .claude_ai_oauth
        .expect("oauth block round-trips");
    assert_eq!(oauth.access_token, "sk-ant-oat01-TESTACCESS");
    assert_eq!(
        oauth.refresh_token.as_deref(),
        Some("sk-ant-ort01-TESTREFRESH")
    );
    assert_eq!(oauth.subscription_type.as_deref(), Some("max"));

    // add-generic-password -U is add-or-update: a second write replaces in place.
    let updated = sample_creds("sk-ant-oat01-ROTATED", "sk-ant-ort01-ROTATED");
    write_at(&service, account, &updated).expect("update");
    let rotated = read_at(&service, account)
        .expect("read")
        .expect("some")
        .claude_ai_oauth
        .expect("oauth");
    assert_eq!(rotated.access_token, "sk-ant-oat01-ROTATED");

    // Delete → absent; delete again is still Ok (idempotent).
    delete_at(&service, account).expect("delete");
    assert!(
        read_at(&service, account)
            .expect("read after delete")
            .is_none()
    );
    delete_at(&service, account).expect("second delete idempotent");
}

// ── TECH-3: `security` subprocess deadline (no Keychain touched) ───────────────
//
// Exercise `run_with_deadline` with benign stand-in commands (`sleep` / `true`)
// so the timeout-and-kill path is proven without a real `/usr/bin/security`
// invocation — these run in `cargo test` (unlike the #[ignore]d round-trip).

#[test]
fn keychain_timeout_kills_a_hung_command() {
    use std::process::Command;
    use std::time::{Duration, Instant};

    let mut cmd = Command::new("/bin/sleep");
    cmd.arg("30");
    let start = Instant::now();
    let result = run_with_deadline(cmd, Duration::from_millis(300));
    let elapsed = start.elapsed();

    assert!(
        result.is_err(),
        "a command outrunning its deadline must return an error"
    );
    assert!(
        result.unwrap_err().to_string().contains("deadline"),
        "the error should name the deadline"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the child must be killed near the deadline (was {elapsed:?}), not left to run 30s"
    );
}

#[test]
fn keychain_deadline_returns_output_for_a_fast_command() {
    use std::process::Command;
    use std::time::Duration;

    let cmd = Command::new("/usr/bin/true");
    let out = run_with_deadline(cmd, Duration::from_secs(5)).expect("fast command succeeds");
    assert!(out.status.success(), "`true` exits 0 within the deadline");
}
