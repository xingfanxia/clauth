use super::*;
use crate::profile::{ClaudeCredentials, OAuthToken};
use std::fs;

fn creds(access: &str, refresh: Option<&str>) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: access.to_string(),
            refresh_token: refresh.map(str::to_string),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

#[test]
fn diverged_returns_false_when_either_side_missing() {
    let c = creds("a", Some("r"));
    assert!(!credentials_diverged(None, Some(&c)));
    assert!(!credentials_diverged(Some(&c), None));
    assert!(!credentials_diverged(None, None));
}

#[test]
fn diverged_returns_false_when_tokens_match() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-1", Some("refresh-1"));
    assert!(!credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_true_when_access_token_differs() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-2", Some("refresh-1"));
    assert!(credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_true_when_refresh_token_differs() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-1", Some("refresh-2"));
    assert!(credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_true_when_refresh_token_disappears() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-1", None);
    assert!(credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_false_when_oauth_block_missing_on_one_side() {
    let with = creds("a", Some("r"));
    let without = ClaudeCredentials {
        claude_ai_oauth: None,
    };
    assert!(!credentials_diverged(Some(&with), Some(&without)));
    assert!(!credentials_diverged(Some(&without), Some(&with)));
}

#[test]
fn classify_link_missing_when_path_absent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Missing,
    );
}

#[test]
fn classify_link_diverged_when_plain_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(&link, b"{}").expect("write live");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Diverged,
    );
}

/// macOS reality: Claude Code rewrites `~/.claude/.credentials.json` as a plain-file
/// mirror of the Keychain after every run, replacing clauth's symlink. When the live
/// token still matches the active profile's stored token, that is NOT divergence —
/// classify must report LinkedTo so an ordinary switch doesn't falsely prompt to
/// capture credentials that already match. (Regression: the switch prompt fired on
/// every `clauth <name>` because a plain file was unconditionally Diverged.)
#[test]
fn classify_link_linked_to_when_plain_file_token_matches_stored() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    let same = serde_json::to_vec(&creds("same-access", Some("same-refresh"))).expect("ser");
    fs::write(&link, &same).expect("write live");
    fs::write(&expected, &same).expect("write stored");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::LinkedTo,
        "a plain file whose token matches the profile is CC's mirror, not divergence",
    );
}

/// A plain file whose access token DIFFERS from the profile's stored token is a
/// genuine CC re-login / rotation — still Diverged so the capture prompt fires.
#[test]
fn classify_link_diverged_when_plain_file_token_differs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(
        &link,
        serde_json::to_vec(&creds("live-access", Some("r"))).expect("ser"),
    )
    .expect("write live");
    fs::write(
        &expected,
        serde_json::to_vec(&creds("stored-access", Some("r"))).expect("ser"),
    )
    .expect("write stored");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Diverged,
    );
}

/// A degenerate empty access token on both sides is a corrupt/partial write, not
/// a completed login — it must NOT read as `LinkedTo` just because two empty
/// strings compare equal. Matches the completed-login intent of `is_first_login`.
#[test]
fn classify_link_diverged_when_plain_file_access_token_empty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    let empty = serde_json::to_vec(&creds("", Some("r"))).expect("ser");
    fs::write(&link, &empty).expect("write live");
    fs::write(&expected, &empty).expect("write stored");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Diverged,
        "an empty access token is not a completed login, so it is not a mirror",
    );
}

#[cfg(unix)]
#[test]
fn classify_link_linked_to_when_pointing_at_expected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(&expected, b"{}").expect("write target");
    std::os::unix::fs::symlink(&expected, &link).expect("symlink");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::LinkedTo,
    );
}

#[cfg(unix)]
#[test]
fn classify_link_diverged_when_symlink_points_elsewhere() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    let other = tmp.path().join("other.json");
    fs::write(&other, b"{}").expect("write other");
    fs::write(&expected, b"{}").expect("write target");
    std::os::unix::fs::symlink(&other, &link).expect("symlink");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Diverged,
    );
}

