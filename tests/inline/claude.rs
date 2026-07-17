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

// ── CAP-1: the snapshot decides and writes on ONE read of the live file ─────

/// A live login belonging to a different account (Diverged) is never captured
/// by the unattended snapshot — the outgoing profile keeps its own chain. This
/// is the incident shape of 2026-07-12: a running claude wrote a sibling's
/// rotated pair into the live mirror, and the capture window between classify
/// and write copied it into the wrong profile's store.
#[cfg(unix)]
#[test]
fn snapshot_never_captures_a_foreign_live_login() {
    let _home = HomeSandbox::new();
    let mut config = seed_relogin_scenario(
        "active",
        creds("own-access", Some("own-refresh")),
        creds("foreign-access", Some("foreign-refresh")),
    );

    snapshot_active_credentials(&mut config).expect("snapshot");

    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("active")
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("stored parse");
    assert_eq!(
        stored.access_token(),
        Some("own-access"),
        "a diverged live login must never overwrite the stored identity",
    );
}

/// Equal access token ⇒ same rotation state: the snapshot refreshes the store
/// with exactly the bytes it examined (CC rewrites the mirror with identical
/// tokens but sometimes fresher metadata, e.g. `subscription_type`).
#[cfg(unix)]
#[test]
fn snapshot_refreshes_the_store_when_live_token_matches() {
    let _home = HomeSandbox::new();
    let mut live = creds("same-access", Some("same-refresh"));
    live.claude_ai_oauth
        .as_mut()
        .expect("oauth")
        .subscription_type = Some("max".into());
    let mut config =
        seed_relogin_scenario("active", creds("same-access", Some("same-refresh")), live);

    snapshot_active_credentials(&mut config).expect("snapshot");

    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("active")
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("stored parse");
    assert_eq!(
        stored
            .claude_ai_oauth
            .as_ref()
            .and_then(|o| o.subscription_type.as_deref()),
        Some("max"),
        "an equal-token live snapshot carries the live metadata into the store",
    );
}

/// A completed first login adopts the bytes the check saw AND anchors the
/// profile's identity to the login just captured (from CC's own
/// `~/.claude.json` hint), so the identity-guarded adopt/follow paths can
/// vouch for the profile immediately.
#[cfg(unix)]
#[test]
fn first_login_adopt_anchors_the_profile() {
    let _home = HomeSandbox::new();
    let home = crate::profile::home_dir().expect("home");
    std::fs::write(
        home.join(".claude.json"),
        r#"{"oauthAccount":{"accountUuid":"uuid-first-login"}}"#,
    )
    .expect("write claude.json");

    let live_path = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir .claude");
    std::fs::write(
        &live_path,
        serde_json::to_vec(&creds("fresh-access", Some("fresh-refresh"))).expect("ser"),
    )
    .expect("write live");

    let profile = crate::profile::Profile::new("newbie".to_string(), None, None);
    crate::profile::save_profile(&profile).expect("save profile");
    let mut config = AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile],
    };
    config.state.active_profile = Some("newbie".into());
    config.state.profiles = vec!["newbie".into()];

    snapshot_active_credentials(&mut config).expect("snapshot");

    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("newbie")
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("stored parse");
    assert_eq!(stored.access_token(), Some("fresh-access"));
    let anchor: Option<String> = crate::profile_cache::load_profile_cache(
        "newbie",
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
    );
    assert_eq!(
        anchor.as_deref(),
        Some("uuid-first-login"),
        "the adopt anchors the profile to the captured login's identity",
    );
}

// ── RESCUE-2c: write_live_oauth_pair's in-lock supersede guards ───────────────

fn rotated_pair() -> crate::oauth::TokenResponse {
    crate::oauth::TokenResponse {
        access_token: "at-rotated".to_string(),
        refresh_token: "rt-rotated".to_string(),
        expires_in: 28_800,
        scope: None,
    }
}

