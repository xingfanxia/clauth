//! `clauth login`'s hand-rolled arg parsing. `parse_login_args` is a pure
//! shape check, so it's exercised directly; `dispatch`'s success arm for a
//! *valid* `--model` shape calls `cmd_login`, which spawns a real `claude`
//! process via `start::run` and is never exercised here. Model persistence
//! itself is covered in `tests/inline/actions.rs` (`set_profile_default_model`).

use super::*;

fn login_args<'a>(
    name: &'a str,
    model: Option<&'a str>,
    base_url: Option<&'a str>,
    api_key: Option<&'a str>,
) -> Option<LoginArgs<'a>> {
    Some(LoginArgs {
        name,
        model,
        base_url,
        api_key,
        setup_token: false,
        yes: false,
    })
}

#[test]
fn parse_login_args_setup_token_flag() {
    let args = ["acme".to_string(), "--setup-token".to_string()];
    let parsed = parse_login_args(&args).expect("valid shape");
    assert!(parsed.setup_token);
    assert!(!parsed.yes);
    assert_eq!(parsed.name, "acme");

    let args = [
        "acme".to_string(),
        "--setup-token".to_string(),
        "--yes".to_string(),
    ];
    assert!(parse_login_args(&args).expect("valid shape").yes);
}

#[test]
fn parse_login_args_setup_token_excludes_api_mode_and_bare_yes() {
    // The sidecar capture and the API-key pair are different logins — the
    // combination is a contradiction, not a preference.
    let args = [
        "acme".to_string(),
        "--setup-token".to_string(),
        "--base-url".to_string(),
        "https://x".to_string(),
    ];
    assert_eq!(parse_login_args(&args), None);
    // `--yes` means nothing outside the capture flow.
    let args = ["acme".to_string(), "--yes".to_string()];
    assert_eq!(parse_login_args(&args), None);
}

#[test]
fn parse_login_args_bare_name_has_no_model() {
    let args = ["acme".to_string()];
    assert_eq!(
        parse_login_args(&args),
        login_args("acme", None, None, None)
    );
}

#[test]
fn parse_login_args_accepts_model_flag() {
    let args = [
        "acme".to_string(),
        "--model".to_string(),
        "opus".to_string(),
    ];
    assert_eq!(
        parse_login_args(&args),
        login_args("acme", Some("opus"), None, None)
    );
}

#[test]
fn parse_login_args_accepts_a_full_custom_model_id() {
    let args = [
        "acme".to_string(),
        "--model".to_string(),
        "claude-opus-4-8".to_string(),
    ];
    assert_eq!(
        parse_login_args(&args),
        login_args("acme", Some("claude-opus-4-8"), None, None)
    );
}

#[test]
fn parse_login_args_model_flag_without_value_is_none() {
    let args = ["acme".to_string(), "--model".to_string()];
    assert_eq!(parse_login_args(&args), None);
}

#[test]
fn parse_login_args_rejects_flag_shaped_profile_names() {
    // `clauth login --model` (value forgotten, name missing) must bail with
    // usage instead of creating a profile literally named "--model".
    assert_eq!(parse_login_args(&["--model".to_string()]), None);
    let flag_value = [
        "--model".to_string(),
        "--model".to_string(),
        "opus".to_string(),
    ];
    assert_eq!(parse_login_args(&flag_value), None);
}

#[test]
fn parse_login_args_rejects_unrecognized_flag() {
    let args = ["acme".to_string(), "--bogus".to_string(), "x".to_string()];
    assert_eq!(parse_login_args(&args), None);
}

#[test]
fn parse_login_args_rejects_empty_and_trailing_extra_args() {
    assert_eq!(parse_login_args(&[]), None);
    let extra = [
        "acme".to_string(),
        "--model".to_string(),
        "opus".to_string(),
        "extra".to_string(),
    ];
    assert_eq!(parse_login_args(&extra), None);
}

// ── API-key mode: --base-url/--api-key select it, in any order ──

