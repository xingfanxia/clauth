#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

// The centralized diagnostics are load-bearing precisely because they render on
// three surfaces from one definition; these pin that one head reaches all three
// without drifting, and that the CLI/log and toast forms differ only in the
// head↔detail separator.

#[test]
fn login_expired_shares_one_head_across_line_and_toast() {
    let m = login_expired("work");
    assert_eq!(
        m.line(),
        "login for 'work' has expired: refresh token revoked or invalid: run clauth login work"
    );
    assert_eq!(
        m.toast(),
        "login for 'work' has expired\nrefresh token revoked or invalid: run clauth login work"
    );
    // The bold toast head is exactly the line() prefix before the separator.
    assert_eq!(
        m.toast().lines().next().unwrap(),
        "login for 'work' has expired"
    );
}

#[test]
fn refresh_transient_carries_the_error_in_the_detail() {
    let m = refresh_transient("flaky", "no network");
    assert_eq!(
        m.line(),
        "could not refresh 'flaky' before switching: no network: check your connection and retry"
    );
    // The head stays fixed-length regardless of the (arbitrary, possibly long)
    // error text, so it can never wrap the toast's bold first line.
    assert_eq!(
        m.toast().lines().next().unwrap(),
        "could not refresh 'flaky' before switching"
    );
    assert_eq!(
        m.toast().lines().nth(1).unwrap(),
        "no network: check your connection and retry"
    );
}

#[test]
fn line_and_toast_collapse_to_the_head_when_detail_is_absent() {
    let m = Message {
        head: "done".to_string(),
        detail: None,
    };
    assert_eq!(m.line(), "done");
    assert_eq!(m.toast(), "done");
}

#[test]
fn resolve_in_tui_names_the_clauth_surface() {
    assert!(RESOLVE_IN_TUI.contains("clauth TUI"));
}

#[test]
fn plan_label_marks_a_canceled_subscription() {
    let canceled = PlanInfo {
        tier: PlanTier::Free,
        subscription_status: Some("canceled".to_string()),
    };
    assert_eq!(plan_label(&canceled), "Claude Free · canceled");

    // A genuine, never-subscribed free account carries no canceled marker.
    let free = PlanInfo {
        tier: PlanTier::Free,
        subscription_status: None,
    };
    assert_eq!(plan_label(&free), "Claude Free");
}
