#![allow(clippy::unwrap_used, clippy::expect_used)]
//! UPS-18's one-time fork migration: the legacy `harness = "codex"` layout into
//! upstream's two-file split. Every test runs against a `HomeSandbox` — the
//! migration rewrites `profiles.toml` and renames credential files, so none of
//! it may ever reach a real `~/.clauth`.

use super::*;
use crate::codex_profiles::CodexState;
use crate::testutil::HomeSandbox;

/// Lay down the legacy shape: `profiles.toml` with both rosters mixed, each
/// profile's `config.toml` carrying the dead `harness` key, and each codex
/// profile's credential at the fork's `codex-auth.json`.
fn legacy_home(claude: &[&str], codex: &[&str], chain: &[&str], active: Option<&str>) {
    let dir = clauth_dir().unwrap();
    crate::profile::mkdir_700(&dir).unwrap();
    let all: Vec<String> = claude
        .iter()
        .chain(codex.iter())
        .map(|n| format!("\"{n}\""))
        .collect();
    let active_line =
        active.map_or_else(String::new, |a| format!("active_codex_profile = \"{a}\"\n"));
    let chain_line = format!(
        "codex_fallback_chain = [{}]\n",
        chain
            .iter()
            .map(|n| format!("\"{n}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    std::fs::write(
        dir.join("profiles.toml"),
        format!(
            "active_profile = \"{}\"\n\
             profiles = [{}]\n\
             fallback_chain = [\"{}\"]\n\
             wrap_off = true\n\
             weekly_switch_threshold = 95.0\n\
             {active_line}{chain_line}",
            claude.first().unwrap_or(&""),
            all.join(", "),
            claude.first().unwrap_or(&""),
        ),
    )
    .unwrap();

    for (names, harness) in [(claude, "claude"), (codex, "codex")] {
        for name in names {
            let pdir = profile_dir(&ProfileName::from(*name)).unwrap();
            crate::profile::mkdir_700(&pdir).unwrap();
            std::fs::write(
                pdir.join("config.toml"),
                format!("harness = \"{harness}\"\nauto_start = false\n"),
            )
            .unwrap();
            if harness == "codex" {
                std::fs::write(
                    pdir.join("codex-auth.json"),
                    format!("{{\"who\":\"{name}\"}}"),
                )
                .unwrap();
            }
        }
    }
}

fn read_state() -> String {
    std::fs::read_to_string(clauth_dir().unwrap().join("profiles.toml")).unwrap()
}

// ── detection ───────────────────────────────────────────────────────────────

#[test]
fn an_install_that_never_ran_this_fork_has_nothing_to_migrate() {
    let _home = HomeSandbox::new();
    assert!(plan().unwrap().is_empty(), "no profiles.toml at all");

    let dir = clauth_dir().unwrap();
    crate::profile::mkdir_700(&dir).unwrap();
    std::fs::write(
        dir.join("profiles.toml"),
        "active_profile = \"work\"\nprofiles = [\"work\"]\n",
    )
    .unwrap();
    let p = plan().unwrap();
    assert!(
        p.is_empty(),
        "a pure-claude roster is already in the new shape"
    );
    assert!(p.profiles.is_empty() && p.harness_keys.is_empty());
}

#[test]
fn the_plan_reads_the_whole_legacy_record_off_disk() {
    let _home = HomeSandbox::new();
    legacy_home(
        &["work"],
        &["cx-a", "cx-b"],
        &["cx-a", "cx-b"],
        Some("cx-a"),
    );

    let p = plan().unwrap();
    assert_eq!(
        p.profiles,
        vec![ProfileName::from("cx-a"), ProfileName::from("cx-b")]
    );
    assert_eq!(p.active, Some(ProfileName::from("cx-a")));
    assert_eq!(
        p.chain,
        vec![ProfileName::from("cx-a"), ProfileName::from("cx-b")]
    );
    assert_eq!(p.store_renames.len(), 2);
    assert_eq!(
        p.harness_keys.len(),
        3,
        "the key is dead for every harness, so the claude config.toml is swept too"
    );
    assert!(
        p.inherited_wrap_off,
        "the fork had ONE wrap-off governing both chains"
    );
    assert_eq!(p.inherited_weekly, Some(95.0));
}

#[test]
fn the_classifier_is_the_profiles_own_config_never_the_legacy_chain() {
    // A claude account listed in `codex_fallback_chain` by a hand-edit must NOT
    // be moved: migrating it would file a claude credential under the codex
    // roster, where the codex engine would try to refresh it as an auth.json.
    let _home = HomeSandbox::new();
    legacy_home(&["work"], &["cx-a"], &["cx-a", "work"], Some("cx-a"));

    let p = plan().unwrap();
    assert_eq!(p.profiles, vec![ProfileName::from("cx-a")]);
    assert_eq!(
        p.chain,
        vec![ProfileName::from("cx-a")],
        "the stray claude name is filtered out of the carried chain"
    );
}

#[test]
fn a_stale_active_slot_naming_a_deleted_profile_is_dropped() {
    let _home = HomeSandbox::new();
    legacy_home(&["work"], &["cx-a"], &["cx-a"], Some("gone"));
    assert_eq!(plan().unwrap().active, None);
}

// ── the run ─────────────────────────────────────────────────────────────────

#[test]
fn the_run_moves_the_roster_the_slot_the_chain_and_the_stores() {
    let _home = HomeSandbox::new();
    legacy_home(
        &["work"],
        &["cx-a", "cx-b"],
        &["cx-a", "cx-b"],
        Some("cx-a"),
    );
    let p = plan().unwrap();
    run(&p).unwrap();

    let codex = CodexState::load().unwrap();
    assert_eq!(
        codex.profiles(),
        &[ProfileName::from("cx-a"), ProfileName::from("cx-b")]
    );
    assert_eq!(codex.active_profile(), Some(&ProfileName::from("cx-a")));
    assert_eq!(
        codex.fallback_chain(),
        &[ProfileName::from("cx-a"), ProfileName::from("cx-b")]
    );
    assert!(codex.switch_off_when_spent(), "the wrap-off carried across");
    assert_eq!(codex.weekly_switch_threshold_pct(), 95.0);

    // The claude roster keeps its own and loses the codex names + legacy keys.
    let state = crate::profile::load_app_state().unwrap();
    assert_eq!(state.profiles, vec![ProfileName::from("work")]);
    let raw = read_state();
    assert!(!raw.contains("active_codex_profile"));
    assert!(!raw.contains("codex_fallback_chain"));
    assert!(
        raw.contains("active_profile = \"work\""),
        "the claude slot is untouched"
    );

    // Each store moved to upstream's path, contents byte-identical.
    for name in ["cx-a", "cx-b"] {
        let dir = profile_dir(&ProfileName::from(name)).unwrap();
        assert!(!dir.join("codex-auth.json").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("auth.json")).unwrap(),
            format!("{{\"who\":\"{name}\"}}"),
            "the credential is MOVED, never rewritten"
        );
        let cfg = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(!cfg.contains("harness"), "the dead key is gone");
        assert!(
            cfg.contains("auto_start = false"),
            "every other setting survives"
        );
    }
}

#[test]
fn a_rename_never_overwrites_a_store_that_already_exists() {
    // A half-run migration, or an adopt that already wrote upstream's path,
    // leaves a LIVE chain at auth.json. Clobbering it with the stale legacy
    // copy would install a spent refresh token.
    let _home = HomeSandbox::new();
    legacy_home(&["work"], &["cx-a"], &["cx-a"], Some("cx-a"));
    let dir = profile_dir(&ProfileName::from("cx-a")).unwrap();
    std::fs::write(dir.join("auth.json"), "{\"who\":\"fresher\"}").unwrap();

    let p = plan().unwrap();
    assert!(p.store_renames.is_empty(), "the rename is not even planned");
    run(&p).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("auth.json")).unwrap(),
        "{\"who\":\"fresher\"}"
    );
    assert!(
        dir.join("codex-auth.json").exists(),
        "the legacy copy is left in place as evidence, not deleted"
    );
}