#[test]
fn parse_login_args_api_mode_both_endpoint_flags() {
    let args = [
        "acme".to_string(),
        "--base-url".to_string(),
        "https://api.deepseek.com".to_string(),
        "--api-key".to_string(),
        "sk-x".to_string(),
    ];
    let parsed = parse_login_args(&args);
    assert_eq!(
        parsed,
        login_args("acme", None, Some("https://api.deepseek.com"), Some("sk-x"))
    );
    assert!(parsed.unwrap().is_api_mode());
}

#[test]
fn parse_login_args_api_mode_one_flag_leaves_the_other_for_prompt() {
    // Only --api-key: base_url stays None (prompted at runtime).
    let args = [
        "acme".to_string(),
        "--api-key".to_string(),
        "sk-x".to_string(),
    ];
    let parsed = parse_login_args(&args).expect("api-key flag parses");
    assert_eq!(parsed.name, "acme");
    assert_eq!(parsed.base_url, None);
    assert_eq!(parsed.api_key, Some("sk-x"));
    assert!(parsed.is_api_mode());
}

#[test]
fn parse_login_args_api_mode_flags_in_any_order_with_model() {
    let args = [
        "deepseek".to_string(),
        "--api-key".to_string(),
        "sk-x".to_string(),
        "--model".to_string(),
        "deepseek-chat".to_string(),
        "--base-url".to_string(),
        "https://api.deepseek.com".to_string(),
    ];
    assert_eq!(
        parse_login_args(&args),
        login_args(
            "deepseek",
            Some("deepseek-chat"),
            Some("https://api.deepseek.com"),
            Some("sk-x")
        )
    );
}

#[test]
fn parse_login_args_api_mode_flag_without_value_is_none() {
    assert_eq!(
        parse_login_args(&["acme".to_string(), "--base-url".to_string()]),
        None
    );
    assert_eq!(
        parse_login_args(&["acme".to_string(), "--api-key".to_string()]),
        None
    );
}

#[test]
fn parse_login_args_api_mode_rejects_flag_as_value() {
    // `--base-url --api-key` is a forgotten base-url value, not base_url="--api-key".
    let args = [
        "acme".to_string(),
        "--base-url".to_string(),
        "--api-key".to_string(),
        "sk-x".to_string(),
    ];
    assert_eq!(parse_login_args(&args), None);
}

#[test]
fn parse_login_args_bare_name_is_oauth_mode() {
    let args = ["acme".to_string()];
    let parsed = parse_login_args(&args).expect("bare name parses");
    assert!(!parsed.is_api_mode());
}

// ── collect_api_endpoint: flag values get the prompt's trim + empty-reject ──
// Both flags present means no stdin read, so these run headless.

#[test]
fn collect_api_endpoint_trims_flag_values() {
    let (base, key) = collect_api_endpoint(Some("  https://api.x  "), Some("  sk-y  "))
        .expect("both flags present, no prompt");
    assert_eq!(base.as_deref(), Some("https://api.x"));
    assert_eq!(key.as_deref(), Some("sk-y"));
}

#[test]
fn collect_api_endpoint_rejects_empty_flag_values() {
    assert!(
        collect_api_endpoint(Some("   "), Some("sk")).is_err(),
        "a blank --base-url must bail, not create an empty-endpoint profile"
    );
    assert!(
        collect_api_endpoint(Some("https://x"), Some("")).is_err(),
        "a blank --api-key must bail, not store an empty key"
    );
}

#[test]
fn collect_api_endpoint_rejects_control_chars_in_key() {
    // The key is minted verbatim into a request header; a CRLF would inject one.
    assert!(
        collect_api_endpoint(Some("https://x"), Some("sk-a\r\nX-Evil: 1")).is_err(),
        "a control-char key must bail at capture, not persist a header-injecting value"
    );
    assert!(
        collect_api_endpoint(Some("https://x"), Some("sk a b")).is_err(),
        "interior whitespace in a key is a bad paste"
    );
}

// ── parse_delete_args ──

#[test]
fn parse_delete_args_bare_name_no_yes() {
    assert_eq!(
        parse_delete_args(&["acme".to_string()]),
        Some(("acme", false, false))
    );
}

