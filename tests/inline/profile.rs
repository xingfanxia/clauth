//! Regression tests pinning the serde alias that lets clauth 0.2.0 users
//! upgrade without losing their persisted settings: `kick_timer` (per-profile
//! config.toml) was renamed to `auto_start` after 0.2.0. Drop the alias and the
//! test below fails.

use super::*;

#[test]
fn profile_config_reads_kick_timer_as_auto_start() {
    let toml = "kick_timer = true\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse old config");
    assert!(cfg.auto_start);
}

#[test]
fn profile_config_reads_auto_start_directly() {
    let toml = "auto_start = true\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse new config");
    assert!(cfg.auto_start);
}

// Drop `bell_threshold` from `ProfileConfig` and the hand-edited value is
// silently ignored on load (the bug this pins): the field must round-trip.
#[test]
fn profile_config_reads_bell_threshold() {
    let toml = "bell_threshold = 90.0\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse bell config");
    assert_eq!(cfg.bell_threshold, Some(90.0));
}

// `last_resort` (issue #8 follow-up) must default to `false` so every existing
// config.toml written before this field existed keeps loading unchanged.
#[test]
fn profile_config_last_resort_defaults_false() {
    let cfg: ProfileConfig = toml::from_str("").expect("parse empty config");
    assert!(!cfg.last_resort);
}

#[test]
fn profile_config_reads_last_resort_true() {
    let toml = "last_resort = true\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse last_resort config");
    assert!(cfg.last_resort);
}

// `last_resort` must survive a config.toml render→parse round-trip, matching
// the guarantee `model_settings_round_trip_through_config_toml` pins for models.
#[test]
fn last_resort_round_trips_through_config_toml() {
    let mut profile = Profile::new("p".to_string(), None, None);
    profile.last_resort = true;
    let rendered = render_config_toml(&profile);
    let parsed: ProfileConfig = toml::from_str(&rendered).expect("parse rendered toml");
    assert!(parsed.last_resort);
}

// `burn_aware_switching` (issue #8 follow-up b) must default to `false` so
// every existing profiles.toml written before this field existed keeps
// loading unchanged, matching the `last_resort` guarantee above at the
// `AppState` level.
#[test]
fn app_state_burn_aware_switching_defaults_false() {
    let state: AppState = toml::from_str("profiles = []\n").expect("parse state");
    assert!(!state.burn_aware_switching);
}

#[test]
fn app_state_reads_burn_aware_switching_true() {
    let toml = "profiles = []\nburn_aware_switching = true\n";
    let state: AppState = toml::from_str(toml).expect("parse state");
    assert!(state.burn_aware_switching);
}

// On must round-trip explicitly; off (the default) is omitted entirely from
// the rendered profiles.toml, matching `show_pace`/`count_cache`'s treatment
// of their own default-off booleans.
#[test]
fn burn_aware_switching_round_trips_and_is_omitted_when_off() {
    let on = AppState {
        burn_aware_switching: true,
        ..AppState::default()
    };
    let rendered_on = toml::to_string_pretty(&on).expect("render on state");
    assert!(
        rendered_on.contains("burn_aware_switching = true"),
        "on must render explicitly, got:\n{rendered_on}"
    );
    let reparsed: AppState = toml::from_str(&rendered_on).expect("reparse on state");
    assert!(reparsed.burn_aware_switching);

    let off = AppState::default();
    let rendered_off = toml::to_string_pretty(&off).expect("render default state");
    assert!(
        !rendered_off.contains("burn_aware_switching"),
        "off (default) must be omitted, got:\n{rendered_off}"
    );
}

// `preemptive_rotation` (rotation coherence #1) shares `burn_aware_switching`'s
// serde contract exactly: absent from old state files → false (stock stays
// strictly lazy), on renders explicitly, off is omitted.
#[test]
fn preemptive_rotation_defaults_false_and_round_trips() {
    let state: AppState = toml::from_str("profiles = []\n").expect("parse state");
    assert!(!state.preemptive_rotation);

    let on = AppState {
        preemptive_rotation: true,
        ..AppState::default()
    };
    let rendered_on = toml::to_string_pretty(&on).expect("render on state");
    assert!(
        rendered_on.contains("preemptive_rotation = true"),
        "on must render explicitly, got:\n{rendered_on}"
    );
    let reparsed: AppState = toml::from_str(&rendered_on).expect("reparse on state");
    assert!(reparsed.preemptive_rotation);

    let rendered_off = toml::to_string_pretty(&AppState::default()).expect("render default state");
    assert!(
        !rendered_off.contains("preemptive_rotation"),
        "off (default) must be omitted, got:\n{rendered_off}"
    );
}

// `auto_rescue` (isolated-transcript rescue) shares `preemptive_rotation`'s
// serde contract exactly: absent from old state files → false (stock discards
// an isolated store on teardown), on renders explicitly, off is omitted.
#[test]
fn auto_rescue_defaults_false_and_round_trips() {
    let state: AppState = toml::from_str("profiles = []\n").expect("parse state");
    assert!(!state.auto_rescue);

    let on = AppState {
        auto_rescue: true,
        ..AppState::default()
    };
    let rendered_on = toml::to_string_pretty(&on).expect("render on state");
    assert!(
        rendered_on.contains("auto_rescue = true"),
        "on must render explicitly, got:\n{rendered_on}"
    );
    let reparsed: AppState = toml::from_str(&rendered_on).expect("reparse on state");
    assert!(reparsed.auto_rescue);

    let rendered_off = toml::to_string_pretty(&AppState::default()).expect("render default state");
    assert!(
        !rendered_off.contains("auto_rescue"),
        "off (default) must be omitted, got:\n{rendered_off}"
    );
}