#[test]
fn running_it_twice_changes_nothing_the_second_time() {
    let _home = HomeSandbox::new();
    legacy_home(&["work"], &["cx-a"], &["cx-a"], Some("cx-a"));
    run(&plan().unwrap()).unwrap();
    let after_first = (read_state(), CodexState::load().unwrap());

    let second = plan().unwrap();
    assert!(second.is_empty(), "nothing left to find");
    run(&second).unwrap();
    assert_eq!(read_state(), after_first.0);
    assert_eq!(CodexState::load().unwrap(), after_first.1);
}

#[test]
fn a_conflicting_codex_roster_refuses_rather_than_picking_one() {
    let _home = HomeSandbox::new();
    legacy_home(&["work"], &["cx-a"], &["cx-a"], Some("cx-a"));
    // Someone already built a DIFFERENT codex roster by hand.
    std::fs::write(
        clauth_dir().unwrap().join("codex-profiles.toml"),
        "profiles = [\"other\"]\n",
    )
    .unwrap();

    let err = run(&plan().unwrap()).expect_err("two rosters must not merge silently");
    assert!(
        err.to_string().contains("already holds a roster"),
        "the refusal names the reason: {err}"
    );
    // And it refused BEFORE touching anything.
    assert!(
        profile_dir(&ProfileName::from("cx-a"))
            .unwrap()
            .join("codex-auth.json")
            .exists()
    );
}

