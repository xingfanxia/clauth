#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::profile_cache::write_profile_cache;
use crate::testutil::{HomeSandbox, blank_profile};
use crate::usage::{PlanInfo, PlanTier};

/// `tier_label` feeds both the MCP `list_profiles` and `which` tier fields, and
/// reads straight off `usage_cache.json` — never a live fetch. A profile
/// canceled in a prior session already carries `subscription_status: "canceled"`
/// in that cache, so the canceled hint must show on a cold start with no
/// network call at all.
#[test]
fn tier_label_reports_canceled_from_a_prior_sessions_cache() {
    let _home = HomeSandbox::new();
    let profile = blank_profile("kerry");
    let usage = UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: Some("canceled".to_string()),
        }),
        ..Default::default()
    };
    write_profile_cache("kerry", USAGE_CACHE_FILE, &usage);

    assert_eq!(tier_label(&profile), Some("canceled".to_string()));
}

/// Regression guard the other direction: an un-canceled cached plan still
/// reports its real tier, not a false "canceled".
#[test]
fn tier_label_reports_the_real_tier_when_not_canceled() {
    let _home = HomeSandbox::new();
    let profile = blank_profile("kerry");
    let usage = UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Max(Some(5)),
            subscription_status: None,
        }),
        ..Default::default()
    };
    write_profile_cache("kerry", USAGE_CACHE_FILE, &usage);

    assert_eq!(tier_label(&profile), Some("Max 5x".to_string()));
}
