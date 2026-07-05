//! Behaviour tests for `next_target` — the fallback-chain target picker.
//!
//! Tests stay hermetic: no filesystem I/O, no `switch_profile`. All scenarios
//! construct an in-memory `AppConfig` and assert on `next_target`'s return value.
//!
//! Issue #8 follow-up: `Profile::last_resort` replaced the old `threshold >=
//! 100.0` sentinel as the chain-walk terminal-stop marker. Several scenarios
//! below deliberately mark a member `last_resort` at a threshold OTHER than
//! 100 to prove the two are now independent.

use crate::lockorder::RankedMutex;
use std::collections::HashMap;
use std::sync::Arc;

use super::*;
use crate::profile::{AppConfig, AppState, Profile, ProfileName};
use crate::usage::{UsageInfo, UsageStore, UsageWindow, epoch_secs_to_iso, now_epoch_secs};

/// ISO reset an hour ahead — a live 5h window.
fn live_reset() -> String {
    epoch_secs_to_iso(now_epoch_secs() + 3600)
}

/// ISO reset an hour ago — a lapsed 5h window.
fn expired_reset() -> String {
    epoch_secs_to_iso(now_epoch_secs() - 3600)
}

fn window(utilization: f64, resets_at: Option<String>) -> UsageWindow {
    UsageWindow {
        utilization,
        resets_at,
    }
}

fn usage_info(five_hour: Option<UsageWindow>) -> UsageInfo {
    UsageInfo {
        five_hour,
        ..UsageInfo::default()
    }
}

fn profile_with_usage(name: &str, threshold: Option<f64>, usage: Option<UsageInfo>) -> Profile {
    use std::collections::BTreeMap;
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: threshold,
        last_resort: false,
        bell_threshold: None,
        credentials: None,
        usage,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

/// Profile whose 5h window is live (future reset) at the given utilization —
/// the exhaustion predicates only trust a live window.
fn profile_with_util(name: &str, threshold: Option<f64>, utilization: Option<f64>) -> Profile {
    profile_with_usage(
        name,
        threshold,
        utilization.map(|u| usage_info(Some(window(u, Some(live_reset()))))),
    )
}

/// Marks a profile as the chain's last resort (`Profile::last_resort = true`).
/// A separate wrapper — rather than threading a bool through every helper —
/// so call sites read as "this member is the sink" instead of inferring it
/// from an incidental threshold value.
fn mark_last_resort(mut p: Profile) -> Profile {
    p.last_resort = true;
    p
}

fn config_with_chain(profiles: Vec<Profile>, active: &str) -> AppConfig {
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    AppConfig {
        state: AppState {
            active_profile: Some(active.into()),
            profiles: names.clone(),
            fallback_chain: names,
            ..AppState::default()
        },
        profiles,
    }
}

// Both maxed and BOTH marked last_resort at a non-100 threshold: active is
// itself the sink, so it stays parked without the walk ever reaching B.
#[test]
fn all_maxed_sinks_no_switch() {
    let config = config_with_chain(
        vec![
            mark_last_resort(profile_with_util("a", Some(90.0), Some(95.0))),
            mark_last_resort(profile_with_util("b", Some(90.0), Some(95.0))),
        ],
        "a",
    );
    assert_eq!(next_target(&config), None);
}

// Active (unmarked, threshold 95) at 100%; B is marked last_resort at an 80%
// threshold — not 100 — proving the migration follows the mark, not the
// threshold value. One migration allowed.
#[test]
fn non_sink_active_migrates_to_sink_once() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            mark_last_resort(profile_with_util("b", Some(80.0), Some(100.0))),
        ],
        "a",
    );
    assert_eq!(
        next_target(&config),
        Some(SwitchAction::To("b".to_string()))
    );
}

// B active and marked last_resort (80% threshold, not 100) — no further
// migration, exactly like the old threshold==100 sentinel parked.
#[test]
fn sink_active_maxed_stays_put() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            mark_last_resort(profile_with_util("b", Some(80.0), Some(100.0))),
        ],
        "b",
    );
    assert_eq!(next_target(&config), None);
}

// Active marked last_resort (80% threshold, maxed), B has headroom (95% @
// 50%) — migrates to B. The headroom pass always wins before the
// last-resort loop guard is even consulted.
#[test]
fn sink_active_switches_to_member_with_headroom() {
    let config = config_with_chain(
        vec![
            mark_last_resort(profile_with_util("a", Some(80.0), Some(100.0))),
            profile_with_util("b", Some(95.0), Some(50.0)),
        ],
        "a",
    );
    assert_eq!(
        next_target(&config),
        Some(SwitchAction::To("b".to_string()))
    );
}

