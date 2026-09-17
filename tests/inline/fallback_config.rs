//! Tests for the shared fallback-chain config edits. Each runs under a throwaway
//! HOME (`HomeSandbox`) so `save_app_state` / `save_profile` write into a tempdir
//! and never touch the real `~/.clauth`.

use super::*;
use crate::profile::{AppConfig, AppState, Profile};
use crate::testutil::{HomeSandbox, blank_profile};

/// Build an in-memory config from profile names + an initial chain.
fn config(names: &[&str], chain: &[&str]) -> AppConfig {
    let profiles: Vec<Profile> = names
        .iter()
        .map(|n| blank_profile(&crate::profile::ProfileName::from(*n)))
        .collect();
    AppConfig {
        state: AppState {
            active_profile: names.first().map(|n| (*n).into()),
            profiles: names.iter().map(|n| (*n).into()).collect(),
            fallback_chain: chain.iter().map(|n| (*n).into()).collect(),
            ..AppState::default()
        },
        profiles,
    }
}

fn chain_of(c: &AppConfig) -> Vec<String> {
    c.state
        .fallback_chain
        .iter()
        .map(|n| n.as_str().to_string())
        .collect()
}

#[test]
fn add_appends_and_seeds_default_threshold() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a", "b", "c"], &["a"]);
    add(&mut c, "b").unwrap();
    assert_eq!(chain_of(&c), vec!["a", "b"]);
    assert_eq!(
        c.find(&crate::profile::ProfileName::from("b"))
            .unwrap()
            .fallback_threshold,
        Some(DEFAULT_THRESHOLD)
    );
}

#[test]
fn add_existing_member_is_noop() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a", "b"], &["a", "b"]);
    add(&mut c, "a").unwrap();
    assert_eq!(chain_of(&c), vec!["a", "b"]);
}

#[test]
fn add_unknown_profile_errors() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a"], &[]);
    assert!(add(&mut c, "nope").is_err());
}

#[test]
fn add_preserves_an_existing_threshold() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a", "b"], &["a"]);
    c.find_mut(&crate::profile::ProfileName::from("b"))
        .unwrap()
        .fallback_threshold = Some(50.0);
    add(&mut c, "b").unwrap();
    assert_eq!(
        c.find(&crate::profile::ProfileName::from("b"))
            .unwrap()
            .fallback_threshold,
        Some(50.0)
    );
}

#[test]
fn remove_drops_the_member() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a", "b", "c"], &["a", "b", "c"]);
    remove(&mut c, "b").unwrap();
    assert_eq!(chain_of(&c), vec!["a", "c"]);
}

#[test]
fn remove_absent_member_is_noop() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a", "b"], &["a"]);
    remove(&mut c, "b").unwrap();
    assert_eq!(chain_of(&c), vec!["a"]);
}

#[test]
fn move_up_and_down_reorders() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a", "b", "c"], &["a", "b", "c"]);
    move_member(&mut c, "c", MoveDir::Up).unwrap();
    assert_eq!(chain_of(&c), vec!["a", "c", "b"]);
    move_member(&mut c, "a", MoveDir::Down).unwrap();
    assert_eq!(chain_of(&c), vec!["c", "a", "b"]);
}

#[test]
fn move_at_a_boundary_is_noop() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a", "b"], &["a", "b"]);
    move_member(&mut c, "a", MoveDir::Up).unwrap();
    assert_eq!(chain_of(&c), vec!["a", "b"]);
    move_member(&mut c, "b", MoveDir::Down).unwrap();
    assert_eq!(chain_of(&c), vec!["a", "b"]);
}

#[test]
fn move_non_member_is_noop() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a", "b"], &["a"]);
    move_member(&mut c, "b", MoveDir::Up).unwrap();
    assert_eq!(chain_of(&c), vec!["a"]);
}

#[test]
fn set_threshold_clamps_to_0_100() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a"], &["a"]);
    set_threshold(&mut c, "a", 150.0).unwrap();
    assert_eq!(
        c.find(&crate::profile::ProfileName::from("a"))
            .unwrap()
            .fallback_threshold,
        Some(100.0)
    );
    set_threshold(&mut c, "a", -5.0).unwrap();
    assert_eq!(
        c.find(&crate::profile::ProfileName::from("a"))
            .unwrap()
            .fallback_threshold,
        Some(0.0)
    );
    set_threshold(&mut c, "a", 80.0).unwrap();
    assert_eq!(
        c.find(&crate::profile::ProfileName::from("a"))
            .unwrap()
            .fallback_threshold,
        Some(80.0)
    );
}

#[test]
fn set_threshold_unknown_profile_errors() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a"], &["a"]);
    assert!(set_threshold(&mut c, "nope", 50.0).is_err());
}

