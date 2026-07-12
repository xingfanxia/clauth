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
    assert_eq!(next_target(&config, None), None);
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
        next_target(&config, None),
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
    assert_eq!(next_target(&config, None), None);
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
        next_target(&config, None),
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
    assert_eq!(next_target(&config, None), None);
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
    assert_eq!(next_target(&config, None), Some(SwitchAction::Off));
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
    assert_eq!(next_target(&config, None), Some(SwitchAction::Off));
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
    assert_eq!(next_target(&config, None), Some(SwitchAction::Off));
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
        next_target(&config, None),
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
    assert_eq!(next_target(&config, None), None);
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
    assert_eq!(next_target(&config, None), None);
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
        find_recovered_member(&members, &store, 98.0),
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
    assert_eq!(find_recovered_member(&members, &store, 98.0), None);
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
    assert_eq!(find_recovered_member(&members, &store, 98.0), None);
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
        find_recovered_member(&members, &store, 98.0),
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
        find_recovered_member(&members, &store, 98.0),
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
        find_recovered_member(&members, &store, 98.0),
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
        find_recovered_member(&members, &store, 98.0),
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
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("b".into()))
    );
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

// ── AUTH-1: auth-broken members excluded from the chain walk ──────────────────
//
// A member whose OAuth refresh is revoked/invalid (`AppState::auth_broken`) must
// never be picked as a switch target — installing its dead token would log out
// every running claude (Incident C). Excluded in the headroom pass AND the sink
// pass, on both the config-side (`next_target`) and scheduler-side
// (`next_auto_switch_target`) walks.

// next_target: broken member with headroom is skipped; the next viable one wins.
#[test]
fn next_target_skips_broken_member_picks_next() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)), // active, exhausted
            profile_with_util("b", Some(95.0), Some(20.0)),  // headroom but broken
            profile_with_util("c", Some(95.0), Some(20.0)),  // headroom, viable
        ],
        "a",
    );
    config.set_auth_broken("b", true);
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".into()))
    );
}

// next_target: the only non-active member is broken → nothing viable → None.
#[test]
fn next_target_returns_none_when_only_alternative_is_broken() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)), // active, exhausted
            profile_with_util("b", Some(95.0), Some(20.0)),  // headroom but broken
        ],
        "a",
    );
    config.set_auth_broken("b", true);
    assert_eq!(next_target(&config, None), None);
}

// next_auto_switch_target: the scheduler-side walk skips broken members too
// (snapshot_chain carries `auth_broken` into ChainSnapshot.broken).
#[test]
fn auto_switch_skips_broken_member_picks_next() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
            profile_with_util("c", Some(95.0), None),
        ],
        "a",
    );
    config.set_auth_broken("b", true);
    let snap = snapshot_chain(&config).expect("snapshot");
    assert!(snap.broken.iter().any(|n| n == "b"));
    let store = store_with_utils(&[("a", 100.0), ("b", 10.0), ("c", 10.0)]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("c".into())),
    );
}

// A broken 100%-sink is excluded from the sink pass: with wrap-off on and no
// other viable member, the chain switches OFF rather than installing the dead
// sink's token.
#[test]
fn next_target_broken_sink_wrap_off_switches_off() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)), // active, exhausted, not a sink
            profile_with_util("b", Some(100.0), Some(100.0)), // sink — but broken
        ],
        "a",
    );
    config.state.wrap_off = true;
    config.set_auth_broken("b", true);
    assert_eq!(next_target(&config, None), Some(SwitchAction::Off));
}

// ── AUTH-4: an auth-broken ACTIVE is itself a switch trigger ──────────────────
//
// A dead login can never produce a fresh usage read again, so its store entry
// freezes at whatever it last held — typically a lapsed 5h window that reads
// as idle headroom. Gating the walk on the active's exhaustion therefore
// wedged the daemon on the dead account forever while a viable sibling idled
// (observed 2026-07-09: the active broke mid-window; the next member sat at
// 0%). The broken flag is terminal-confirmed (set only after a rejected
// refresh AND a failed live-mirror adopt), so walking away is the only move
// that keeps the machine serving.

// Broken active whose frozen last read is a lapsed window (reads as idle
// headroom, NOT exhausted) → the walk runs anyway and leaves for the sibling.
#[test]
fn auto_switch_broken_active_walks_away_despite_stale_headroom() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.set_auth_broken("a", true);
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        // The active's last-ever read: maxed on a window that has since
        // lapsed — the exact frozen shape the wedge held.
        ("a", usage_info(Some(window(100.0, Some(expired_reset()))))),
        ("b", usage_info(Some(window(10.0, Some(live_reset()))))),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".into())),
    );
}