#[test]
fn first_login_true_when_no_stored_creds_and_plain_oauth_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(
        &link,
        serde_json::to_vec(&creds("a", Some("r"))).expect("ser"),
    )
    .expect("write");
    assert!(is_first_login_at(&link, &expected));
}

#[test]
fn first_login_false_when_stored_creds_exist() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(
        &link,
        serde_json::to_vec(&creds("a", Some("r"))).expect("ser"),
    )
    .expect("write");
    fs::write(&expected, b"{}").expect("write stored");
    assert!(!is_first_login_at(&link, &expected));
}

#[test]
fn first_login_false_when_link_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    assert!(!is_first_login_at(&link, &expected));
}

#[test]
fn first_login_false_when_oauth_block_absent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    // valid JSON but no OAuth block — mid-flight partial write
    fs::write(&link, b"{}").expect("write");
    assert!(!is_first_login_at(&link, &expected));
}

#[cfg(unix)]
#[test]
fn first_login_false_when_link_is_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    let store = tmp.path().join("store.json");
    fs::write(
        &store,
        serde_json::to_vec(&creds("a", Some("r"))).expect("ser"),
    )
    .expect("write");
    std::os::unix::fs::symlink(&store, &link).expect("symlink");
    assert!(!is_first_login_at(&link, &expected));
}

#[cfg(unix)]
#[test]
fn classify_link_linked_to_even_when_target_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    std::os::unix::fs::symlink(&expected, &link).expect("symlink");
    // target absent (e.g. first-ever link, before save_profile writes it)
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::LinkedTo,
    );
}

// ── account-change `[Y/n]` overwrite path ──────────────────────────────────
//
// When Claude Code re-logged into a different account while clauth was closed,
// the live `~/.claude/.credentials.json` is a plain file diverging from the
// active profile's stored chain. clauth shows a `[Y/n]` prompt before the
// stored tokens are overwritten. These tests pin the prompt's GATE (when it
// fires) and both BRANCHES (confirm overwrites/captures, cancel is a no-op) at
// the home-derived seam the prompt actually drives, no TTY needed.

#[cfg(unix)]
use crate::testutil::HomeSandbox;

/// Seed an active profile `name` with stored credentials, then simulate CC
/// re-logging into a different account: write a plain (non-symlink) live
/// `~/.claude/.credentials.json` carrying `live`. Returns the assembled config.
#[cfg(unix)]
fn seed_relogin_scenario(
    name: &str,
    stored: ClaudeCredentials,
    live: ClaudeCredentials,
) -> AppConfig {
    let mut profile = crate::profile::Profile::new(name.to_string(), None, None);
    profile.credentials = Some(stored);
    crate::profile::save_profile(&profile).expect("save profile");

    let live_path = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir .claude");
    std::fs::write(&live_path, serde_json::to_vec(&live).expect("ser live")).expect("write live");

    let mut config = AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile],
    };
    config.state.active_profile = Some(name.into());
    config.state.profiles = vec![name.into()];
    config
}

/// The `[Y/n]` prompt's gate: a re-login is a Diverged plain file that is NOT a
/// first login (the profile already has stored creds), so the prompt fires.
#[cfg(unix)]
#[test]
fn relogin_is_diverged_and_not_first_login() {
    let _home = HomeSandbox::new();
    let _config = seed_relogin_scenario(
        "active",
        creds("stored-access", Some("stored-refresh")),
        creds("relogin-access", Some("relogin-refresh")),
    );

    assert_eq!(
        classify_credentials_link("active").expect("classify"),
        LinkState::Diverged,
        "a CC re-login leaves a plain file diverging from the stored chain",
    );
    assert!(
        !is_first_login("active").expect("first login"),
        "stored creds exist, so this is a re-login overwrite, not a first login",
    );
}