// No member marked last_resort (both threshold 95 at 100%) — returns None.
#[test]
fn no_sink_available_returns_none() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    assert_eq!(next_target(&config), None);
}

// ── issue #8 follow-up: threshold no longer implies last_resort ─────────────

// A threshold of 100 alone must NOT act as a sink anymore. With wrap_off on
// and NEITHER member marked last_resort, switch-off-all still fires even
// though the active sits at an unmarked 100% threshold. The OLD
// `threshold >= 100.0` sentinel treated the active as a sink here and
// returned `None` (the walk stopped dead) — this assertion fails against it.
#[test]
fn unmarked_hundred_threshold_active_no_longer_acts_as_sink() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(100.0), Some(100.0)),
            profile_with_util("b", Some(100.0), Some(100.0)),
        ],
        "a",
    );
    config.state.wrap_off = true;
    assert_eq!(next_target(&config), Some(SwitchAction::Off));
}

// Same decoupling from the other direction: an unmarked 100%-threshold OTHER
// member (not the active) must not block switch-off-all either. The OLD
// sentinel would have accepted B as a last-resort switch target instead of
// firing Off; unmarked, B is just another exhausted, non-viable member.
#[test]
fn wrap_off_switches_off_when_unmarked_hundred_threshold_member_present() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(100.0), Some(100.0)),
        ],
        "a",
    );
    config.state.wrap_off = true;
    assert_eq!(next_target(&config), Some(SwitchAction::Off));
}

// ── next_auto_switch_target ───────────────────────────────────────────────────
//
// Same scenarios via the scheduler-side path: snapshot the chain from AppConfig,
// read utilization from UsageStore (not Profile.usage). The split avoids the
// config ↔ store lock inversion against App::apply_usage.

/// Store entries with live 5h windows (future reset) at the given utilizations.
fn store_with_utils(pairs: &[(&str, f64)]) -> UsageStore {
    store_with_infos(
        pairs
            .iter()
            .map(|(name, util)| (*name, usage_info(Some(window(*util, Some(live_reset()))))))
            .collect(),
    )
}

fn store_with_infos(entries: Vec<(&str, UsageInfo)>) -> UsageStore {
    let map: HashMap<String, UsageInfo> = entries
        .into_iter()
        .map(|(name, info)| (name.to_string(), info))
        .collect();
    Arc::new(RankedMutex::new(map))
}

#[test]
fn snapshot_chain_captures_thresholds_and_active() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(50.0)),
            mark_last_resort(profile_with_util("b", Some(80.0), Some(20.0))),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    assert_eq!(snap.active, "a");
    assert_eq!(snap.chain.len(), 2);
    assert_eq!(snap.chain[0].name, "a");
    assert!((snap.chain[0].threshold - 95.0).abs() < f64::EPSILON);
    assert!(!snap.chain[0].last_resort);
    assert_eq!(snap.chain[1].name, "b");
    assert!((snap.chain[1].threshold - 80.0).abs() < f64::EPSILON);
    assert!(
        snap.chain[1].last_resort,
        "last_resort is captured independent of threshold"
    );
}

#[test]
fn snapshot_chain_none_when_active_not_in_chain() {
    let mut config = config_with_chain(vec![profile_with_util("a", Some(95.0), Some(50.0))], "a");
    // active is set but absent from the chain
    config.state.fallback_chain = vec!["other".into()];
    assert!(snapshot_chain(&config).is_none());
}

#[test]
fn auto_switch_returns_none_when_active_below_threshold() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("a", 90.0), ("b", 10.0)]); // active at 90% < 95% → no switch
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

#[test]
fn auto_switch_picks_member_with_headroom() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("a", 100.0), ("b", 20.0)]); // active maxed, B has headroom
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string())),
    );
}

// Both marked last_resort at a non-100 threshold (80%) — active is itself the
// sink; the loop guard holds and no migration to B forms.
#[test]
fn auto_switch_sink_loop_guard_holds() {
    let config = config_with_chain(
        vec![
            mark_last_resort(profile_with_util("a", Some(80.0), None)),
            mark_last_resort(profile_with_util("b", Some(80.0), None)),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]); // both maxed sinks → no migration
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

// Parity with `next_target`'s decoupling on the snapshot walk: an unmarked
// threshold-100 member is a late switch point, never a sink — with everyone
// exhausted and nothing marked, wrap-off switches off instead of parking.
#[test]
fn auto_switch_unmarked_hundred_threshold_member_is_not_a_sink() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(100.0), None),
        ],
        "a",
    );
    config.state.wrap_off = true;
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::Off),
        "threshold 100 without the mark must not park the snapshot walk"
    );
}

