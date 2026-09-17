//! `clauth doctor` pure-core tests — the check-formatting + classification logic
//! (freshness, version/schema skew, exit-code aggregation, the RFC3339 parse, and
//! line rendering). The impure probes (launchctl / codesign / security / socket)
//! are operator-run and not exercised here — no real Keychain is ever touched.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use super::*;

#[test]
fn freshness_tracks_the_1s_write_cadence_not_the_refresh_interval() {
    // status.json is rewritten every 1s tick; a healthy file is seconds-fresh
    // regardless of the (unrelated) usage refresh interval. Bands are anchored to
    // that cadence, bounded by the ~60s watchdog.
    assert_eq!(freshness(Duration::from_secs(3)), Status::Pass);
    assert_eq!(freshness(Duration::from_secs(10)), Status::Pass); // ==10s boundary
    assert_eq!(freshness(Duration::from_secs(40)), Status::Warn); // >10s, <=75s
    assert_eq!(freshness(Duration::from_secs(75)), Status::Warn); // ==75s boundary
    assert_eq!(freshness(Duration::from_secs(120)), Status::Fail); // past the watchdog → dead
}

#[test]
fn exit_code_is_nonzero_only_when_something_failed() {
    assert_eq!(
        exit_code(&[Check::pass("a", "ok"), Check::pass("b", "ok")]),
        0
    );
    // A WARN alone never fails the run.
    assert_eq!(exit_code(&[Check::warn("a", "meh", "do x")]), 0);
    assert_eq!(
        exit_code(&[Check::pass("a", "ok"), Check::fail("b", "broken", "fix it")]),
        1
    );
}

#[test]
fn skew_classifies_version_and_schema_mismatches() {
    // Exact match → Pass.
    let (s, _) = skew("0.7.1", 1, Some("0.7.1"), Some(1));
    assert_eq!(s, Status::Pass);
    // Same schema, different version → Warn (a restart adopts the new binary).
    let (s, d) = skew("0.7.1", 1, Some("0.7.0"), Some(1));
    assert_eq!(s, Status::Warn);
    assert!(d.contains("0.7.0") && d.contains("0.7.1"));
    // Schema mismatch → Fail (the read format diverged).
    let (s, _) = skew("0.7.1", 1, Some("0.7.1"), Some(2));
    assert_eq!(s, Status::Fail);
    // No fields in status.json → Warn (old/absent daemon).
    let (s, _) = skew("0.7.1", 1, None, None);
    assert_eq!(s, Status::Warn);
}

#[test]
fn iso_to_ms_round_trips_the_daemon_writer_format() {
    // Couple the reader to the ACTUAL writer (`+00:00`, not `Z`) so a writer
    // format change can't silently break this parse and blank the freshness check.
    use crate::usage::epoch_secs_to_iso;
    assert_eq!(iso_to_ms(&epoch_secs_to_iso(0)), Some(0));
    assert_eq!(
        iso_to_ms(&epoch_secs_to_iso(1_609_459_200)),
        Some(1_609_459_200_000)
    );
    // Also tolerant of the `Z` / fractional shapes (only chars 0..19 are read).
    assert_eq!(iso_to_ms("2021-01-01T00:00:00Z"), Some(1_609_459_200_000));
    assert_eq!(
        iso_to_ms("2021-01-01T00:00:00.123Z"),
        Some(1_609_459_200_000)
    );
    // Too short → None (caller falls back to file mtime).
    assert_eq!(iso_to_ms("2021-01-01"), None);
}

#[test]
fn render_shows_a_fix_only_when_not_passing() {
    let pass = Check::pass("socket", "responds").render();
    assert!(pass.starts_with("[PASS] socket — responds"));
    assert!(!pass.contains("fix:"));

    let fail = Check::fail("socket", "no reply", "restart the daemon").render();
    assert!(fail.starts_with("[FAIL] socket — no reply"));
    assert!(fail.contains("fix: restart the daemon"));
}

// ---- CDX-1 T9: check_codex (sandboxed — no real ~/.codex, WARN-only) ----
//
// Since the harness split, the codex roster IS `codex-profiles.toml`
// (`CodexState`): a name in `profiles.toml` is a claude profile and can never
// produce a codex line, so every fixture here seeds the codex file itself.
//
// `check_codex` answers two questions on that roster — a chain the server
// declared dead (the quarantine record beside the store, the codex twin of
// `AppState::auth_broken`) and a chain whose standby keep-alive stopped
// landing — plus the silence of a claude-only install. The fork engine's
// live-slot lines are gone with the fork engine itself: the operator's
// `~/.codex/auth.json` is a symlink the CAPTURE installs
// (`adopt_operator_auth_slot`), a switch only moves the active marker in the
// codex state file, and sessions bind their own `CODEX_HOME` — so "the live
// login disagrees with the active codex profile" is no longer a state this
// engine can be in.