#[test]
fn set_last_resort_sets_clears_and_persists() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a"], &["a"]);
    assert!(
        !c.find(&crate::profile::ProfileName::from("a"))
            .unwrap()
            .last_resort,
        "off by default"
    );
    // Ok(false): writes only the profile's config.toml, never profiles.toml.
    assert!(!set_last_resort(&mut c, "a", true).unwrap());
    assert!(
        c.find(&crate::profile::ProfileName::from("a"))
            .unwrap()
            .last_resort
    );
    // Survives a reload — the mark is persisted to the profile's config.toml,
    // not in-memory only.
    let toml_path = crate::profile::profile_dir(&crate::profile::ProfileName::from("a"))
        .unwrap()
        .join("config.toml");
    let toml = std::fs::read_to_string(toml_path).unwrap();
    assert!(toml.contains("last_resort = true"), "persisted: {toml}");
    assert!(!set_last_resort(&mut c, "a", false).unwrap());
    assert!(
        !c.find(&crate::profile::ProfileName::from("a"))
            .unwrap()
            .last_resort
    );
}

#[test]
fn set_last_resort_unknown_profile_errors() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a"], &["a"]);
    assert!(set_last_resort(&mut c, "nope", true).is_err());
}

#[test]
fn set_weekly_threshold_persists_validates_and_noops() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a"], &["a"]);
    assert_eq!(c.state.weekly_switch_threshold, None);
    assert!(
        set_weekly_threshold(&mut c, 95.0).unwrap(),
        "first set writes"
    );
    assert_eq!(c.state.weekly_switch_threshold, Some(95.0));
    assert!(
        !set_weekly_threshold(&mut c, 95.0).unwrap(),
        "same value is a no-op (no state write)"
    );
    // Out-of-band values error and leave state untouched.
    assert!(set_weekly_threshold(&mut c, 49.9).is_err());
    assert!(set_weekly_threshold(&mut c, 100.1).is_err());
    assert_eq!(c.state.weekly_switch_threshold, Some(95.0));
}

#[test]
fn set_wrap_off_toggles_and_persists() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a"], &["a"]);
    assert!(!c.state.switch_off_when_spent);
    set_wrap_off(&mut c, true).unwrap();
    assert!(c.state.switch_off_when_spent);
    set_wrap_off(&mut c, false).unwrap();
    assert!(!c.state.switch_off_when_spent);
}

#[test]
fn return_flags_whether_profiles_toml_was_written() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a", "b", "c"], &["a", "b"]);
    // State-changing edits report true; no-ops and threshold-only report false —
    // this is what gates the daemon's last_reload_fp adoption.
    assert!(add(&mut c, "c").unwrap(), "add new member writes state");
    assert!(!add(&mut c, "c").unwrap(), "re-add is a no-op");
    assert!(
        move_member(&mut c, "c", MoveDir::Up).unwrap(),
        "move writes state"
    );
    assert!(
        !move_member(&mut c, "a", MoveDir::Up).unwrap(),
        "move at top is a no-op"
    );
    assert!(
        !set_threshold(&mut c, "a", 70.0).unwrap(),
        "threshold writes config.toml, not profiles.toml"
    );
    assert!(set_wrap_off(&mut c, true).unwrap(), "wrap-off writes state");
    assert!(remove(&mut c, "c").unwrap(), "remove writes state");
    assert!(!remove(&mut c, "c").unwrap(), "remove-absent is a no-op");
}

#[test]
fn move_dir_parse_is_case_insensitive() {
    assert_eq!(MoveDir::parse("up"), Some(MoveDir::Up));
    assert_eq!(MoveDir::parse("DOWN"), Some(MoveDir::Down));
    assert_eq!(MoveDir::parse("  Up "), Some(MoveDir::Up));
    assert_eq!(MoveDir::parse("sideways"), None);
}

#[test]
fn rename_updates_every_reference_and_moves_the_dir() {
    let _h = HomeSandbox::new();
    // "a" is active AND a chain member — rename must update the name list, the chain,
    // the active marker, AND move the on-disk directory.
    let mut c = config(&["a", "b"], &["a", "b"]);
    save_profile(c.find(&crate::profile::ProfileName::from("a")).unwrap()).unwrap();
    assert!(
        crate::profile::profile_dir(&crate::profile::ProfileName::from("a"))
            .unwrap()
            .exists()
    );

    assert!(
        rename(&mut c, "a", "renamed").unwrap(),
        "a real rename writes state"
    );
    assert!(
        c.find(&crate::profile::ProfileName::from("a")).is_none(),
        "old name gone from config"
    );
    assert!(
        c.find(&crate::profile::ProfileName::from("renamed"))
            .is_some(),
        "new name present"
    );
    assert_eq!(
        chain_of(&c),
        vec!["renamed", "b"],
        "chain reference renamed"
    );
    assert_eq!(
        c.state.active_profile.as_ref().map(|n| n.as_str()),
        Some("renamed"),
        "active marker renamed",
    );
    assert!(
        !crate::profile::profile_dir(&crate::profile::ProfileName::from("a"))
            .unwrap()
            .exists(),
        "old dir moved"
    );
    assert!(
        crate::profile::profile_dir(&crate::profile::ProfileName::from("renamed"))
            .unwrap()
            .exists(),
        "new dir present"
    );
}