// `refresh_spent_accounts` defaults to TRUE (poll every account — today's
// behavior) so pre-field profiles.toml files load unchanged; only an explicit
// `false` opt-out renders, and the default is omitted (the inverse serde shape
// of the default-off toggles above, matching `show_estimates`).
#[test]
fn refresh_spent_accounts_defaults_true_and_round_trips() {
    let state: AppState = toml::from_str("profiles = []\n").expect("parse state");
    assert!(state.refresh_spent_accounts, "absent → default on");

    let off = AppState {
        refresh_spent_accounts: false,
        ..AppState::default()
    };
    let rendered_off = toml::to_string_pretty(&off).expect("render off state");
    assert!(
        rendered_off.contains("refresh_spent_accounts = false"),
        "an explicit opt-out must render, got:\n{rendered_off}"
    );
    let reparsed: AppState = toml::from_str(&rendered_off).expect("reparse off state");
    assert!(!reparsed.refresh_spent_accounts);

    let rendered_on = toml::to_string_pretty(&AppState::default()).expect("render default state");
    assert!(
        !rendered_on.contains("refresh_spent_accounts"),
        "default (on) must be omitted, got:\n{rendered_on}"
    );
}

#[test]
fn profile_name_is_serde_transparent() {
    // `ProfileName` must serialize as a bare string so profiles.toml stays
    // byte-identical to the pre-newtype format (a non-transparent newtype
    // would silently migrate every user's state file).
    let toml = r#"active_profile = "work"
profiles = ["work", "play"]
fallback_chain = ["work"]
"#;
    let state: AppState = toml::from_str(toml).expect("parse bare-string state");
    assert_eq!(state.active_profile.as_deref(), Some("work"));
    assert_eq!(state.profiles, ["work", "play"]);
    assert_eq!(state.fallback_chain, ["work"]);

    let rendered = toml::to_string_pretty(&state).expect("render state");
    let reparsed: AppState = toml::from_str(&rendered).expect("reparse");
    assert_eq!(reparsed.active_profile.as_deref(), Some("work"));
    assert_eq!(reparsed.profiles, ["work", "play"]);
    assert_eq!(reparsed.fallback_chain, ["work"]);
    assert!(
        rendered.contains("active_profile = \"work\""),
        "active_profile must render as a bare string, got:\n{rendered}"
    );
    assert!(
        rendered.contains("\"work\"") && rendered.contains("\"play\""),
        "profile names must render as bare strings, got:\n{rendered}"
    );
    assert!(
        !rendered.contains("ProfileName") && !rendered.contains("[profiles."),
        "no newtype wrapper may appear on disk, got:\n{rendered}"
    );

    // Byte-for-byte equality with a String-typed control — no format migration.
    // Field order and serde attrs mirror `AppState`'s ON-DISK shape exactly, so
    // this field is spelled `wrap_off` (the published key) rather than
    // `switch_off_when_spent` (the Rust name behind `serde(rename)`).
    #[derive(serde::Serialize, Default)]
    struct BareState {
        active_profile: Option<String>,
        profiles: Vec<String>,
        fallback_chain: Vec<String>,
        wrap_off: bool,
        refresh_interval_ms: u64,
    }
    let control = BareState {
        active_profile: Some("work".to_string()),
        profiles: vec!["work".to_string(), "play".to_string()],
        fallback_chain: vec!["work".to_string()],
        refresh_interval_ms: 90_000,
        ..Default::default()
    };
    assert_eq!(
        rendered,
        toml::to_string_pretty(&control).expect("render control"),
        "ProfileName AppState must serialize byte-identically to a String one"
    );
}

// ── AUTH-1: `auth_broken` quarantine set semantics + persistence ──────────────

// `set_auth_broken` returns whether the set actually changed — the transition
// signal `mark_auth_broken` keys its single stderr line off of. Both directions
// flip once and then no-op.
#[test]
fn set_auth_broken_reports_transitions_and_is_idempotent() {
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    assert!(
        config.set_auth_broken("x", true),
        "clear→broken is a transition"
    );
    assert!(config.is_auth_broken("x"));
    assert!(
        !config.set_auth_broken("x", true),
        "broken→broken is a no-op (no duplicate log)"
    );
    assert!(
        config.set_auth_broken("x", false),
        "broken→clear is a transition"
    );
    assert!(!config.is_auth_broken("x"));
    assert!(
        !config.set_auth_broken("x", false),
        "clear→clear is a no-op"
    );
}

// A quarantined account must survive a save/load of profiles.toml, and an older
// state file written before the field existed must still load (serde default →
// empty), or upgrading would either forget a dead login or fail to parse.
#[test]
fn auth_broken_round_trips_and_is_omitted_when_empty() {
    let on = AppState {
        auth_broken: vec!["dead".into()],
        ..AppState::default()
    };
    let rendered = toml::to_string_pretty(&on).expect("render quarantined state");
    assert!(
        rendered.contains("auth_broken"),
        "a populated quarantine must render, got:\n{rendered}"
    );
    let reparsed: AppState = toml::from_str(&rendered).expect("reparse quarantined state");
    assert_eq!(
        reparsed
            .auth_broken
            .iter()
            .map(ProfileName::as_str)
            .collect::<Vec<_>>(),
        ["dead"],
        "the quarantined name survives the round-trip"
    );

    let rendered_off = toml::to_string_pretty(&AppState::default()).expect("render default state");
    assert!(
        !rendered_off.contains("auth_broken"),
        "an empty quarantine is omitted from disk, got:\n{rendered_off}"
    );

    let older: AppState = toml::from_str("profiles = []\n").expect("parse pre-field state");
    assert!(
        older.auth_broken.is_empty(),
        "a state file without the field defaults to an empty quarantine"
    );
}