#[test]
fn parse_delete_args_accepts_yes_and_short_flag_anywhere() {
    assert_eq!(
        parse_delete_args(&["acme".to_string(), "--yes".to_string()]),
        Some(("acme", true, false))
    );
    assert_eq!(
        parse_delete_args(&["-y".to_string(), "acme".to_string()]),
        Some(("acme", true, false))
    );
}

#[test]
fn parse_delete_args_force_is_independent_of_yes() {
    // --force overrides the live-session guard but does NOT skip the confirm.
    assert_eq!(
        parse_delete_args(&["acme".to_string(), "--force".to_string()]),
        Some(("acme", false, true)),
        "--force alone leaves yes unset"
    );
    assert_eq!(
        parse_delete_args(&[
            "--force".to_string(),
            "--yes".to_string(),
            "acme".to_string()
        ]),
        Some(("acme", true, true)),
        "both flags parse together, order-independent"
    );
}

#[test]
fn parse_delete_args_requires_a_name() {
    assert_eq!(parse_delete_args(&[]), None);
    assert_eq!(
        parse_delete_args(&["--yes".to_string()]),
        None,
        "--yes without a name must bail, not delete nothing"
    );
}

#[test]
fn parse_delete_args_rejects_unknown_flag_and_second_name() {
    assert_eq!(
        parse_delete_args(&["acme".to_string(), "--bogus".to_string()]),
        None
    );
    assert_eq!(
        parse_delete_args(&["acme".to_string(), "other".to_string()]),
        None
    );
}

/// End-to-end through `dispatch` for the one login shape that's safe to run
/// without side effects: an invalid arg shape bails before ever reaching
/// `cmd_login`.
#[test]
fn dispatch_login_model_flag_without_value_errors_with_usage() {
    let args = [
        "login".to_string(),
        "somename".to_string(),
        "--model".to_string(),
    ];
    let err = dispatch(&args).expect_err("--model with no value must error");
    assert!(
        err.to_string().contains("usage"),
        "error must be a usage message, got: {err}"
    );
}

// ── login_route: `clauth login <existing>` re-authenticates instead of bailing ──

fn config_with(names: &[&str]) -> crate::profile::AppConfig {
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    for n in names {
        config.add(crate::profile::Profile::new((*n).to_string(), None, None));
    }
    config
}

#[test]
fn login_route_new_name_creates() {
    let config = config_with(&["acme"]);
    assert_eq!(
        login_route(&config, "fresh"),
        LoginRoute::New("fresh".to_string())
    );
}

#[test]
fn login_route_existing_name_reauths() {
    let config = config_with(&["acme"]);
    assert_eq!(
        login_route(&config, "acme"),
        LoginRoute::Reauth("acme".to_string())
    );
}

// A case variant must land on the STORED canonical spelling — otherwise the
// collision validator would bail "already exists" and the reauth path is
// unreachable for anyone who types `ACME` for stored `acme` (the #7 report).
#[test]
fn login_route_case_variant_reauths_canonical_spelling() {
    let config = config_with(&["acme"]);
    assert_eq!(
        login_route(&config, "ACME"),
        LoginRoute::Reauth("acme".to_string())
    );
    assert_eq!(
        login_route(&config, "  acme  "),
        LoginRoute::Reauth("acme".to_string()),
        "surrounding whitespace is trimmed before matching"
    );
}

// The New arm must trim too, symmetric with Reauth: a stored `"  new  "` would
// be unreachable by the no-trim lookups every later command uses.
#[test]
fn login_route_new_name_trims_surrounding_whitespace() {
    let config = config_with(&["acme"]);
    assert_eq!(
        login_route(&config, "  fresh  "),
        LoginRoute::New("fresh".to_string())
    );
}

// Reauth overwrite confirm is default-NO: only an explicit y/yes proceeds.
#[test]
fn reauth_confirmed_only_on_explicit_yes() {
    for yes in ["y", "Y", "yes", "YES", "  y  ", "Yes\n"] {
        assert!(reauth_confirmed(yes), "{yes:?} should confirm");
    }
    for no in ["", "  ", "n", "no", "nope", "\n", "yeah", "ok"] {
        assert!(!reauth_confirmed(no), "{no:?} should decline");
    }
}