/// Confirm branch (`y`): capture the live re-login into the active profile, then
/// relink. The stored chain is overwritten with the live one and the live path
/// becomes a symlink back to the profile's now-updated credentials.
#[cfg(unix)]
#[test]
fn overwrite_confirm_captures_relogin_into_profile() {
    let _home = HomeSandbox::new();
    let mut config = seed_relogin_scenario(
        "active",
        creds("stored-access", Some("stored-refresh")),
        creds("relogin-access", Some("relogin-refresh")),
    );

    // `y` answer = force-snapshot the live creds into the active profile, relink.
    force_snapshot_active_credentials(&mut config).expect("snapshot");
    force_link_profile_credentials("active").expect("relink");

    // The profile's stored chain now holds the re-logged tokens.
    let stored = config
        .find("active")
        .and_then(|p| p.credentials.as_ref())
        .and_then(|c| c.refresh_token());
    assert_eq!(
        stored,
        Some("relogin-refresh"),
        "confirm must overwrite the stored chain with the live re-login",
    );

    // The live path is reconciled back to a symlink into the profile.
    assert_eq!(
        classify_credentials_link("active").expect("classify"),
        LinkState::LinkedTo,
        "after capture+relink the live path links to the profile's creds",
    );

    // The on-disk profile credentials file carries the re-logged chain too.
    let on_disk: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("active")
            .expect("profile dir")
            .join("credentials.json"),
    )
    .expect("read stored creds");
    assert_eq!(
        on_disk.refresh_token(),
        Some("relogin-refresh"),
        "the persisted profile credentials must hold the captured chain",
    );
}

/// Cancel branch (`n`): no capture, no relink. The stored chain keeps its old
/// tokens and the live path is left exactly as CC wrote it (untouched).
#[cfg(unix)]
#[test]
fn overwrite_cancel_leaves_stored_and_live_untouched() {
    let _home = HomeSandbox::new();
    let config = seed_relogin_scenario(
        "active",
        creds("stored-access", Some("stored-refresh")),
        creds("relogin-access", Some("relogin-refresh")),
    );

    // `n` answer = abort. We perform no snapshot and no relink; assert the
    // pre-prompt state is preserved.
    let stored = config
        .find("active")
        .and_then(|p| p.credentials.as_ref())
        .and_then(|c| c.refresh_token());
    assert_eq!(
        stored,
        Some("stored-refresh"),
        "cancel must not overwrite the stored chain",
    );

    // The live file CC wrote is still a plain diverged file with its own chain.
    assert_eq!(
        classify_credentials_link("active").expect("classify"),
        LinkState::Diverged,
        "cancel leaves the live re-login in place (still diverged)",
    );
    let live = read_claude_credentials()
        .expect("read live")
        .expect("live present");
    assert_eq!(
        live.refresh_token(),
        Some("relogin-refresh"),
        "cancel must leave the live re-login bytes untouched",
    );
}

#[test]
fn build_settings_writes_model_knobs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json"); // absent → starts from `{}`
    let mut profile = crate::profile::Profile::new("p".to_string(), None, None);
    profile.models = crate::profile::ModelSettings {
        default: Some("opusplan".to_string()),
        opus: Some("claude-opus-4-8[1m]".to_string()),
        sonnet: None,
        haiku: None,
        subagent: Some("claude-haiku-4-5".to_string()),
    };
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build settings");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse settings");
    assert_eq!(v["model"], "opusplan", "default model → top-level `model`");
    assert_eq!(
        v["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"],
        "claude-opus-4-8[1m]"
    );
    assert_eq!(v["env"]["CLAUDE_CODE_SUBAGENT_MODEL"], "claude-haiku-4-5");
    assert!(
        v["env"].get("ANTHROPIC_DEFAULT_SONNET_MODEL").is_none(),
        "an unset tier override writes no env key",
    );
}