#[test]
fn a_codex_name_in_the_claude_quarantine_list_is_dropped_and_reported() {
    // Upstream records a codex quarantine per profile with a verdict and a
    // clock; the claude list carries neither, so the name is dropped rather
    // than translated. Fail-safe: the next poll re-judges on evidence.
    let _home = HomeSandbox::new();
    legacy_home(&["work"], &["cx-a"], &["cx-a"], Some("cx-a"));
    let path = clauth_dir().unwrap().join("profiles.toml");
    let raw = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{raw}auth_broken = [\"cx-a\"]\n")).unwrap();

    let p = plan().unwrap();
    assert_eq!(p.dropped_quarantines, vec![ProfileName::from("cx-a")]);
    assert!(
        p.describe().iter().any(|l| l.contains("auth_broken")),
        "the dry run says so out loud"
    );
    run(&p).unwrap();
    assert!(!read_state().contains("cx-a"));
}

// ── the text edits ──────────────────────────────────────────────────────────

#[test]
fn stripping_a_key_leaves_every_other_byte_alone() {
    // profiles.toml is hand-editable, so the operator's diff after a migration
    // must show the keys that went — not a wholesale reformat.
    let raw = "# a comment\n\
               active_profile = \"work\"\n\
               active_codex_profile = \"cx\"\n\
               weekly_switch_threshold = 95.0\n\n\
               [herdr]\n\
               active_codex_profile = \"kept\"\n";
    let out = strip_top_level_key(raw, "active_codex_profile");
    assert!(out.contains("# a comment"));
    assert!(out.contains("active_profile = \"work\""));
    assert!(out.contains("weekly_switch_threshold = 95.0"));
    assert!(
        out.contains("active_codex_profile = \"kept\""),
        "a same-named key inside a table is NOT top-level and must survive"
    );
    assert!(!out.contains("active_codex_profile = \"cx\""));
}

#[test]
fn stripping_a_key_takes_its_whole_multi_line_array() {
    let raw = "profiles = [\"a\"]\n\
               codex_fallback_chain = [\n  \"cx-a\",\n  \"cx-b\",\n]\n\
               wrap_off = true\n";
    let out = strip_top_level_key(raw, "codex_fallback_chain");
    assert!(!out.contains("cx-a") && !out.contains("cx-b"));
    assert!(out.contains("profiles = [\"a\"]") && out.contains("wrap_off = true"));
    out.parse::<toml::Table>().expect("still parses");
}

#[test]
fn removing_names_from_an_array_keeps_the_rest_in_order() {
    let names: std::collections::BTreeSet<String> = ["cx-a".to_string(), "cx-b".to_string()]
        .into_iter()
        .collect();
    let raw = "profiles = [\"work\", \"cx-a\", \"home\", \"cx-b\"]\n";
    let out = remove_from_array(raw, "profiles", &names);
    let table: toml::Table = out.parse().unwrap();
    let kept: Vec<&str> = table["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(toml::Value::as_str)
        .collect();
    assert_eq!(kept, ["work", "home"]);
}

#[test]
fn an_unmodelled_key_survives_the_migration() {
    // The same rule `preserve_unmodelled_state_keys` enforces on a save: a key
    // a NEWER clauth wrote, which this binary does not know, must not be erased
    // by a migration either.
    let _home = HomeSandbox::new();
    legacy_home(&["work"], &["cx-a"], &["cx-a"], Some("cx-a"));
    let path = clauth_dir().unwrap().join("profiles.toml");
    let raw = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{raw}some_future_key = 7\n")).unwrap();

    run(&plan().unwrap()).unwrap();
    assert!(read_state().contains("some_future_key = 7"));
}