// `remove` must drop the removed name from the quarantine list too — a stale
// entry would otherwise linger and could re-attach to a re-created same-name
// profile.
#[test]
fn remove_drops_auth_broken_entry() {
    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["a".into(), "b".into()],
            ..AppState::default()
        },
        profiles: vec![
            Profile::new("a".to_string(), None, None),
            Profile::new("b".to_string(), None, None),
        ],
    };
    config.set_auth_broken("a", true);
    config.set_auth_broken("b", true);
    config.remove("a");
    assert!(
        !config.is_auth_broken("a"),
        "removed name leaves the quarantine"
    );
    assert!(
        config.is_auth_broken("b"),
        "the other quarantine is untouched"
    );
}

// `rename_all_occurrences` must carry the quarantine to the new name — a rename
// that dropped it would silently un-quarantine a dead login.
#[test]
fn rename_carries_auth_broken_entry() {
    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["old".into()],
            ..AppState::default()
        },
        profiles: vec![Profile::new("old".to_string(), None, None)],
    };
    config.set_auth_broken("old", true);
    config.rename_all_occurrences("old", "new");
    assert!(
        !config.is_auth_broken("old"),
        "old name no longer quarantined"
    );
    assert!(
        config.is_auth_broken("new"),
        "quarantine follows the rename"
    );
}

#[cfg(unix)]
use crate::testutil::HomeSandbox;