mod codex_check {
    use crate::codex_profiles::CodexState;
    use crate::doctor::check_codex;
    use crate::doctor::core::Status;
    use crate::profile::{AppState, save_app_state, save_profile};
    use crate::testutil::{HomeSandbox, blank_profile, write_codex_store};

    /// One codex chain as codex's own writer leaves it. `last_refresh` is the
    /// stamp a landed rotation re-writes — the signal the keep-alive check
    /// reads, so a fixture that omits it is a chain with nothing to judge.
    fn auth_body(access: &str, refresh: &str, last_refresh: Option<&str>) -> String {
        let mut body = serde_json::json!({
            "tokens": {
                "access_token": access,
                "refresh_token": refresh,
                "account_id": "acct-a",
            },
        });
        if let Some(stamp) = last_refresh {
            body["last_refresh"] = serde_json::Value::String(stamp.to_string());
        }
        body.to_string()
    }

    /// Persist one codex profile: the roster entry in `codex-profiles.toml`
    /// (the file IS the harness axis) plus its chain in the profile store at
    /// `profiles/<name>/auth.json`, which is where `read_store_auth` looks.
    fn seed_codex_profile(name: &str, refresh: &str, last_refresh: Option<&str>) {
        CodexState::update(|state| {
            state.add_profile(name);
            state.set_active(Some(name));
            Ok(())
        })
        .expect("persist the codex roster");
        write_codex_store(name, &auth_body("at-a", refresh, last_refresh));
    }

    // CDX-3 R6: a quarantined codex profile outranks every other line — the
    // chain is dead and only a fresh login fixes it. The verdict is the
    // quarantine record bound to the token it judged, so it speaks only while
    // the store still holds that refresh token.
    #[test]
    fn warns_on_a_quarantined_codex_profile() {
        let _home = HomeSandbox::new();
        seed_codex_profile("cdx-dead", "rt-dead", None);
        crate::codex_auth::quarantine_for_test("cdx-dead", "reused", "rt-dead");
        let check = check_codex().expect("codex line");
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("rejected"), "{}", check.detail);
        assert!(check.detail.contains("cdx-dead"), "{}", check.detail);
    }

    // CDX-3 R6: a stored chain whose last_refresh is far past the keep-alive
    // line means the standby refresh isn't landing — surface it.
    #[test]
    fn warns_when_standby_keep_alive_is_not_landing() {
        let _home = HomeSandbox::new();
        let old = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs() - 20 * 86_400);
        seed_codex_profile("cdx-stale", "rt-s", Some(&old));
        let check = check_codex().expect("codex line");
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("last refreshed"), "{}", check.detail);
    }

    // CDX-5: the proxy check is silent until a heartbeat exists, WARNs when
    // stale, and PASSes when fresh (+notes the config-pointed state).
    #[test]
    fn codex_proxy_check_tracks_the_heartbeat() {
        use crate::doctor::check_codex_proxy;
        let _home = HomeSandbox::new();
        assert!(check_codex_proxy().is_none(), "no heartbeat → no line");

        // Fresh heartbeat, config NOT pointed → PASS with the nudge.
        crate::proxy::touch_heartbeat_for_test(4517);
        let check = check_codex_proxy().expect("line");
        assert_eq!(check.status, Status::Pass);
        assert!(check.detail.contains("NOT pointed"), "{}", check.detail);

        // Point the config → PASS clean.
        let codex_dir = crate::actions::default_codex_operator_home().unwrap();
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            "model_provider = \"clauth\"\n",
        )
        .unwrap();
        let check = check_codex_proxy().expect("line");
        assert_eq!(check.status, Status::Pass);
        assert!(check.detail.contains("points at it"), "{}", check.detail);
    }

    // A claude-only install gets NO codex line at all — and `profiles.toml` is
    // where that install's accounts live, so a claude roster can never make
    // one appear.
    #[test]
    fn silent_when_no_codex_profile_exists() {
        let _home = HomeSandbox::new();
        save_profile(&blank_profile(&crate::profile::ProfileName::from("work"))).unwrap();
        save_app_state(&AppState {
            profiles: vec!["work".into()],
            ..AppState::default()
        })
        .unwrap();
        assert!(
            check_codex().is_none(),
            "a claude roster is not a codex roster"
        );
    }

    // The PASS line is about the CHAINS, not about any live login: a roster
    // whose members are neither quarantined nor past the keep-alive line
    // passes, and the line counts the roster it judged.
    #[test]
    fn passes_when_every_stored_chain_is_healthy() {
        let _home = HomeSandbox::new();
        seed_codex_profile("cdx-a", "rt-a", None);
        let check = check_codex().expect("codex line");
        assert_eq!(check.status, Status::Pass, "{}", check.render());
        assert!(check.detail.contains("chains healthy"), "{}", check.detail);
    }
}