/// A fresh CC login landing between the caller's judgment and the write-back
/// must survive: the stale expected-fingerprint makes the write a benign
/// `Superseded` no-op instead of clobbering the fresh login.
#[cfg(unix)]
#[test]
fn write_back_supersedes_when_the_live_fingerprint_moved() {
    let _home = HomeSandbox::new();
    let live = crate::profile::claude_dir()
        .expect("dir")
        .join(".credentials.json");
    fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    fs::write(
        &live,
        serde_json::to_vec(&creds("at-corpse", Some("rt-corpse"))).expect("ser"),
    )
    .expect("write corpse");
    let corpse_fingerprint = live_credentials_fingerprint();

    // A fresh login lands before the write-back.
    fs::write(
        &live,
        serde_json::to_vec(&creds("at-fresh", Some("rt-fresh"))).expect("ser"),
    )
    .expect("write fresh");

    let outcome = write_live_oauth_pair(&rotated_pair(), corpse_fingerprint).expect("write back");
    assert_eq!(outcome, LiveWriteBack::Superseded);
    let survived: ClaudeCredentials = crate::profile::read_json_file(&live).expect("read");
    assert_eq!(
        survived.access_token(),
        Some("at-fresh"),
        "the freshly landed login must never be clobbered"
    );
}

/// A profile's own store taking the slot (symlink) mid-rescue is the same
/// benign supersede — NOT an error (the old behavior surfaced it as a scary
/// "its chain is lost; re-login" message for what loses nothing).
#[cfg(unix)]
#[test]
fn write_back_supersedes_when_the_live_path_became_a_symlink() {
    let _home = HomeSandbox::new();
    let mut _config = seed_relogin_scenario(
        "active",
        creds("stored-access", Some("stored-refresh")),
        creds("relogin-access", Some("relogin-refresh")),
    );
    let fingerprint = live_credentials_fingerprint();
    force_link_profile_credentials("active").expect("relink");

    let outcome = write_live_oauth_pair(&rotated_pair(), fingerprint).expect("write back");
    assert_eq!(outcome, LiveWriteBack::Superseded);
    assert_eq!(
        classify_credentials_link("active").expect("classify"),
        LinkState::LinkedTo,
        "the profile's symlink is left untouched"
    );
    // The profile's stored chain was not corrupted by the unowned pair.
    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("active")
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read stored");
    assert_eq!(stored.access_token(), Some("stored-access"));
}

/// The happy path still writes surgically: tokens + expiry replaced, every
/// foreign top-level key (mcpOAuth) preserved.
#[cfg(unix)]
#[test]
fn write_back_writes_in_place_and_preserves_foreign_keys() {
    let _home = HomeSandbox::new();
    let live = crate::profile::claude_dir()
        .expect("dir")
        .join(".credentials.json");
    fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    fs::write(
        &live,
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": { "accessToken": "at-corpse", "refreshToken": "rt-corpse" },
            "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
        }))
        .expect("ser"),
    )
    .expect("write corpse");
    let fingerprint = live_credentials_fingerprint();

    let outcome = write_live_oauth_pair(&rotated_pair(), fingerprint).expect("write back");
    assert_eq!(outcome, LiveWriteBack::Written);
    let root: serde_json::Value = crate::profile::read_json_file(&live).expect("read");
    assert_eq!(root["claudeAiOauth"]["accessToken"], "at-rotated");
    assert_eq!(root["claudeAiOauth"]["refreshToken"], "rt-rotated");
    assert_eq!(
        root["mcpOAuth"]["some-server"]["accessToken"], "mcp-tok",
        "foreign top-level keys must survive the surgical write"
    );
}

/// RESCUE-2 archive retention: the quarantine keeps the newest 20 copies and
/// prunes older ones; same-millisecond archives never overwrite each other
/// (the per-process sequence uniquifies names).
#[cfg(unix)]
#[test]
fn quarantine_archive_prunes_to_the_newest_twenty() {
    let _home = HomeSandbox::new();
    let live = crate::profile::claude_dir()
        .expect("dir")
        .join(".credentials.json");
    fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");

    for i in 0..25 {
        fs::write(
            &live,
            serde_json::to_vec(&creds(&format!("at-foreign-{i:02}"), Some("rt"))).expect("ser"),
        )
        .expect("write live");
        archive_live_credentials("victim").expect("archive");
    }

    let dir = crate::profile::clauth_dir()
        .expect("dir")
        .join("quarantine");
    let mut archived: Vec<_> = fs::read_dir(&dir)
        .expect("read quarantine")
        .map(|e| e.expect("entry").path())
        .collect();
    archived.sort();
    assert_eq!(archived.len(), 20, "retention keeps exactly the newest 20");
    let newest = fs::read_to_string(archived.last().expect("newest")).expect("read newest");
    assert!(
        newest.contains("at-foreign-24"),
        "the newest archive holds the last-archived login"
    );
    let oldest = fs::read_to_string(archived.first().expect("oldest")).expect("read oldest");
    assert!(
        oldest.contains("at-foreign-05"),
        "pruning removed the five oldest archives"
    );
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