// A profile with no model config must strip a previous profile's model knobs
// from the base settings.json, so a switch never inherits stale model routing.
#[test]
fn build_settings_clears_stale_model_knobs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json");
    fs::write(
        &base,
        r#"{"model":"opus","env":{"ANTHROPIC_DEFAULT_OPUS_MODEL":"old","CLAUDE_CODE_SUBAGENT_MODEL":"old","KEEP":"1"}}"#,
    )
    .expect("seed base settings");
    let profile = crate::profile::Profile::new("p".to_string(), None, None); // empty models
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build settings");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse settings");
    assert!(v.get("model").is_none(), "top-level `model` cleared");
    assert!(v["env"].get("ANTHROPIC_DEFAULT_OPUS_MODEL").is_none());
    assert!(v["env"].get("CLAUDE_CODE_SUBAGENT_MODEL").is_none());
    assert_eq!(v["env"]["KEEP"], "1", "unrelated env keys are preserved");
}

// ── logged-out shell detection ────────────────────────────────────────────────
//
// When Claude Code's own token refresh dies it does not delete the live
// `.credentials.json`: it blanks both tokens and zeroes `expiresAt`, keeping
// unrelated keys like `mcpOAuth` — a logged-out shell. A shell still
// classifies Diverged, so without the exemption every guard built on
// "diverged and unsaved" deferred switches behind a TUI decision about an
// empty file.

/// Truth table for [`live_login_is_empty`]: only a login with NO usable token
/// (both absent or blank, or no OAuth block at all) is empty — one live token
/// on either side keeps the login's protections.
#[test]
fn live_login_is_empty_truth_table() {
    // CC's logged-out shell: both tokens blanked.
    assert!(live_login_is_empty(&creds("", Some(""))));
    // Blank access token and no refresh token at all.
    assert!(live_login_is_empty(&creds("", None)));
    // No OAuth block (a file holding only foreign keys like mcpOAuth).
    assert!(live_login_is_empty(&ClaudeCredentials {
        claude_ai_oauth: None,
    }));
    // A live access token alone is a login.
    assert!(!live_login_is_empty(&creds("at-live", None)));
    assert!(!live_login_is_empty(&creds("at-live", Some(""))));
    // A refresh token alone is a login (the access side merely expired).
    assert!(!live_login_is_empty(&creds("", Some("rt-live"))));
    // A full pair is a login.
    assert!(!live_login_is_empty(&creds("at-live", Some("rt-live"))));
}

/// [`live_credentials_are_shell`] is true only for a PARSED empty login: a
/// missing file is not a shell, and an unreadable/non-JSON file is not a shell
/// either (it may be a CC write in progress — "possibly a login" must keep a
/// real login's protections).
#[test]
fn live_credentials_are_shell_requires_a_parsed_empty_login() {
    let _home = crate::testutil::HomeSandbox::new();
    let live = claude_credentials_path().expect("creds path");
    fs::create_dir_all(live.parent().expect("parent")).expect("mkdir .claude");

    // Missing file: nothing there to call a shell.
    assert!(!live_credentials_are_shell());

    // CC's logged-out shell, foreign keys and all.
    fs::write(
        &live,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "",
                "refreshToken": "",
                "expiresAt": 0,
                "scopes": ["user:inference"],
                "subscriptionType": "max",
            },
            "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
        })
        .to_string(),
    )
    .expect("write shell");
    assert!(live_credentials_are_shell());

    // No OAuth block at all is the same shell.
    fs::write(&live, r#"{"mcpOAuth":{}}"#).expect("write oauth-less file");
    assert!(live_credentials_are_shell());

    // Torn JSON (a write in progress): NOT a shell — guards stay armed.
    fs::write(&live, br#"{"claudeAiOauth":{"accessToken":""#).expect("write torn file");
    assert!(!live_credentials_are_shell());

    // A real login: not a shell.
    fs::write(
        &live,
        serde_json::to_vec(&creds("at-live", Some("rt-live"))).expect("ser live"),
    )
    .expect("write live");
    assert!(!live_credentials_are_shell());
}

// ── CLA-SPLIT: long-lived session token beside the usage OAuth pair ───────────

/// Write a `session-token.json` (static long-lived login) into `name`'s
/// profile dir, as the split-credential fill does.
fn fill_session_token_by_hand(name: &str, access: &str) {
    let dir = crate::profile::profile_dir(name).expect("profile dir");
    fs::create_dir_all(&dir).expect("mkdir profile");
    fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&creds(access, None)).expect("ser session token"),
    )
    .expect("write session token");
}