#[cfg(unix)]
fn oauth_credentials() -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "tok-access".to_string(),
            refresh_token: Some("tok-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

/// Out-of-band per-profile thresholds are CLAMPED to the band at load, while the
/// app-level weekly line RESETS TO DEFAULT (pinned separately by
/// `weekly_switch_threshold_out_of_band_resets_to_default_at_load`). Two
/// deliberately different normalizations, one line apart in `docs/internals.md`
/// and one line apart in the source — exactly the shape a well-meaning "unify
/// the threshold handling" refactor collapses into one rule, silently moving
/// every hand-edited config to the wrong value. A garbage `fallback_threshold`
/// left raw would also drive the auto-switch walk off a nonsense line, so the
/// clamp is load-bearing rather than cosmetic. Both fields, both directions.
#[cfg(unix)]
#[test]
fn out_of_band_per_profile_thresholds_clamp_to_the_band_at_load() {
    let _home = HomeSandbox::new();
    let name = "clamp-test";
    save_profile(&crate::testutil::blank_profile(name)).expect("save_profile");

    // Hand-edit the per-profile config the way a user would.
    let config_path = profile_subpath(name, "config.toml").expect("config path");
    std::fs::write(
        &config_path,
        "fallback_threshold = 250.0\nbell_threshold = -30.0\n",
    )
    .expect("write config.toml");

    let loaded = load_profile(name).expect("load_profile");
    assert_eq!(
        loaded.fallback_threshold,
        Some(100.0),
        "an over-band fallback_threshold clamps to the top of the band, it does not \
         reset to default and is never left raw",
    );
    assert_eq!(
        loaded.bell_threshold,
        Some(0.0),
        "an under-band bell_threshold clamps to the bottom of the band",
    );

    // In-band values are untouched — the clamp must not round or default them.
    std::fs::write(
        &config_path,
        "fallback_threshold = 73.5\nbell_threshold = 12.0\n",
    )
    .expect("rewrite config.toml");
    let loaded = load_profile(name).expect("load_profile");
    assert_eq!(loaded.fallback_threshold, Some(73.5));
    assert_eq!(loaded.bell_threshold, Some(12.0));
}

/// The Rust field is `switch_off_when_spent`; the ON-DISK key must stay
/// `wrap_off`. Nothing else pins this: `status.json`'s contract test covers its
/// own key, and every round-trip test goes through serde in both directions, so
/// a rename of the serde name passes them all while silently resetting the
/// setting to `false` in every profiles.toml already on disk. A blind
/// find-and-replace across the field name did exactly that (2026-07-17), which
/// is what this test exists to catch.
#[test]
fn switch_off_when_spent_keeps_its_wrap_off_key_on_disk() {
    let from_disk: AppState = toml::from_str("profiles = []\nwrap_off = true\n")
        .expect("the legacy key must still parse");
    assert!(
        from_disk.switch_off_when_spent,
        "an existing profiles.toml's `wrap_off = true` must survive the rename"
    );

    let rendered = toml::to_string(&AppState {
        switch_off_when_spent: true,
        ..AppState::default()
    })
    .expect("serialize");
    assert!(
        rendered.contains("wrap_off = true"),
        "writes must keep the published key, else an older clauth reads the file \
         and silently loses the setting: {rendered}"
    );
    assert!(
        !rendered.contains("switch_off_when_spent"),
        "the Rust name must not reach disk: {rendered}"
    );
}

/// `max_auto_spend` is a dollar ceiling on unattended spending, so its load
/// normalization is a money guard, not a tidy-up. `inf` and `nan` are both
/// valid TOML floats: left raw, an infinite ceiling means an account with no
/// declared cap has infinite room (`fallback::spend_room`), i.e. unbounded
/// spending from one hand-edited word. Anything non-finite reads as the
/// never-spend default instead.
#[cfg(unix)]
#[test]
fn non_finite_max_auto_spend_reads_as_zero_at_load() {
    let _home = HomeSandbox::new();
    let name = "spend-ceiling-test";
    save_profile(&crate::testutil::blank_profile(name)).expect("save_profile");
    let config_path = profile_subpath(name, "config.toml").expect("config path");

    for raw in ["max_auto_spend = inf\n", "max_auto_spend = nan\n"] {
        std::fs::write(&config_path, raw).expect("write config.toml");
        assert_eq!(
            load_profile(name).expect("load_profile").max_auto_spend,
            Some(0.0),
            "{raw:?} must not survive the load boundary as a spendable ceiling"
        );
    }

    // A negative ceiling floors at $0 rather than staying raw...
    std::fs::write(&config_path, "max_auto_spend = -5.0\n").expect("write config.toml");
    assert_eq!(
        load_profile(name).expect("load_profile").max_auto_spend,
        Some(0.0)
    );

    // ...and an ordinary ceiling is passed through untouched.
    std::fs::write(&config_path, "max_auto_spend = 12.5\n").expect("write config.toml");
    assert_eq!(
        load_profile(name).expect("load_profile").max_auto_spend,
        Some(12.5)
    );
}

// ── crash-durable rotation: the pending sidecar's adopt/discard decision ─────
//
// `stage_rotated_credentials` writes a rotated pair to `credentials.json.pending`
// BEFORE `save_profile`, so a crash between the OAuth response and the commit
// can't lose a single-use refresh token (`docs/internals.md`, crash-durable
// rotation). That guarantee reduces to ONE mtime compare in
// `recover_pending_credentials`, and until now only the sidecar's file *mode* was
// tested — never the decision. Both ways of getting it wrong are silent and
// unrecoverable: adopt too eagerly and a clean commit is overwritten by the pair
// it already superseded (a spent token reinstalled, next refresh 400s), discard
// too eagerly and a genuinely orphaned rotation is dropped (that pair is gone
// and the account needs a manual re-login). Each arm below is one of those.

#[cfg(unix)]
fn pair(access: &str, refresh: &str) -> ClaudeCredentials {
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

#[cfg(unix)]
fn refresh_token_of(creds: &Option<ClaudeCredentials>) -> Option<&str> {
    creds
        .as_ref()?
        .claude_ai_oauth
        .as_ref()?
        .refresh_token
        .as_deref()
}

#[cfg(unix)]
fn seed_committed(name: &str, creds: &ClaudeCredentials) {
    let mut profile = crate::testutil::blank_profile(name);
    profile.credentials = Some(creds.clone());
    save_profile(&profile).expect("save_profile");
}

/// Sidecar NEWER than `credentials.json`: the rotation was staged but the commit
/// never landed, so the staged pair is the only live one — adopt it, write it
/// through to `credentials.json`, and consume the sidecar.
#[cfg(unix)]
#[test]
fn pending_sidecar_newer_than_the_commit_is_adopted_and_written_through() {
    let _home = HomeSandbox::new();
    let name = "pending-adopt-newer";
    let committed = pair("old-access", "old-refresh");
    seed_committed(name, &committed);

    let staged = pair("new-access", "new-refresh");
    stage_rotated_credentials(name, &staged).expect("stage_rotated_credentials");

    let cred_path = profile_subpath(name, "credentials.json").expect("cred path");
    let pending_path = profile_subpath(name, "credentials.json.pending").expect("pending path");
    let now = std::time::SystemTime::now();
    crate::testutil::set_mtime(&cred_path, now - std::time::Duration::from_secs(60));
    crate::testutil::set_mtime(&pending_path, now);

    let got = recover_pending_credentials(name, Some(committed.clone()));
    assert_eq!(
        refresh_token_of(&got),
        Some("new-refresh"),
        "a rotation staged after the last commit is the live pair and must be adopted",
    );

    // Written through, so the next load sees it even without the sidecar.
    let on_disk: ClaudeCredentials = read_json_file(&cred_path).expect("re-read credentials.json");
    assert_eq!(
        on_disk
            .claude_ai_oauth
            .and_then(|o| o.refresh_token)
            .as_deref(),
        Some("new-refresh"),
        "the adopted pair must be committed to credentials.json, not just returned",
    );
    assert!(
        !pending_path.exists(),
        "the sidecar must be consumed so the next load can't adopt it a second time",
    );
}

/// Sidecar OLDER than `credentials.json`: the commit landed cleanly and the
/// sidecar is its already-superseded predecessor. Adopting it would reinstall a
/// spent refresh token, so it must be discarded — and still cleaned up.
#[cfg(unix)]
#[test]
fn pending_sidecar_older_than_the_commit_is_discarded_not_reinstalled() {
    let _home = HomeSandbox::new();
    let name = "pending-discard-older";
    let committed = pair("live-access", "live-refresh");
    seed_committed(name, &committed);

    let superseded = pair("spent-access", "spent-refresh");
    stage_rotated_credentials(name, &superseded).expect("stage_rotated_credentials");

    let cred_path = profile_subpath(name, "credentials.json").expect("cred path");
    let pending_path = profile_subpath(name, "credentials.json.pending").expect("pending path");
    let now = std::time::SystemTime::now();
    crate::testutil::set_mtime(&pending_path, now - std::time::Duration::from_secs(60));
    crate::testutil::set_mtime(&cred_path, now);

    let got = recover_pending_credentials(name, Some(committed.clone()));
    assert_eq!(
        refresh_token_of(&got),
        Some("live-refresh"),
        "a commit newer than the sidecar already won; reinstalling the sidecar would \
         resurrect a spent refresh token",
    );

    let on_disk: ClaudeCredentials = read_json_file(&cred_path).expect("re-read credentials.json");
    assert_eq!(
        on_disk
            .claude_ai_oauth
            .and_then(|o| o.refresh_token)
            .as_deref(),
        Some("live-refresh"),
        "a discarded sidecar must not touch credentials.json",
    );
    assert!(
        !pending_path.exists(),
        "even a discarded sidecar is cleaned up, or it is re-evaluated on every load",
    );
}

/// The boundary is `>=`, not `>`: equal mtimes adopt. Staging and committing
/// within one filesystem timestamp tick is the common case on a coarse-grained
/// mtime, and treating that as "the commit won" would drop a rotation that may
/// never have landed.
#[cfg(unix)]
#[test]
fn pending_sidecar_with_an_equal_mtime_is_adopted() {
    let _home = HomeSandbox::new();
    let name = "pending-adopt-equal";
    let committed = pair("old-access", "old-refresh");
    seed_committed(name, &committed);

    let staged = pair("tie-access", "tie-refresh");
    stage_rotated_credentials(name, &staged).expect("stage_rotated_credentials");

    let cred_path = profile_subpath(name, "credentials.json").expect("cred path");
    let pending_path = profile_subpath(name, "credentials.json.pending").expect("pending path");
    let same = std::time::SystemTime::now();
    crate::testutil::set_mtime(&cred_path, same);
    crate::testutil::set_mtime(&pending_path, same);

    assert_eq!(
        refresh_token_of(&recover_pending_credentials(name, Some(committed))),
        Some("tie-refresh"),
        "an equal mtime must adopt: the compare is `pending >= committed`",
    );
}

/// No `credentials.json` at all (the crash landed between staging and the first
/// commit): there is nothing to compare against and the sidecar is the only pair
/// in existence — adopt unconditionally rather than treating the missing file as
/// a reason to discard.
#[cfg(unix)]
#[test]
fn pending_sidecar_is_adopted_when_no_commit_exists_at_all() {
    let _home = HomeSandbox::new();
    let name = "pending-adopt-absent";
    // Seed the profile dir without credentials so only the sidecar exists.
    save_profile(&crate::testutil::blank_profile(name)).expect("save_profile");

    let staged = pair("only-access", "only-refresh");
    stage_rotated_credentials(name, &staged).expect("stage_rotated_credentials");
    let cred_path = profile_subpath(name, "credentials.json").expect("cred path");
    assert!(
        !cred_path.exists(),
        "precondition: no committed credentials"
    );

    assert_eq!(
        refresh_token_of(&recover_pending_credentials(name, None)),
        Some("only-refresh"),
        "with no commit to compare against, the staged pair is the only live one",
    );
}

/// `scopes_joined` feeds the refresh `scope` field (Claude Code echoes its
/// credential's granted scopes on refresh). Order must survive and an empty set
/// must read as `None` so the refresh path falls back instead of sending `""`.
#[test]
fn scopes_joined_space_joins_preserving_order_and_maps_empty_to_none() {
    let creds = |scopes: Option<Vec<String>>| ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at".to_string(),
            refresh_token: Some("rt".to_string()),
            expires_at: None,
            scopes,
            subscription_type: None,
        }),
    };
    assert_eq!(
        creds(Some(vec!["user:profile".into(), "user:inference".into()])).scopes_joined(),
        Some("user:profile user:inference".to_string())
    );
    assert_eq!(creds(Some(Vec::new())).scopes_joined(), None);
    assert_eq!(creds(None).scopes_joined(), None);
    assert_eq!(
        ClaudeCredentials {
            claude_ai_oauth: None
        }
        .scopes_joined(),
        None
    );
}

