#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `switch_profile_noninteractive` divergence-branch coverage for the MCP
//! `switch` tool. The four divergence outcomes are exercised at the home-derived
//! seam, no TTY: `Some(NewProfile)`/`None` must error, `Some(Overwrite)` captures
//! the diverged live login into the OUTGOING profile (the
//! `switch_profile_reconciled` path), and `Some(Discard)` force-relinks the
//! target's stored creds WITHOUT capturing the live login into any profile (the
//! `switch_profile_discard` path).

#![cfg(unix)]

use super::*;

use crate::claude::{LinkState, classify_credentials_link};
use crate::profile::{
    AppConfig, AppState, ClaudeCredentials, DivergenceChoice, OAuthToken, Profile, profile_dir,
    read_json_file, save_profile,
};
use crate::testutil::HomeSandbox;

fn creds(access: &str, refresh: &str) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(refresh.to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

fn stored_profile(name: &str, c: Option<ClaudeCredentials>) -> Profile {
    let mut p = Profile::new(name.to_string(), None, None);
    p.credentials = c;
    save_profile(&p).expect("save profile");
    p
}

/// Active profile `active` has stored creds; CC re-logged into a different
/// account, so the live `~/.claude/.credentials.json` is a plain diverged file
/// carrying `live_bytes`. `target` is a second profile to switch to.
fn seed_diverged(
    active: &str,
    active_stored: ClaudeCredentials,
    live: &ClaudeCredentials,
    target: &str,
    target_stored: Option<ClaudeCredentials>,
) -> AppConfig {
    let active_profile = stored_profile(active, Some(active_stored));
    let target_profile = stored_profile(target, target_stored);

    let live_path = crate::profile::claude_dir()
        .expect("claude dir")
        .join(".credentials.json");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir .claude");
    std::fs::write(&live_path, serde_json::to_vec(live).expect("ser live")).expect("write live");

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![active_profile, target_profile],
    };
    config.state.active_profile = Some(active.into());
    config.state.profiles = vec![active.into(), target.into()];
    config
}

#[test]
fn divergence_without_default_errors() {
    let _home = HomeSandbox::new();
    let live = creds("relogin-a", "relogin-r");
    let mut config = seed_diverged(
        "active",
        creds("stored-a", "stored-r"),
        &live,
        "target",
        Some(creds("target-a", "target-r")),
    );

    let err = switch_profile_noninteractive(&mut config, "target", None);
    assert!(
        err.is_err(),
        "diverged active with no default must error, never prompt"
    );
}

#[test]
fn divergence_new_profile_default_errors() {
    let _home = HomeSandbox::new();
    let live = creds("relogin-a", "relogin-r");
    let mut config = seed_diverged(
        "active",
        creds("stored-a", "stored-r"),
        &live,
        "target",
        Some(creds("target-a", "target-r")),
    );

    let err =
        switch_profile_noninteractive(&mut config, "target", Some(DivergenceChoice::NewProfile));
    assert!(
        err.is_err(),
        "'save as new profile' needs an interactive name prompt; headless must error",
    );
}

#[test]
fn divergence_overwrite_captures_relogin_into_outgoing() {
    let _home = HomeSandbox::new();
    let live = creds("relogin-a", "relogin-r");
    let mut config = seed_diverged(
        "active",
        creds("stored-a", "stored-r"),
        &live,
        "target",
        Some(creds("target-a", "target-r")),
    );

    let (previous, active) =
        switch_profile_noninteractive(&mut config, "target", Some(DivergenceChoice::Overwrite))
            .expect("overwrite switch");
    assert_eq!(previous.as_deref(), Some("active"));
    assert_eq!(active, "target");

    // Overwrite = force-snapshot the live re-login into the OUTGOING profile.
    let on_disk: ClaudeCredentials =
        read_json_file(&profile_dir("active").unwrap().join("credentials.json"))
            .expect("read outgoing creds");
    assert_eq!(
        on_disk.refresh_token(),
        Some("relogin-r"),
        "overwrite must capture the live re-login into the outgoing profile",
    );

    // …and the active link ends pointing at target's stored creds.
    let live_now: ClaudeCredentials = read_json_file(
        &crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json"),
    )
    .expect("read live creds");
    assert_eq!(
        live_now.refresh_token(),
        Some("target-r"),
        "overwrite must end with the active link pointing at target",
    );
}

#[test]
fn divergence_discard_drops_relogin_and_relinks_target() {
    let _home = HomeSandbox::new();
    let live = creds("relogin-a", "relogin-r");
    let mut config = seed_diverged(
        "active",
        creds("stored-a", "stored-r"),
        &live,
        "target",
        Some(creds("target-a", "target-r")),
    );

    let (previous, active) =
        switch_profile_noninteractive(&mut config, "target", Some(DivergenceChoice::Discard))
            .expect("discard switch succeeds — it force-relinks past the divergence guard");
    assert_eq!(previous.as_deref(), Some("active"));
    assert_eq!(active, "target");

    // The live file now resolves to target's stored creds (the foreign re-login
    // was dropped, not captured).
    let live_now: ClaudeCredentials = read_json_file(
        &crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json"),
    )
    .expect("read live creds");
    assert_eq!(
        live_now.refresh_token(),
        Some("target-r"),
        "discard relinks the live file to target's stored creds",
    );

    // The OUTGOING profile's stored chain is untouched — the foreign login was
    // never captured into it.
    let outgoing: ClaudeCredentials =
        read_json_file(&profile_dir("active").unwrap().join("credentials.json"))
            .expect("read outgoing creds");
    assert_eq!(
        outgoing.refresh_token(),
        Some("stored-r"),
        "discard must not capture the live login into the outgoing profile",
    );
}

#[test]
fn non_diverged_switch_takes_plain_path() {
    let _home = HomeSandbox::new();
    // Active profile is freshly linked (LinkedTo), not diverged: any
    // `on_divergence` is irrelevant and the switch just succeeds.
    let active_stored = creds("stored-a", "stored-r");
    let active_profile = stored_profile("active", Some(active_stored.clone()));
    let target_profile = stored_profile("target", Some(creds("target-a", "target-r")));

    // Link the live path to the active profile so it classifies as LinkedTo.
    crate::claude::force_link_profile_credentials("active").expect("link active");

    let mut config = AppConfig {
        state: AppState {
            active_profile: Some("active".into()),
            profiles: vec!["active".into(), "target".into()],
            ..Default::default()
        },
        profiles: vec![active_profile, target_profile],
    };

    assert_eq!(
        classify_credentials_link("active").expect("classify"),
        LinkState::LinkedTo,
        "precondition: active is cleanly linked, not diverged",
    );

    let (previous, active) =
        switch_profile_noninteractive(&mut config, "target", None).expect("plain switch");
    assert_eq!(previous.as_deref(), Some("active"));
    assert_eq!(active, "target");
}
