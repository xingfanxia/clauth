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
    })
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

// ── parse_delete_args ──

#[test]
fn parse_delete_args_bare_name_no_yes() {
    assert_eq!(
        parse_delete_args(&["acme".to_string()]),
        Some(("acme", false))
    );
}

#[test]
fn parse_delete_args_accepts_yes_and_short_flag_anywhere() {
    assert_eq!(
        parse_delete_args(&["acme".to_string(), "--yes".to_string()]),
        Some(("acme", true))
    );
    assert_eq!(
        parse_delete_args(&["-y".to_string(), "acme".to_string()]),
        Some(("acme", true))
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