/// credentials.json, its `.pending` rotation sidecar, and the per-profile dir
/// must carry tightened permissions: 0o600 files, 0o700 dir.
#[cfg(unix)]
#[test]
fn credential_and_cache_files_have_restricted_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let name = "perm-test-credentials";
    let creds = oauth_credentials();

    let profile = Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: std::collections::BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        last_resort: false,
        max_auto_spend: None,
        bell_threshold: None,
        credentials: Some(creds.clone()),
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    };
    // Goes through ConfigHandle-equivalent path: save_profile takes the state
    // flock (rank-ordered) and writes credentials.json before config.toml.
    save_profile(&profile).expect("save_profile");

    let dir_mode = std::fs::metadata(profile_dir(name).expect("profile_dir"))
        .expect("dir metadata")
        .permissions()
        .mode();
    assert_eq!(
        dir_mode & 0o777,
        0o700,
        "profile dir mode should be 0o700, got {:#o}",
        dir_mode & 0o777,
    );

    let cred_path = profile_subpath(name, "credentials.json").expect("cred path");
    let cred_mode = std::fs::metadata(&cred_path)
        .expect("credentials.json metadata")
        .permissions()
        .mode();
    assert_eq!(
        cred_mode & 0o777,
        0o600,
        "credentials.json mode should be 0o600, got {:#o}",
        cred_mode & 0o777,
    );

    // Stage the rotation sidecar and assert its mode too.
    stage_rotated_credentials(name, &creds).expect("stage_rotated_credentials");
    let pending_path = profile_subpath(name, "credentials.json.pending").expect("pending path");
    let pending_mode = std::fs::metadata(&pending_path)
        .expect("credentials.json.pending metadata")
        .permissions()
        .mode();
    assert_eq!(
        pending_mode & 0o777,
        0o600,
        "credentials.json.pending mode should be 0o600, got {:#o}",
        pending_mode & 0o777,
    );

    // profiles.toml goes through the same `atomic_write_600` and names every
    // account plus the active one; it was the one state file this test never
    // covered, so a writer swapped back to a plain `fs::write` would land it at
    // the process umask (world-readable on a default 022) with nothing failing.
    save_app_state(&AppState::default()).expect("save_app_state");
    let state_mode = std::fs::metadata(app_state_path().expect("app_state_path"))
        .expect("profiles.toml metadata")
        .permissions()
        .mode();
    assert_eq!(
        state_mode & 0o777,
        0o600,
        "profiles.toml mode should be 0o600, got {:#o}",
        state_mode & 0o777,
    );
}