/// The install source is `credentials.json` until a session token appears,
/// then the session token — and never the OAuth pair while it exists.
#[test]
fn install_source_prefers_session_token() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("split".to_string(), None, None);
    profile.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    assert!(!has_session_token("split"));
    assert!(
        install_source_path("split")
            .expect("source")
            .ends_with("credentials.json")
    );

    fill_session_token_by_hand("split", "oat-access");
    assert!(has_session_token("split"));
    assert!(
        install_source_path("split")
            .expect("source")
            .ends_with("session-token.json")
    );
}

/// A live slot holding the profile's static session token is the designed
/// steady state: LinkedTo (the divergence machinery stays dormant), and a
/// snapshot leaves the clauth-private usage OAuth pair untouched instead of
/// clobbering it with the token just read.
#[test]
fn session_token_live_is_linked_and_snapshot_keeps_usage_oauth() {
    let _home = HomeSandbox::new();
    let mut config = seed_relogin_scenario(
        "split",
        creds("usage-access", Some("usage-refresh")),
        creds("oat-access", None),
    );
    fill_session_token_by_hand("split", "oat-access");

    assert_eq!(
        classify_credentials_link("split").expect("classify"),
        LinkState::LinkedTo,
        "live slot holding the session token is the steady state, not divergence",
    );

    snapshot_active_credentials(&mut config).expect("snapshot");
    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("split")
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read stored");
    assert_eq!(
        stored.refresh_token(),
        Some("usage-refresh"),
        "snapshot must never overwrite the usage OAuth pair with the session token",
    );
}

/// A switch to a session-token profile links the LIVE slot to
/// `session-token.json` — the rotating usage pair is never installed, and it
/// survives the switch on disk byte-for-byte.
#[cfg(unix)]
#[test]
fn switch_installs_session_token_not_usage_oauth() {
    let _home = HomeSandbox::new();

    let mut a = crate::profile::Profile::new("a".to_string(), None, None);
    a.credentials = Some(creds("at-a", Some("rt-a")));
    crate::profile::save_profile(&a).expect("save a");
    let mut b = crate::profile::Profile::new("b".to_string(), None, None);
    b.credentials = Some(creds("usage-access-b", Some("usage-refresh-b")));
    crate::profile::save_profile(&b).expect("save b");
    fill_session_token_by_hand("b", "oat-b");

    let mut config = AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![a, b],
    };
    config.state.profiles = vec!["a".into(), "b".into()];
    config.state.active_profile = Some("a".into());
    force_link_profile_credentials("a").expect("link a");

    crate::actions::switch_profile(&mut config, "b").expect("switch to b");

    let live_target =
        std::fs::read_link(claude_credentials_path().expect("path")).expect("live is a symlink");
    assert!(
        live_target.ends_with("session-token.json"),
        "the live slot must point at b's session token, got {live_target:?}",
    );
    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("b")
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read b store");
    assert_eq!(
        stored.refresh_token(),
        Some("usage-refresh-b"),
        "b's usage OAuth pair must survive the switch untouched",
    );
}

// ── CLA-SPLIT-2: the `--setup-token` capture flow's building blocks ───────────