// A (unmarked, threshold 95%) is not a sink; B is marked last_resort at an
// 80% threshold — one migration to B.
#[test]
fn auto_switch_non_sink_active_migrates_to_sink_once() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            mark_last_resort(profile_with_util("b", Some(80.0), None)),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]); // active not a sink, B is → one migration
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string())),
    );
}

#[test]
fn auto_switch_missing_util_is_not_exhausted() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("b", 10.0)]); // active absent from store → not exhausted → no switch
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

// ── wrap-off mode ───────────────────────────────────────────────────────────
//
// When no member is marked last_resort and the whole chain is exhausted,
// wrap-off turns off all accounts instead of staying put.

// next_target: wrap_off on, no last_resort member, all exhausted → Off.
#[test]
fn wrap_off_switches_off_when_chain_spent() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    config.state.wrap_off = true;
    assert_eq!(next_target(&config), Some(SwitchAction::Off));
}

// next_target: wrap_off on but a last_resort member exists (at an 80%
// threshold, not 100) → migrate there, not Off.
#[test]
fn wrap_off_prefers_sink_over_off() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            mark_last_resort(profile_with_util("b", Some(80.0), Some(100.0))),
        ],
        "a",
    );
    config.state.wrap_off = true;
    assert_eq!(
        next_target(&config),
        Some(SwitchAction::To("b".to_string()))
    );
}

// next_target: wrap_off on but active still has headroom → no Off.
#[test]
fn wrap_off_skips_off_when_active_has_headroom() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(50.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    config.state.wrap_off = true;
    // a at 50% < 95% → not exhausted → stay
    assert_eq!(next_target(&config), None);
}

// next_target: same spent chain, wrap_off off → legacy None.
#[test]
fn wrap_off_disabled_stays_put() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    assert_eq!(next_target(&config), None);
}

// ── soonest_resume ───────────────────────────────────────────────────────────
//
// All-exhausted caption data source (issue #10 follow-up): names the chain
// member that resumes soonest, valid only when EVERY member is currently
// exhausted.

/// ISO reset `secs` in the future.
fn reset_in(secs: i64) -> String {
    epoch_secs_to_iso(now_epoch_secs() + secs)
}

#[test]
fn soonest_resume_empty_chain_is_none() {
    let config = config_with_chain(vec![], "a");
    assert_eq!(soonest_resume(&config), None);
}

#[test]
fn soonest_resume_picks_the_soonest_reset() {
    let config = config_with_chain(
        vec![
            profile_with_usage(
                "a",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(reset_in(3600)))))),
            ),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(reset_in(1800)))))),
            ),
        ],
        "a",
    );
    let (name, eta) = soonest_resume(&config).expect("all exhausted");
    assert_eq!(name, "b", "b resets sooner than a");
    assert!((1700..=1800).contains(&eta), "eta ~1800s, got {eta}");
}

// Ties on `resets_at` keep the earlier chain-order member.
#[test]
fn soonest_resume_ties_keep_earlier_chain_order() {
    let reset = reset_in(3600);
    let config = config_with_chain(
        vec![
            profile_with_usage(
                "a",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(reset.clone()))))),
            ),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(reset))))),
            ),
        ],
        "a",
    );
    let (name, _) = soonest_resume(&config).expect("all exhausted");
    assert_eq!(name, "a", "a tie keeps the earlier chain-order member");
}

// b's utilization is below its own threshold — already recovered, so the
// all-exhausted premise doesn't hold; recovery would relink it next tick.
#[test]
fn soonest_resume_none_when_one_member_recovered() {
    let config = config_with_chain(
        vec![
            profile_with_usage(
                "a",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(reset_in(3600)))))),
            ),
            profile_with_util("b", Some(95.0), Some(10.0)),
        ],
        "a",
    );
    assert_eq!(soonest_resume(&config), None);
}

// b's 5h window already reset — headroom again whatever its stale util says
// (same wall-clock rule `find_recovered_member`/`is_exhausted` use).
#[test]
fn soonest_resume_none_when_a_member_window_expired() {
    let config = config_with_chain(
        vec![
            profile_with_usage(
                "a",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(reset_in(3600)))))),
            ),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(expired_reset()))))),
            ),
        ],
        "a",
    );
    assert_eq!(soonest_resume(&config), None);
}

// A chain member with no resolvable profile (deleted, still listed) can't be
// proven exhausted — bail rather than pick around it.
#[test]
fn soonest_resume_none_when_chain_member_missing_profile() {
    let mut config = config_with_chain(
        vec![profile_with_usage(
            "a",
            Some(95.0),
            Some(usage_info(Some(window(100.0, Some(reset_in(3600)))))),
        )],
        "a",
    );
    config.state.fallback_chain.push("ghost".into());
    assert_eq!(soonest_resume(&config), None);
}