// Broken active with NO viable member: stays put. Wrap-off in particular must
// not fire off the broken flag alone — the live session's own Keychain chain
// may still be healthy, and only real usage exhaustion may halt it.
#[test]
fn auto_switch_broken_active_without_viable_member_never_wraps_off() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.state.wrap_off = true;
    config.set_auth_broken("a", true);
    config.set_auth_broken("b", true); // the only sibling is dead too
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(expired_reset()))))),
        ("b", usage_info(Some(window(10.0, Some(live_reset()))))),
    ]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

// Broken AND genuinely exhausted (live window over threshold): the Off leg
// keys on the real exhaustion, which broken must not suppress either.
#[test]
fn auto_switch_broken_and_exhausted_active_still_wraps_off() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.state.wrap_off = true;
    config.set_auth_broken("a", true);
    config.set_auth_broken("b", true);
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", usage_info(Some(window(100.0, Some(live_reset()))))),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::Off),
    );
}

// The last-resort pass excludes a broken member too (not just the headroom
// pass): a `last_resort` sink whose token is dead must not be migrated to. The
// base case migrates a→b; quarantining b makes the last-resort walk find
// nothing.
#[test]
fn next_target_skips_broken_last_resort_member() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)), // active, exhausted, not last_resort
            mark_last_resort(profile_with_util("b", Some(80.0), Some(100.0))), // last_resort sink
        ],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("b".into())),
        "base case: the last-resort pass migrates to the sink"
    );
    config.set_auth_broken("b", true);
    assert_eq!(
        next_target(&config, None),
        None,
        "a broken last_resort sink is excluded from the last-resort pass"
    );
}

// ── issue #8 follow-up b: burn-aware auto-switch (opt-in, default off) ─────
//
// `is_exhausted_projected`/`is_exhausted_active`/`is_exhausted_active_from_store`
// only ever change the ACTIVE profile's own exhaustion decision; candidate
// selection stays on the static `is_exhausted`/`is_exhausted_from_store`
// tested above (unchanged by this section).

#[test]
fn is_exhausted_projected_none_burn_falls_back_to_static_threshold() {
    assert!(
        is_exhausted_projected(96.0, 95.0, None, 90_000),
        "over threshold, no rate available → static check fires"
    );
    assert!(
        !is_exhausted_projected(90.0, 95.0, None, 90_000),
        "under threshold, no rate available → static check doesn't fire"
    );
}

#[test]
fn is_exhausted_projected_heavy_burn_fires_before_static_threshold() {
    // 90% is under the 95% static threshold, but a 1200 %/h burn over a 90s
    // poll projects to 120% — the cap fires ahead of the static check.
    assert!(is_exhausted_projected(90.0, 95.0, Some(1200.0), 90_000));
}

#[test]
fn is_exhausted_projected_light_burn_runs_past_static_threshold() {
    // 96% is already over the 95% static threshold, but a light 4 %/h burn
    // over a 90s poll barely moves — nowhere near the 100% cap, so burn-aware
    // mode keeps running where mode-off would already have switched.
    assert!(!is_exhausted_projected(96.0, 95.0, Some(4.0), 90_000));
}

#[test]
fn is_exhausted_active_mode_off_matches_static_is_exhausted() {
    // Mode off must reproduce `is_exhausted` bit for bit — no divergence from
    // today's static behavior regardless of what a rate would otherwise say.
    let exhausted = profile_with_util("a", Some(95.0), Some(100.0));
    let headroom = profile_with_util("a", Some(95.0), Some(50.0));
    assert_eq!(
        is_exhausted_active(&exhausted, false, 90_000, None, 98.0),
        is_exhausted(&exhausted, 98.0)
    );
    assert_eq!(
        is_exhausted_active(&headroom, false, 90_000, None, 98.0),
        is_exhausted(&headroom, 98.0)
    );
    assert!(is_exhausted_active(&exhausted, false, 90_000, None, 98.0));
    assert!(!is_exhausted_active(&headroom, false, 90_000, None, 98.0));
}