// ── parse_start_args ──

fn sv(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

#[test]
fn parse_start_args_bare_name_is_shared_no_override() {
    let args = sv(&["acme"]);
    let a = parse_start_args(&args).expect("bare name parses");
    assert_eq!(a.name, "acme");
    assert_eq!(a.isolation, Isolation::Shared);
    assert_eq!(a.rescue_override, None);
    assert!(a.claude_args.is_empty());
}

#[test]
fn parse_start_args_isolated_flag_precedes_name() {
    let args = sv(&["--isolated", "acme", "-p", "hi"]);
    let a = parse_start_args(&args).expect("isolated + name parses");
    assert_eq!(a.name, "acme");
    assert_eq!(a.isolation, Isolation::Isolated);
    assert_eq!(a.rescue_override, None);
    // Everything after the name is `claude`'s — a passthrough `-p` is not a
    // clauth flag.
    assert_eq!(a.claude_args, ["-p".to_string(), "hi".to_string()]);
}

#[test]
fn parse_start_args_rescue_flags_override_in_any_order() {
    let on = sv(&["--rescue", "--isolated", "acme"]);
    let a = parse_start_args(&on).expect("--rescue before --isolated parses");
    assert_eq!(a.rescue_override, Some(true));
    assert_eq!(a.isolation, Isolation::Isolated);

    let off = sv(&["--isolated", "--no-rescue", "acme"]);
    let b = parse_start_args(&off).expect("--no-rescue after --isolated parses");
    assert_eq!(b.rescue_override, Some(false));
}

#[test]
fn parse_start_args_rescue_requires_isolated() {
    // A rescue flag on a shared start is a user error, not a silent no-op.
    assert_eq!(parse_start_args(&sv(&["--rescue", "acme"])), None);
    assert_eq!(parse_start_args(&sv(&["--no-rescue", "acme"])), None);
}

#[test]
fn parse_start_args_requires_a_name() {
    assert_eq!(parse_start_args(&[]), None);
    assert_eq!(parse_start_args(&sv(&["--isolated"])), None);
    assert_eq!(
        parse_start_args(&sv(&["--isolated", "--rescue"])),
        None,
        "flags without a name must bail"
    );
}

// ── hidden `clauth __api-key <profile>` (CC's apiKeyHelper body) ──────────────
//
// The hidden subcommand is what CC's `apiKeyHelper` runs per request to mint
// an auth value for an api-key profile (see `src/claude.rs`
// `build_claude_settings_json`). It reads the key from `config.toml` and
// prints it to stdout; on a missing profile or a profile with no api_key it
// fails closed with no stdout. The key never reaches argv (the helper command
// line carries only the profile name).

#[cfg(unix)]
mod api_key_helper_tests {
    use super::*;
    use crate::profile::save_profile;
    use crate::testutil::HomeSandbox;

    /// Write a profile to the sandboxed home with the given api_key (or none),
    /// then save it so `load_profile` can read it back the way the helper does.
    fn save_profile_with_key(name: &str, api_key: Option<&str>) {
        let mut profile = crate::testutil::blank_profile(name);
        profile.api_key = api_key.map(str::to_string);
        save_profile(&profile).expect("save_profile");
    }

    /// `api_key_for_profile` returns the stored key verbatim for a profile that
    /// has one — the load path CC's helper relies on each request.
    #[test]
    fn api_key_for_profile_returns_stored_key() {
        let _home = HomeSandbox::new();
        save_profile_with_key("acme", Some("sk-test-12345"));
        let key = api_key_for_profile("acme").expect("load_profile");
        assert_eq!(key.as_deref(), Some("sk-test-12345"));
    }

    /// A profile that exists but has no api_key yields `Ok(None)`, which
    /// `cmd_api_key` turns into an Err (no stdout). This is the fail-closed
    /// path for a misconfigured helper.
    #[test]
    fn api_key_for_profile_none_when_profile_has_no_key() {
        let _home = HomeSandbox::new();
        save_profile_with_key("oauth-profile", None);
        let key = api_key_for_profile("oauth-profile").expect("load_profile");
        assert!(
            key.is_none(),
            "a profile with no api_key must yield Ok(None), not a blank Some"
        );
    }

    /// A missing profile surfaces as `Err`, not `Ok(None)` — so `cmd_api_key`
    /// fails for a helper string pointing at a profile name that no longer
    /// exists, rather than silently minting nothing.
    #[test]
    fn api_key_for_profile_err_for_missing_profile() {
        let _home = HomeSandbox::new();
        let err = api_key_for_profile("no-such-profile").expect_err("missing profile");
        assert!(
            err.to_string().contains("no-such-profile")
                || err.to_string().contains("failed to read"),
            "error must name the missing profile; got: {err}"
        );
    }

    /// A whitespace-only api_key reads as `None`: the helper must fail closed
    /// rather than emit a blank line CC would send as a credential. The trim
    /// also forgives a config.toml hand-edit with a trailing newline inside
    /// the quotes, which serde would otherwise preserve.
    #[test]
    fn api_key_for_profile_treats_blank_key_as_absent() {
        let _home = HomeSandbox::new();
        save_profile_with_key("blank", Some("   "));
        let key = api_key_for_profile("blank").expect("load_profile");
        assert!(key.is_none(), "a whitespace-only key must read as None");
    }

    /// End-to-end through `dispatch`: the helper dispatch arm reaches
    /// `cmd_api_key` for a profile that has a key and returns Ok (CC sees exit
    /// 0; the printed bytes are asserted separately by `write_api_key_*`
    /// below, since stdout can't be captured cleanly from a same-process
    /// `dispatch` call).
    #[test]
    fn dispatch_api_key_helper_returns_ok_for_profile_with_key() {
        let _home = HomeSandbox::new();
        save_profile_with_key("acme", Some("sk-dispatch-xyz"));
        let args = ["__api-key".to_string(), "acme".to_string()];
        dispatch(&args).expect("a profile with a key must exit 0");
    }

    /// `write_api_key` emits the key bytes VERBATIM with no trailing newline
    /// or other framing. CC's `apiKeyHelper` contract does not document
    /// trimming, so a trailing `\n` would be correct only under the unverified
    /// trim assumption; the bare-key form is correct either way. Pinned as a
    /// byte-exact assertion so a regression that reintroduces `println!`-style
    /// framing fails loudly instead of leaking a `\n`-suffixed credential.
    #[test]
    fn write_api_key_emits_no_trailing_newline() {
        let mut buf: Vec<u8> = Vec::new();
        write_api_key(&mut buf, "sk-test-12345").expect("write");
        assert_eq!(
            buf,
            b"sk-test-12345".to_vec(),
            "must emit exactly the key bytes — no newline, no framing"
        );

        // An empty-key call is structurally unreachable (`cmd_api_key` bails
        // before this fn on None), but the writer itself handles it without
        // inventing output.
        let mut empty: Vec<u8> = Vec::new();
        write_api_key(&mut empty, "").expect("write empty");
        assert!(empty.is_empty(), "an empty key writes zero bytes");
    }

    /// End-to-end through `dispatch`: the helper returns Err for a profile with
    /// no api_key — a helper pointing at a non-api profile must surface as a
    /// non-zero exit so CC's request fails loudly rather than sending a blank
    /// credential.
    #[test]
    fn dispatch_api_key_helper_fails_closed_for_profile_without_key() {
        let _home = HomeSandbox::new();
        save_profile_with_key("oauth-profile", None);
        let args = ["__api-key".to_string(), "oauth-profile".to_string()];
        dispatch(&args).expect_err("profile without a key must fail closed");
    }

    /// End-to-end through `dispatch`: the helper returns Err for a profile name
    /// that doesn't exist. A stale helper string from a deleted profile must
    /// surface as a request failure, not silently exit 0.
    #[test]
    fn dispatch_api_key_helper_fails_closed_for_missing_profile() {
        let _home = HomeSandbox::new();
        let args = ["__api-key".to_string(), "ghost-profile".to_string()];
        dispatch(&args).expect_err("missing profile must fail closed");
    }
}