/// The real usage-cache writer (`profile_cache::write_profile_cache`) must
/// create usage_cache.json at 0o600 and, when it has to create the per-profile
/// dir, that dir at 0o700. Driven on a FRESH profile name so the dir does not
/// pre-exist.
#[cfg(unix)]
#[test]
fn usage_cache_write_creates_restricted_file_and_dir() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let name = "perm-test-usage-cache";

    // Fresh profile: its dir must not exist before the cache write.
    let dir = profile_dir(name).expect("profile_dir");
    assert!(
        !dir.exists(),
        "precondition: profile dir must not pre-exist for a fresh profile"
    );

    // Drive the actual production writer.
    let info = crate::usage::UsageInfo::default();
    crate::profile_cache::write_profile_cache(name, crate::profile_cache::USAGE_CACHE_FILE, &info);

    let dir_mode = std::fs::metadata(&dir)
        .expect("freshly-created profile dir metadata")
        .permissions()
        .mode();
    assert_eq!(
        dir_mode & 0o777,
        0o700,
        "freshly-created profile dir mode should be 0o700, got {:#o}",
        dir_mode & 0o777,
    );

    let cache_path = profile_subpath(name, "usage_cache.json").expect("cache path");
    let cache_mode = std::fs::metadata(&cache_path)
        .expect("usage_cache.json metadata")
        .permissions()
        .mode();
    assert_eq!(
        cache_mode & 0o777,
        0o600,
        "usage_cache.json mode should be 0o600, got {:#o}",
        cache_mode & 0o777,
    );
}

/// Installs from before the 0o600/0o700 rule carry a umask-moded tree that no
/// writer ever revisits: bytes that never change keep their mode forever. Every
/// entry point loads the config, so that is where the tree gets retightened.
#[cfg(unix)]
#[test]
fn load_config_repairs_a_loose_clauth_tree() {
    use crate::testutil::owner_only_violations;
    use std::os::unix::fs::PermissionsExt;

    let home = HomeSandbox::new();
    let name = "perm-test-repair";
    save_profile(&crate::testutil::blank_profile(name)).expect("save_profile");
    save_app_state(&AppState {
        profiles: vec![name.into()],
        ..Default::default()
    })
    .expect("save_app_state");

    let clauth = clauth_dir().expect("clauth_dir");
    let profile = profile_dir(name).expect("profile_dir");
    let runtime = profile.join("runtime");
    let sessions = profile.join("sessions");
    std::fs::create_dir_all(&runtime).expect("mkdir runtime");
    std::fs::create_dir_all(&sessions).expect("mkdir sessions");
    std::fs::write(runtime.join("settings.json"), b"{}").expect("write settings");
    std::fs::write(profile.join("usage_history.jsonl"), b"").expect("write history");

    // What an older build left behind: umask modes top to bottom.
    for dir in [&clauth, &clauth.join("profiles"), &profile, &runtime] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).expect("chmod dir");
    }
    for file in [
        profile.join("config.toml"),
        profile.join("usage_history.jsonl"),
        runtime.join("settings.json"),
    ] {
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644))
            .expect("chmod file");
    }

    // A runtime links into the operator's ~/.claude, and `set_permissions`
    // resolves links — walking one would chmod a file clauth does not own.
    let outside = home.home().join("outside.json");
    std::fs::write(&outside, b"{}").expect("write outside");
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    std::os::unix::fs::symlink(&outside, runtime.join("CLAUDE.md")).expect("symlink");

    load_config().expect("load_config");

    let left = owner_only_violations(&clauth);
    assert!(
        left.is_empty(),
        "load_config must leave the whole ~/.clauth tree owner-only; still loose: {left:#?}"
    );
    let outside_mode = std::fs::metadata(&outside)
        .expect("outside metadata")
        .permissions()
        .mode();
    assert_eq!(
        outside_mode & 0o777,
        0o644,
        "the repair followed a symlink out of the tree and chmodded {:#o} onto a file clauth does not own",
        outside_mode & 0o777,
    );
}

#[test]
fn profile_config_reads_models_table() {
    let toml = "[models]\n\
        default = \"opusplan\"\n\
        haiku = \"claude-haiku-4-5\"\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse models table");
    assert_eq!(cfg.models.default.as_deref(), Some("opusplan"));
    assert_eq!(cfg.models.haiku.as_deref(), Some("claude-haiku-4-5"));
    assert_eq!(cfg.models.sonnet, None);
}