/// The paste validator refuses everything but a clean single-token mint: a
/// broken sidecar signs every session out on first use, so the failure has to
/// happen at the paste, loudly, and without echoing the value.
#[test]
fn validate_setup_token_accepts_a_mint_and_rejects_bad_pastes() {
    let good = format!("sk-ant-oat01-{}", "x".repeat(48));
    assert_eq!(
        validate_setup_token(&format!("  {good}\n")).expect("valid"),
        good,
        "surrounding whitespace trims away"
    );
    assert!(validate_setup_token("").is_err(), "empty paste");
    assert!(validate_setup_token("   \n").is_err(), "blank paste");
    assert!(
        validate_setup_token("api-key-not-a-mint-0123456789012345678901234567890").is_err(),
        "wrong prefix"
    );
    assert!(
        validate_setup_token(&format!("Setup token: {good}")).is_err(),
        "paste with prompt text has interior whitespace"
    );
    assert!(validate_setup_token("sk-ant-short").is_err(), "truncated paste");
}

/// The capture writes a sidecar the whole CLA-SPLIT machinery recognises:
/// `has_session_token` flips, the install source re-points, the stamped
/// one-year horizon reads back through `session_token_expiry`, and the file
/// carries credential permissions.
#[test]
fn write_session_token_produces_a_recognised_sidecar() {
    let _home = HomeSandbox::new();
    let profile = crate::profile::Profile::new("cap".to_string(), None, None);
    crate::profile::save_profile(&profile).expect("save profile");
    assert_eq!(session_token_status("cap"), None, "no sidecar yet");

    let now = 1_700_000_000_000_i64;
    let token = format!("sk-ant-oat01-{}", "y".repeat(48));
    let stamped = write_session_token("cap", &token, now).expect("write sidecar");
    assert_eq!(stamped, now + SETUP_TOKEN_ASSUMED_LIFETIME_MS);

    assert!(has_session_token("cap"));
    assert!(
        install_source_path("cap")
            .expect("source")
            .ends_with("session-token.json")
    );
    assert_eq!(
        session_token_status("cap"),
        Some(SessionTokenStatus::LongLived(Some(stamped)))
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(
            crate::profile::profile_dir("cap")
                .expect("dir")
                .join("session-token.json"),
        )
        .expect("meta")
        .permissions()
        .mode();
        assert_eq!(mode & 0o777, 0o600, "sidecar is a credential file");
    }
}

/// A hand-rolled sidecar without `expiresAt` still reports "present, horizon
/// unknown" — never `None` (which would hide the token row entirely).
#[test]
fn session_token_status_distinguishes_missing_from_unstamped() {
    let _home = HomeSandbox::new();
    let profile = crate::profile::Profile::new("hand".to_string(), None, None);
    crate::profile::save_profile(&profile).expect("save profile");
    fill_session_token_by_hand("hand", "oat-access");
    assert_eq!(
        session_token_status("hand"),
        Some(SessionTokenStatus::LongLived(None))
    );
}

// ── #53 review: the split engages only for a genuinely LONG-LIVED token ──────

/// A sidecar mis-filled with a rotating pair (refresh token present) must NOT
/// engage the split: it reads `NotLongLived`, `has_session_token` stays
/// false, and the install source falls back to `credentials.json` exactly as
/// if the sidecar weren't there — installing a dies-in-hours token with no
/// refresher behind it is the failure this detection exists to prevent.
#[test]
fn a_rotating_pair_in_the_sidecar_never_engages_the_split() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("mis".to_string(), None, None);
    profile.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    let dir = crate::profile::profile_dir("mis").expect("profile dir");
    fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&creds("rotating-access", Some("rotating-refresh")))
            .expect("ser sidecar"),
    )
    .expect("write sidecar");

    assert_eq!(
        session_token_status("mis"),
        Some(SessionTokenStatus::NotLongLived)
    );
    assert!(!has_session_token("mis"), "the split stays disengaged");
    assert!(
        install_source_path("mis")
            .expect("source")
            .ends_with("credentials.json"),
        "switches keep installing the rotating pair from credentials.json"
    );
}