// Burn-aware ON but no rate available (fresh profile / first tick, or the
// caller's in-memory history is empty): falls back to the same static
// comparison mode-off uses — never leaves an account uncovered for lack of
// data. `is_exhausted_active` takes the rate as a parameter and never reads
// disk itself (that's the caller's job — `App::active_burn_rate` in-memory on
// the UI thread, `fallback::burn_rate_for_profile` on disk for the scheduler),
// so this needs no sandboxed HOME.
#[test]
fn is_exhausted_active_burn_aware_falls_back_without_rate() {
    let exhausted = profile_with_util("a", Some(95.0), Some(100.0));
    let headroom = profile_with_util("a", Some(95.0), Some(50.0));
    assert!(is_exhausted_active(&exhausted, true, 90_000, None, 98.0));
    assert!(!is_exhausted_active(&headroom, true, 90_000, None, 98.0));
}

// Same fallback, exercised through the full `next_target` entry point (wrap-off
// path) rather than the `is_exhausted_active` unit — pins that mode-on with no
// rate available agrees with mode-off all the way through the public walk.
#[test]
fn next_target_burn_aware_none_rate_falls_back_to_static_threshold() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    config.state.wrap_off = true;
    config.state.burn_aware_switching = true;
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::Off),
        "no rate available → static 100% >= 95% threshold fires, same as mode off"
    );
}

/// Writes `entries` as `usage_history.jsonl` lines for `name` — the on-disk
/// shape `crate::profile::load_usage_history` parses. Takes the sandbox so the
/// write can only land in a sandboxed home, never the real one.
fn write_history(_home: &crate::testutil::HomeSandbox, name: &str, entries: &[(u64, UsageInfo)]) {
    let path = crate::profile::profile_history_path(name).expect("history path");
    std::fs::create_dir_all(path.parent().expect("parent dir")).expect("mkdir");
    let mut body = String::new();
    for (ts, usage) in entries {
        let line = serde_json::json!({ "ts": ts, "name": name, "usage": usage });
        body.push_str(&serde_json::to_string(&line).expect("serialize history line"));
        body.push('\n');
    }
    std::fs::write(&path, body).expect("write history");
}

// End-to-end proof that the UI-thread walk (`next_target`) and the
// scheduler-side walk (`next_auto_switch_target`) agree: a heavy burn on the
// active flips the wrap-off decision from "stay" to "switch off" before it
// ever reaches its 95% static threshold, on both paths.
//
// `b` is pinned exhausted (100%, no `last_resort`) on both members so the
// headroom-walk and last-resort-walk (unaffected by burn-aware mode by
// design) both come up empty either way; the only thing that can move the
// outcome is the ACTIVE-only decision this issue changes — the wrap-off
// Off-check inside `next_target`, and the entry gate inside
// `next_auto_switch_target`.
#[test]
fn burn_aware_heavy_burn_flips_wrap_off_decision_on_both_walks() {
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    // Perfectly linear climb, 30 → 90 over 6 minutes = 600 %/h.
    write_history(
        &_home,
        "a",
        &[
            (
                now - 360_000,
                usage_info(Some(window(30.0, Some(live_reset())))),
            ),
            (
                now - 240_000,
                usage_info(Some(window(50.0, Some(live_reset())))),
            ),
            (
                now - 120_000,
                usage_info(Some(window(70.0, Some(live_reset())))),
            ),
        ],
    );

    // Static (mode off): 90% is still under the 95% threshold — active isn't
    // exhausted yet, so wrap-off's Off-check never fires.
    let mut static_config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(90.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    static_config.state.wrap_off = true;
    assert_eq!(
        next_target(&static_config, None),
        None,
        "static mode: 90% is still under the 95% threshold, active stays"
    );

    // `next_target` takes the burn rate as a parameter — it never reads disk
    // itself. Source it the same way `burn_rate_for_profile` does here,
    // standing in for the caller's in-memory `history_cache` lookup
    // (`App::active_burn_rate`) that feeds the real UI-thread call site.
    let active_window = window(90.0, Some(live_reset()));
    let rate = burn_rate_for_profile("a", &active_window).expect("rate computed from history");
    assert!((rate - 600.0).abs() < 1.0, "expected ~600 %/h, got {rate}");

    // Burn-aware: 90% + 600 %/h over a 90s poll projects to 105% — the active
    // is judged exhausted a full 5 points before the static threshold, and
    // with `b` exhausted too and no sink, wrap-off switches off.
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(90.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    config.state.wrap_off = true;
    config.state.burn_aware_switching = true;
    config.state.refresh_interval_ms = 90_000;
    assert_eq!(
        next_target(&config, Some(rate)),
        Some(SwitchAction::Off),
        "burn-aware mode: heavy burn projects past 100% within one poll, no sink → Off"
    );

    let snap = snapshot_chain(&config).expect("snapshot");
    assert!(snap.wrap_off);
    assert!(snap.burn_aware);
    assert_eq!(snap.interval_ms, 90_000);
    let store = store_with_utils(&[("a", 90.0), ("b", 100.0)]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::Off),
        "scheduler-side walk agrees with next_target under burn-aware mode"
    );
}

// ── weekly exhaustion (7d soft line, 98%) ─────────────────────────────────────
//
// A weekly-dead account's 5h window drains and then LAPSES (no live reset), so
// the 5h-only predicates read it as the freshest member in the chain — the
// observed 2026-07-08 bug: auto-fallback switched INTO a 7d=100 account, and
// recovery kept relinking it every time its 5h window rolled over. 2026-07-12
// lowered the line from the 100% hard cap to a 98% soft line: EITHER window
// crossing its line triggers the hop, while there is still room to land it.

/// UsageInfo with both windows populated explicitly.
fn usage_both(five_hour: Option<UsageWindow>, seven_day: Option<UsageWindow>) -> UsageInfo {
    UsageInfo {
        five_hour,
        seven_day,
        ..UsageInfo::default()
    }
}

/// A profile whose weekly window is spent to the cap and whose 5h window has
/// lapsed entirely — the live specimen's exact shape (5h resets_at: null).
fn weekly_dead_profile(name: &str) -> Profile {
    profile_with_usage(
        name,
        Some(95.0),
        Some(usage_both(None, Some(window(100.0, Some(live_reset()))))),
    )
}

#[test]
fn weekly_dead_member_is_never_a_fallback_target() {
    // a (active, 5h exhausted) → b (7d=100, 5h lapsed) → c (fresh): the walk
    // must land on c. Before the weekly gate, b's lapsed 5h read as headroom.
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(97.0)),
            weekly_dead_profile("b"),
            profile_with_util("c", Some(95.0), Some(10.0)),
        ],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".into()))
    );
}