// Model config must survive a config.toml render→parse round-trip, or
// `maybe_rewrite_config_toml` would either drop a hand-set value or thrash the
// file on every reload.
#[test]
fn model_settings_round_trip_through_config_toml() {
    let mut profile = Profile::new("p".to_string(), None, None);
    profile.models = ModelSettings {
        default: Some("opusplan".to_string()),
        opus: Some("claude-opus-4-8[1m]".to_string()),
        sonnet: None,
        haiku: None,
        subagent: Some("claude-haiku-4-5".to_string()),
    };
    let rendered = render_config_toml(&profile);
    let parsed: ProfileConfig = toml::from_str(&rendered).expect("parse rendered toml");
    assert_eq!(parsed.models, profile.models);
}

/// Write `profiles.toml` into the sandboxed home and read it back through the
/// real load boundary — the point of these tests is where normalization
/// happens, so nothing may bypass `load_app_state`.
fn load_state_from_toml(toml: &str) -> AppState {
    std::fs::create_dir_all(clauth_dir().expect("clauth dir")).expect("create clauth dir");
    std::fs::write(app_state_path().expect("state path"), toml).expect("write profiles.toml");
    load_app_state().expect("load state")
}

// A hand-edited out-of-band line must be normalized on LOAD, not on read
// alone: left raw on disk it survives every save and any direct field read
// trusts it. The reset target is the DEFAULT, never the nearest bound —
// honoring a hand-edited 40.0 as 50.0 keeps the weakened gate the edit asked
// for, so fail-safe high instead.
#[test]
fn weekly_switch_threshold_out_of_band_resets_to_default_at_load() {
    let _home = crate::testutil::HomeSandbox::new();
    let low = load_state_from_toml("profiles = []\nweekly_switch_threshold = 40.0\n");
    assert_eq!(
        low.weekly_switch_threshold,
        Some(DEFAULT_WEEKLY_SWITCH_PCT),
        "40.0 resets to the default, never clamps up to MIN"
    );
    let high = load_state_from_toml("profiles = []\nweekly_switch_threshold = 150.0\n");
    assert_eq!(
        high.weekly_switch_threshold,
        Some(DEFAULT_WEEKLY_SWITCH_PCT),
        "150.0 resets to the default, never clamps down to MAX"
    );
}

#[test]
fn weekly_switch_threshold_in_band_survives_load() {
    let _home = crate::testutil::HomeSandbox::new();
    let state = load_state_from_toml("profiles = []\nweekly_switch_threshold = 75.0\n");
    assert_eq!(state.weekly_switch_threshold, Some(75.0));
}

#[test]
fn weekly_switch_threshold_absent_loads_as_default() {
    let _home = crate::testutil::HomeSandbox::new();
    let state = load_state_from_toml("profiles = []\n");
    // Unset stays unset: materializing a value here would start writing the
    // key into every state file that never had it (`skip_serializing_if`).
    assert_eq!(state.weekly_switch_threshold, None);
    assert_eq!(
        state.weekly_switch_threshold_pct(),
        DEFAULT_WEEKLY_SWITCH_PCT
    );
}

// `reload_fingerprint` is the reload trigger for BOTH detectors. These pin the
// three ways it must shift — the profiles.toml mtime (the pre-existing trigger,
// unchanged), a per-account config.toml appearing/vanishing (count), and an
// existing config.toml edited (newest mtime) — plus stability when nothing moved.
#[test]
fn reload_fingerprint_is_stable_with_no_change() {
    let _home = crate::testutil::HomeSandbox::new();
    save_profile(&crate::testutil::blank_profile("p")).expect("save_profile");
    save_app_state(&AppState {
        profiles: vec!["p".into()],
        ..Default::default()
    })
    .expect("save_app_state");
    let first = reload_fingerprint();
    let second = reload_fingerprint();
    assert_eq!(
        first, second,
        "no filesystem change must leave the fingerprint identical"
    );
}

#[test]
fn reload_fingerprint_changes_when_profiles_toml_mtime_bumps() {
    let _home = crate::testutil::HomeSandbox::new();
    save_app_state(&AppState {
        profiles: vec![],
        ..Default::default()
    })
    .expect("save_app_state");
    let before = reload_fingerprint();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
    crate::testutil::set_mtime(&app_state_path().expect("state path"), later);
    let after = reload_fingerprint();
    assert_ne!(
        before, after,
        "a profiles.toml mtime bump must change the fingerprint"
    );
}

#[test]
fn reload_fingerprint_bumps_when_a_config_toml_is_added() {
    let _home = crate::testutil::HomeSandbox::new();
    let bare = profiles_root().expect("profiles_root").join("newcomer");
    std::fs::create_dir_all(&bare).expect("mkdir profile");
    let before = reload_fingerprint();
    assert_eq!(
        before
            .config_mtimes
            .iter()
            .find(|(n, _)| n == "newcomer")
            .map(|(_, m)| m.is_some()),
        Some(false),
        "the dir exists but has no config.toml yet"
    );
    std::fs::write(bare.join("config.toml"), b"auto_start = true\n").expect("write config");
    let after = reload_fingerprint();
    assert_eq!(
        after
            .config_mtimes
            .iter()
            .find(|(n, _)| n == "newcomer")
            .map(|(_, m)| m.is_some()),
        Some(true),
        "adding a config.toml gives the entry an mtime"
    );
    assert_ne!(before, after);
}

