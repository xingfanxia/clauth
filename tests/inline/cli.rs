//! `clauth login`'s hand-rolled arg parsing. `parse_login_args` is a pure
//! shape check, so it's exercised directly; `dispatch`'s success arm for a
//! *valid* `--model` shape calls `cmd_login`, which spawns a real `claude`
//! process via `start::run` and is never exercised here. Model persistence
//! itself is covered in `tests/inline/actions.rs` (`set_profile_default_model`).

use super::*;

#[test]
fn parse_login_args_bare_name_has_no_model() {
    let args = ["acme".to_string()];
    assert_eq!(parse_login_args(&args), Some(("acme", None)));
}

#[test]
fn parse_login_args_accepts_model_flag() {
    let args = [
        "acme".to_string(),
        "--model".to_string(),
        "opus".to_string(),
    ];
    assert_eq!(parse_login_args(&args), Some(("acme", Some("opus"))));
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
        Some(("acme", Some("claude-opus-4-8")))
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
    let flag_name = [
        "--model".to_string(),
        "--model".to_string(),
        "opus".to_string(),
    ];
    assert_eq!(parse_login_args(&flag_name), None);
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