#[test]
fn weekly_dead_active_is_exhausted_despite_idle_5h() {
    // The mirror direction: an ACTIVE account whose week is spent must count
    // exhausted — its 5h window never grows again (requests are refused), so
    // the 5h trigger alone would leave the daemon parked on a dead account.
    let p = weekly_dead_profile("a");
    assert!(is_exhausted(&p, 98.0));
    // Hard block trumps both burn-aware modes (nothing left to project).
    assert!(is_exhausted_active(&p, false, 90_000, None, 98.0));
    assert!(is_exhausted_active(&p, true, 90_000, Some(5.0), 98.0));
}

#[test]
fn weekly_soft_line_gates_below_it_and_lapsed_resets_renew() {
    // The weekly side is a soft line at 98%, not a 100% hard cap (2026-07-12):
    // an account riding 98%+ of its week bricks for DAYS the moment it tops
    // out, so the switch must happen while there is still room to land it —
    // waiting for the API to start refusing means dying mid-session.
    let below = profile_with_usage(
        "a",
        Some(95.0),
        Some(usage_both(None, Some(window(97.9, Some(live_reset()))))),
    );
    assert!(
        !is_exhausted(&below, 98.0),
        "97.9% weekly still has headroom"
    );
    let at_line = profile_with_usage(
        "a",
        Some(95.0),
        Some(usage_both(None, Some(window(98.0, Some(live_reset()))))),
    );
    assert!(
        is_exhausted(&at_line, 98.0),
        "98% weekly counts as exhausted"
    );
    // A 7d window whose reset has PASSED is a renewed quota, not a block.
    let renewed = profile_with_usage(
        "a",
        Some(95.0),
        Some(usage_both(None, Some(window(100.0, Some(expired_reset()))))),
    );
    assert!(!is_exhausted(&renewed, 98.0));
}

#[test]
fn weekly_soft_exhausted_active_triggers_a_switch_despite_5h_headroom() {
    // The user-reported gap (2026-07-12): active at 5h 40% / 7d 98.5% sat
    // unswitched until the weekly cap bricked it. EITHER window crossing its
    // line must trigger the hop.
    let active = profile_with_usage(
        "a",
        Some(95.0),
        Some(usage_both(
            Some(window(40.0, Some(live_reset()))),
            Some(window(98.5, Some(live_reset()))),
        )),
    );
    assert!(
        is_exhausted(&active, 98.0),
        "7d 98.5% triggers despite 5h 40%"
    );
    assert!(is_exhausted_active(&active, false, 90_000, None, 98.0));
    let config = config_with_chain(
        vec![active, profile_with_util("b", Some(95.0), Some(10.0))],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("b".into()))
    );
}