#[test]
fn reload_fingerprint_advances_when_a_config_toml_is_edited() {
    let _home = crate::testutil::HomeSandbox::new();
    save_profile(&crate::testutil::blank_profile("p")).expect("save_profile");
    let cfg = profile_dir("p").expect("profile_dir").join("config.toml");
    let before = reload_fingerprint();
    let before_mtime = before
        .config_mtimes
        .iter()
        .find(|(n, _)| n == "p")
        .and_then(|(_, m)| *m);
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(30);
    crate::testutil::set_mtime(&cfg, later);
    let after = reload_fingerprint();
    let after_mtime = after
        .config_mtimes
        .iter()
        .find(|(n, _)| n == "p")
        .and_then(|(_, m)| *m);
    assert!(
        after_mtime > before_mtime,
        "editing a config.toml must advance its recorded mtime"
    );
    assert_ne!(
        before, after,
        "a config.toml edit must change the fingerprint"
    );
}

#[test]
fn reload_fingerprint_drops_when_a_config_toml_is_removed() {
    let _home = crate::testutil::HomeSandbox::new();
    save_profile(&crate::testutil::blank_profile("p")).expect("save_profile");
    let cfg = profile_dir("p").expect("profile_dir").join("config.toml");
    let before = reload_fingerprint();
    assert!(
        before
            .config_mtimes
            .iter()
            .any(|(n, m)| n == "p" && m.is_some()),
        "the saved profile has a config.toml"
    );
    std::fs::remove_file(&cfg).expect("remove config");
    let after = reload_fingerprint();
    assert!(
        after
            .config_mtimes
            .iter()
            .any(|(n, m)| n == "p" && m.is_none()),
        "removing the config.toml drops its recorded mtime to None"
    );
    assert_ne!(before, after);
}

/// Regression: an edit to a config.toml that is NOT the newest one — its mtime
/// stays below another profile's — must still flip the fingerprint. A max-only
/// fingerprint (count + newest mtime) would miss this (max unchanged, count
/// unchanged, profiles.toml unchanged), silently reintroducing the very
/// "config edit not detected" bug this feature exists to fix.
#[test]
fn reload_fingerprint_catches_a_non_newest_config_edit() {
    let _home = crate::testutil::HomeSandbox::new();
    save_profile(&crate::testutil::blank_profile("a")).expect("save a");
    save_profile(&crate::testutil::blank_profile("b")).expect("save b");
    let cfg_a = profile_dir("a").expect("profile_dir a").join("config.toml");
    let cfg_b = profile_dir("b").expect("profile_dir b").join("config.toml");
    let base = std::time::SystemTime::now();
    // b stays the newest throughout; a is edited but kept below b.
    crate::testutil::set_mtime(&cfg_b, base + std::time::Duration::from_secs(100));
    crate::testutil::set_mtime(&cfg_a, base + std::time::Duration::from_secs(10));
    let before = reload_fingerprint();
    crate::testutil::set_mtime(&cfg_a, base + std::time::Duration::from_secs(50));
    let after = reload_fingerprint();
    assert_ne!(
        before, after,
        "an edit to a non-newest config.toml must still flip the fingerprint"
    );
}

// The burn-aware tunable accessors reset a hand-edited out-of-band value to the
// default (fail-safe, like the weekly line) and keep an in-band one. An unset
// field reads as the default so `skip_serializing_if` keeps omitting it.
#[test]
fn burn_switch_floor_pct_resets_out_of_band_and_keeps_in_band() {
    let mut st = AppState::default();
    assert_eq!(st.burn_switch_floor_pct(), DEFAULT_BURN_FLOOR_PCT);

    st.burn_switch_floor_pct = Some(MIN_BURN_FLOOR_PCT - 1.0);
    assert_eq!(
        st.burn_switch_floor_pct(),
        DEFAULT_BURN_FLOOR_PCT,
        "below-band floor resets to the default, not clamped to the bound"
    );
    st.burn_switch_floor_pct = Some(99.0);
    assert_eq!(st.burn_switch_floor_pct(), 99.0);
}

#[test]
fn burn_horizon_cap_ms_resets_out_of_band_and_keeps_in_band() {
    let mut st = AppState::default();
    assert_eq!(st.burn_horizon_cap_ms(), DEFAULT_BURN_HORIZON_MS);

    st.burn_horizon_cap_ms = Some(MIN_REFRESH_INTERVAL_MS - 1);
    assert_eq!(st.burn_horizon_cap_ms(), DEFAULT_BURN_HORIZON_MS);
    st.burn_horizon_cap_ms = Some(45_000);
    assert_eq!(st.burn_horizon_cap_ms(), 45_000);
}

#[test]
fn burn_tunables_round_trip_and_omit_when_unset() {
    let on = AppState {
        burn_switch_floor_pct: Some(99.0),
        burn_horizon_cap_ms: Some(45_000),
        ..AppState::default()
    };
    let rendered = toml::to_string_pretty(&on).expect("render");
    let reparsed: AppState = toml::from_str(&rendered).expect("reparse");
    assert_eq!(reparsed.burn_switch_floor_pct, Some(99.0));
    assert_eq!(reparsed.burn_horizon_cap_ms, Some(45_000));

    let off = toml::to_string_pretty(&AppState::default()).expect("render default");
    assert!(
        !off.contains("burn_switch_floor_pct") && !off.contains("burn_horizon_cap_ms"),
        "unset burn tunables must be omitted, got:\n{off}"
    );
}