// next_auto_switch_target: scheduler-side wrap-off → Off when chain spent.
#[test]
fn auto_switch_wrap_off_switches_off_when_chain_spent() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.state.wrap_off = true;
    let snap = snapshot_chain(&config).expect("snapshot");
    assert!(snap.wrap_off);
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]); // both over 95% threshold, no sink → Off
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::Off),
    );
}

// ── recovery_target ──────────────────────────────────────────────────────────
//
// After switch-off-all (no active profile), find a chain member whose
// utilization has dropped below its threshold.

#[test]
fn find_recovered_returns_first_member_below_threshold() {
    let members = vec![
        ChainMember {
            name: "a".into(),
            threshold: 95.0,
            last_resort: false,
        },
        ChainMember {
            name: "b".into(),
            threshold: 95.0,
            last_resort: false,
        },
    ];
    let store = store_with_utils(&[("a", 100.0), ("b", 40.0)]);
    assert_eq!(
        find_recovered_member(&members, &store),
        Some("b".to_string()),
    );
}

#[test]
fn find_recovered_skips_exhausted_members() {
    let members = vec![
        ChainMember {
            name: "a".into(),
            threshold: 95.0,
            last_resort: false,
        },
        ChainMember {
            name: "b".into(),
            threshold: 95.0,
            last_resort: false,
        },
    ];
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]);
    assert_eq!(find_recovered_member(&members, &store), None);
}

#[test]
fn find_recovered_returns_none_when_no_member_has_data() {
    let members = vec![
        ChainMember {
            name: "a".into(),
            threshold: 95.0,
            last_resort: false,
        },
        ChainMember {
            name: "b".into(),
            threshold: 95.0,
            last_resort: false,
        },
    ];
    let store = store_with_utils(&[]); // no usage data for any member
    assert_eq!(find_recovered_member(&members, &store), None);
}

#[test]
fn find_recovered_uses_threshold_per_member() {
    let members = vec![
        ChainMember {
            name: "a".into(),
            threshold: 90.0,
            last_resort: false,
        }, // 95% util ≥ 90 → exhausted
        ChainMember {
            name: "b".into(),
            threshold: 95.0,
            last_resort: false,
        }, // 94% util < 95 → recovered
    ];
    let store = store_with_utils(&[("a", 95.0), ("b", 94.0)]);
    assert_eq!(
        find_recovered_member(&members, &store),
        Some("b".to_string()),
    );
}

// A member whose 5h window passed its reset has headroom again whatever its
// last-known utilization says — the wall clock recovered it (issue #2: a sole
// switched-off profile stayed "spent" forever on a stale 100% snapshot).
#[test]
fn find_recovered_recovers_when_window_expired() {
    let members = vec![ChainMember {
        name: "a".into(),
        threshold: 95.0,
        last_resort: false,
    }];
    let store = store_with_infos(vec![(
        "a",
        usage_info(Some(window(100.0, Some(expired_reset())))),
    )]);
    assert_eq!(
        find_recovered_member(&members, &store),
        Some("a".to_string()),
    );
}

// A fetched entry with no 5h window at all (idle account after a reset) is
// recovered; only an ABSENT store entry stays undecidable.
#[test]
fn find_recovered_recovers_when_windowless() {
    let members = vec![ChainMember {
        name: "a".into(),
        threshold: 95.0,
        last_resort: false,
    }];
    let store = store_with_infos(vec![("a", usage_info(None))]);
    assert_eq!(
        find_recovered_member(&members, &store),
        Some("a".to_string()),
    );
}

// No resets_at means the window can't be proven live, so it can't hold the
// member exhausted — same reading `five_hour_live` gives the auto-start leg.
#[test]
fn find_recovered_treats_missing_resets_at_as_lapsed() {
    let members = vec![ChainMember {
        name: "a".into(),
        threshold: 95.0,
        last_resort: false,
    }];
    let store = store_with_infos(vec![("a", usage_info(Some(window(100.0, None))))]);
    assert_eq!(
        find_recovered_member(&members, &store),
        Some("a".to_string()),
    );
}

// The scheduler-side gate must not switch away from an active whose spent
// window already reset — stale-high utilization, wall clock passed.
#[test]
fn auto_switch_ignores_expired_window_active() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(expired_reset()))))),
        ("b", usage_info(Some(window(50.0, Some(live_reset()))))),
    ]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

// A chain member whose 100% window lapsed is a viable switch target again.
#[test]
fn next_target_accepts_member_with_expired_window() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(expired_reset()))))),
            ),
        ],
        "a",
    );
    assert_eq!(next_target(&config), Some(SwitchAction::To("b".into())));
}

// next_auto_switch_target: wrap_off off, spent chain → legacy None.
#[test]
fn auto_switch_wrap_off_disabled_stays_put() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}