#[test]
fn weekly_soft_member_is_not_a_target() {
    // Symmetric: hopping INTO a 98%+ weekly member just re-triggers next tick.
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(97.0)),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_both(
                    Some(window(10.0, Some(live_reset()))),
                    Some(window(98.5, Some(live_reset()))),
                )),
            ),
            profile_with_util("c", Some(95.0), Some(10.0)),
        ],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".into()))
    );
}

#[test]
fn weekly_dead_member_is_skipped_by_the_store_walk_too() {
    // Scheduler-side twin: next_auto_switch_target reads the UsageStore.
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
            profile_with_util("c", Some(95.0), None),
        ],
        "a",
    );
    let snapshot = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(97.0, Some(live_reset()))))),
        (
            "b",
            usage_both(None, Some(window(100.0, Some(live_reset())))),
        ),
        ("c", usage_info(Some(window(10.0, Some(live_reset()))))),
    ]);
    assert_eq!(
        next_auto_switch_target(&snapshot, &store),
        Some(SwitchAction::To("c".into()))
    );
}

#[test]
fn weekly_dead_member_never_recovers() {
    // Recovery's "5h lapsed = idle headroom" rule is exactly how a weekly-dead
    // member kept getting relinked every 5h rollover. It must stay out until
    // the WEEKLY reset passes.
    let members = vec![ChainMember {
        name: "b".into(),
        threshold: 95.0,
        last_resort: false,
    }];
    let dead = store_with_infos(vec![(
        "b",
        usage_both(None, Some(window(100.0, Some(live_reset())))),
    )]);
    assert_eq!(find_recovered_member(&members, &dead, 98.0), None);
    // Same member with the weekly reset in the past HAS recovered.
    let renewed = store_with_infos(vec![(
        "b",
        usage_both(None, Some(window(100.0, Some(expired_reset())))),
    )]);
    assert_eq!(
        find_recovered_member(&members, &renewed, 98.0),
        Some("b".to_string())
    );
}

#[test]
fn soonest_resume_uses_the_weekly_reset_for_a_weekly_dead_member() {
    // The all-exhausted caption must not promise a 5h comeback for an account
    // that is blocked until its WEEKLY reset (nor bail to no caption at all
    // because the 5h window is absent).
    let weekly_reset = epoch_secs_to_iso(now_epoch_secs() + 48 * 3600);
    let five_hour_reset = epoch_secs_to_iso(now_epoch_secs() + 600);
    let config = config_with_chain(
        vec![
            profile_with_usage(
                "a",
                Some(95.0),
                Some(usage_both(Some(window(97.0, Some(five_hour_reset))), None)),
            ),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_both(None, Some(window(100.0, Some(weekly_reset))))),
            ),
        ],
        "a",
    );
    let (name, secs) = soonest_resume(&config).expect("caption data");
    // a's 10-minute 5h reset beats b's 48h weekly reset.
    assert_eq!(name, "a");
    assert!((500..700).contains(&secs), "got {secs}");
}

#[test]
fn weekly_line_is_configurable_chain_wide() {
    // Same roster, two configured lines: at 90 a member riding 7d 92% is
    // exhausted (excluded as a target, triggers as active); back at the 98
    // default it has headroom again. The accessor also rejects out-of-band
    // hand-edits rather than silently disabling the weekly gate.
    let mk = || {
        vec![
            profile_with_util("a", Some(95.0), Some(97.0)),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_both(
                    Some(window(10.0, Some(live_reset()))),
                    Some(window(92.0, Some(live_reset()))),
                )),
            ),
            profile_with_util("c", Some(95.0), Some(10.0)),
        ]
    };
    let mut config = config_with_chain(mk(), "a");
    config.state.weekly_switch_threshold = Some(90.0);
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".into())),
        "at a 90 line, b's 7d 92% is exhausted — walk lands on c"
    );
    let default_line = config_with_chain(mk(), "a");
    assert_eq!(
        next_target(&default_line, None),
        Some(SwitchAction::To("b".into())),
        "at the 98 default, b's 7d 92% is headroom"
    );
    let mut garbage = config_with_chain(mk(), "a");
    garbage.state.weekly_switch_threshold = Some(120.0);
    assert_eq!(garbage.state.weekly_switch_threshold_pct(), 98.0);
    assert_eq!(
        next_target(&garbage, None),
        Some(SwitchAction::To("b".into())),
        "an out-of-band hand-edit falls back to the default line"
    );
}