#[test]
fn rename_to_an_existing_name_is_rejected() {
    let _h = HomeSandbox::new();
    // The collision check reads the ON-DISK roster — it has to see both state
    // files to enforce cross-harness uniqueness — so the names must exist there,
    // not only in this config. In the daemon they always do: its config IS a
    // load of that file.
    crate::testutil::register_names(&["a", "b"]);
    let mut c = config(&["a", "b"], &[]);
    assert!(
        rename(&mut c, "a", "b").is_err(),
        "a collision must be refused"
    );
    assert!(
        c.find(&crate::profile::ProfileName::from("a")).is_some()
            && c.find(&crate::profile::ProfileName::from("b")).is_some(),
        "nothing renamed on rejection"
    );
}

#[test]
fn rename_to_the_same_name_is_a_noop() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a"], &[]);
    // Case-only difference resolves to the same canonical → no write. (A true no-op
    // guards against a needless dir rename + state write for a self-rename.)
    assert!(
        !rename(&mut c, "a", "a").unwrap(),
        "self-rename writes nothing"
    );
}

/// Seed `codex-profiles.toml` inside the sandbox. The harness axis is WHICH
/// FILE holds the name — a codex profile is a name in the codex roster, not a
/// field on a profile in `profiles.toml` — so a codex fixture is a roster on
/// disk, which is exactly what `fallback_config::codex_member` resolves against.
fn seed_codex(roster: &[&str], chain: &[&str]) {
    crate::codex_profiles::CodexState::update(|s| {
        for n in roster {
            s.add_profile(n);
        }
        let members = s.fallback_chain_mut();
        for n in chain {
            members.push(crate::profile::ProfileName::from(*n));
        }
        Ok(())
    })
    .expect("seed the codex roster");
}

/// The codex chain as persisted. There is no in-memory codex copy to read: the
/// chain lives in `codex-profiles.toml` alone, so every read here is an on-disk
/// read and every assertion below is a persistence assertion.
fn codex_chain_on_disk() -> Vec<String> {
    crate::codex_profiles::CodexState::load()
        .expect("load codex state")
        .fallback_chain()
        .iter()
        .map(|n| n.as_str().to_string())
        .collect()
}

// CDX-4 C1: chains are per-harness, enforced by ROUTING — a codex profile's
// membership edits land in the codex chain (`codex-profiles.toml`), never the
// claude chain, and vice versa. Homogeneity holds by construction: the two
// rosters are separate files, so neither chain can hold the other kind.
#[test]
fn membership_edits_route_by_harness() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a"], &["a"]);
    seed_codex(&["cdx1", "cdx2"], &[]);

    // Codex adds land in the codex chain; the claude chain never moves.
    assert!(add(&mut c, "cdx1").unwrap());
    assert!(add(&mut c, "cdx2").unwrap());
    assert_eq!(chain_of(&c), vec!["a"], "claude chain untouched");
    assert_eq!(codex_chain_on_disk(), vec!["cdx1", "cdx2"]);

    // Duplicate add is a no-op; move + remove act on the codex chain only.
    assert!(!add(&mut c, "cdx1").unwrap());
    assert!(move_member(&mut c, "cdx2", MoveDir::Up).unwrap());
    assert_eq!(codex_chain_on_disk(), vec!["cdx2", "cdx1"]);
    assert!(remove(&mut c, "cdx2").unwrap());
    assert_eq!(codex_chain_on_disk(), vec!["cdx1"]);
    assert_eq!(chain_of(&c), vec!["a"], "claude chain still untouched");
}

// The reverse direction: a claude profile's edits never touch the codex chain.
#[test]
fn claude_edits_never_touch_the_codex_chain() {
    let _h = HomeSandbox::new();
    let mut c = config(&["a", "b"], &["a"]);
    seed_codex(&["cdx"], &["cdx"]);
    assert!(add(&mut c, "b").unwrap());
    assert_eq!(chain_of(&c), vec!["a", "b"]);
    assert_eq!(
        codex_chain_on_disk(),
        vec!["cdx"],
        "codex chain untouched by claude edits"
    );
}
