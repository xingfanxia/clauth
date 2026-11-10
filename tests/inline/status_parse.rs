//! Tests for the status feed parser (`src/status.rs`), linked via `#[path]`.
//!
//! The fixture is a trimmed `incidents.json` capture: a resolved multi-update
//! incident with component transitions (`6ptd5skgmy3v`), a maintenance incident
//! (`in_progress` / `scheduled`), and one with an unknown status + impact to
//! exercise the enum fallbacks.

use super::*;

const FIXTURE: &str = include_str!("../fixtures/incidents.json");

#[test]
fn parses_core_fields() {
    let incidents = parse_incidents(FIXTURE).expect("fixture parses");
    assert_eq!(incidents.len(), 3, "three incidents in the fixture");

    let first = &incidents[0];
    assert_eq!(first.id, "6ptd5skgmy3v");
    assert_eq!(first.title, "Elevated errors on Claude Opus 4.8");
    assert_eq!(first.link, "https://stspg.io/32dgd2bnmh8z");
    assert_eq!(first.phase, UpdatePhase::Resolved);
    assert_eq!(first.updates.len(), 2);
}

#[test]
fn maps_impact_including_unknown_fallback() {
    let incidents = parse_incidents(FIXTURE).expect("fixture parses");
    assert_eq!(incidents[0].impact, Impact::Minor);
    assert_eq!(incidents[1].impact, Impact::Maintenance);
    // `catastrophic` is unknown → Other(lowercased), never an error.
    assert_eq!(
        incidents[2].impact,
        Impact::Other("catastrophic".to_string())
    );
}

#[test]
fn maps_maintenance_statuses() {
    let incidents = parse_incidents(FIXTURE).expect("fixture parses");
    let maint = &incidents[1];
    assert_eq!(maint.phase, UpdatePhase::InProgress);
    assert_eq!(maint.updates[0].phase, UpdatePhase::InProgress);
    assert_eq!(maint.updates[1].phase, UpdatePhase::Scheduled);
    assert!(maint.is_active(), "in_progress is not terminal");
}

#[test]
fn unknown_update_status_falls_back_to_other() {
    let incidents = parse_incidents(FIXTURE).expect("fixture parses");
    let weird = &incidents[2];
    assert_eq!(
        weird.updates[0].phase,
        UpdatePhase::Other("surprise".into())
    );
}

#[test]
fn parses_ms_fraction_timestamps() {
    let incidents = parse_incidents(FIXTURE).expect("fixture parses");
    // `.SSS` fraction is tolerated; seconds truncate (no rounding).
    // 2026-06-06T18:37:44.116Z → 1780771064 s.
    assert_eq!(incidents[0].started_ms, 1_780_771_064_000);
    // resolved_at 2026-06-06T19:05:27.178Z.
    assert_eq!(incidents[0].resolved_ms, Some(1_780_772_727_000));
    // update display_at carries the ms fraction too.
    assert_eq!(incidents[0].updates[0].at_ms, 1_780_772_727_000);
}

#[test]
fn nullable_resolved_at_is_none() {
    let incidents = parse_incidents(FIXTURE).expect("fixture parses");
    // The maintenance incident is unresolved (`resolved_at: null`).
    assert_eq!(incidents[1].resolved_ms, None);
}

#[test]
fn transitions_filtered_to_actual_changes() {
    let incidents = parse_incidents(FIXTURE).expect("fixture parses");

    // First incident's resolved update: both components flip degraded → operational.
    let trans = &incidents[0].updates[0].transitions;
    assert_eq!(trans.len(), 2, "both changed components kept");
    assert_eq!(
        trans[0],
        (
            "claude.ai".to_string(),
            "degraded_performance".to_string(),
            "operational".to_string()
        )
    );

    // The `weird` incident's only affected_component has old == new → dropped.
    assert!(
        incidents[2].updates[0].transitions.is_empty(),
        "no-op component change is filtered out"
    );
}

#[test]
fn null_affected_components_yields_no_transitions() {
    let incidents = parse_incidents(FIXTURE).expect("fixture parses");
    // Maintenance update 0 has `affected_components: null`.
    assert!(incidents[1].updates[0].transitions.is_empty());
    // Update 1 has a real change (operational → under_maintenance).
    assert_eq!(incidents[1].updates[1].transitions.len(), 1);
}

#[test]
fn collects_components_with_status_paren_stripped() {
    let incidents = parse_incidents(FIXTURE).expect("fixture parses");
    // Displayed status is FIRST-REPORTED, not the closing snapshot: both
    // components were operational→degraded in the oldest update (then recovered),
    // so the row shows `degraded_performance`, not the snapshot's `operational`.
    assert_eq!(
        incidents[0].components,
        vec![
            ("claude.ai".to_string(), "degraded_performance".to_string()),
            // `Claude API (api.anthropic.com)` → paren group stripped.
            ("Claude API".to_string(), "degraded_performance".to_string()),
        ]
    );
}

#[test]
fn strip_parens_cases() {
    // No parens — unchanged.
    assert_eq!(strip_parens("claude.ai"), "claude.ai");
    // One group, embedded — group dropped, leftover double space collapsed.
    assert_eq!(
        strip_parens("Claude Console (platform.claude.com)"),
        "Claude Console"
    );
    assert_eq!(strip_parens("Claude API (api.anthropic.com)"), "Claude API");
    // Trailing group with no inner spaces.
    assert_eq!(strip_parens("Foo (bar)"), "Foo");
    // Nested parens — the whole balanced span is dropped (depth-tracked).
    assert_eq!(strip_parens("Foo (bar (baz) qux)"), "Foo");
    // Unbalanced `(` — left as-is (only whitespace tidied).
    assert_eq!(strip_parens("Foo (bar"), "Foo (bar");
    // Unbalanced nested — depth never returns to zero, restored verbatim.
    assert_eq!(strip_parens("Foo (bar (baz)"), "Foo (bar (baz)");
}

#[test]
fn dedup_keeps_worst_status() {
    // Two raw names collapse to "Claude API"; the worse (major_outage) wins even
    // though operational came first.
    let json = r#"{
      "incidents": [
        { "id": "dup", "started_at": "2026-06-01T00:00:00.000Z",
          "components": [
            { "name": "Claude API (api.anthropic.com)", "status": "operational" },
            { "name": "Claude API (legacy)", "status": "major_outage" }
          ] }
      ]
    }"#;
    let incidents = parse_incidents(json).expect("parses");
    assert_eq!(
        incidents[0].components,
        vec![("Claude API".to_string(), "major_outage".to_string())],
        "name collision keeps the worst status, first-seen order"
    );
}

#[test]
fn component_status_is_first_reported_not_snapshot() {
    // A resolved incident: component flips operational→degraded in the OLDEST
    // update, then degraded→operational in the NEWEST (the recovery). The wire
    // snapshot is operational (closed), but the row must show what it FIRST
    // reported: degraded_performance.
    let json = r#"{
      "incidents": [
        { "id": "first", "status": "resolved", "impact": "minor",
          "started_at": "2026-06-01T00:00:00.000Z", "resolved_at": "2026-06-01T01:00:00.000Z",
          "incident_updates": [
            { "status": "resolved", "display_at": "2026-06-01T01:00:00.000Z",
              "affected_components": [
                { "name": "claude.ai", "old_status": "degraded_performance", "new_status": "operational" }
              ] },
            { "status": "investigating", "display_at": "2026-06-01T00:00:00.000Z",
              "affected_components": [
                { "name": "claude.ai", "old_status": "operational", "new_status": "degraded_performance" }
              ] }
          ],
          "components": [ { "name": "claude.ai", "status": "operational" } ] }
      ]
    }"#;
    let incidents = parse_incidents(json).expect("parses");
    assert_eq!(
        incidents[0].components,
        vec![("claude.ai".to_string(), "degraded_performance".to_string())],
        "displayed status is first-reported, not the closing snapshot"
    );
}

#[test]
fn component_status_falls_back_to_snapshot_without_transitions() {
    // A component that never appears in any transition keeps its snapshot status.
    let json = r#"{
      "incidents": [
        { "id": "snap", "status": "resolved",
          "started_at": "2026-06-01T00:00:00.000Z",
          "incident_updates": [
            { "status": "resolved", "display_at": "2026-06-01T01:00:00.000Z",
              "affected_components": null }
          ],
          "components": [ { "name": "claude.ai", "status": "under_maintenance" } ] }
      ]
    }"#;
    let incidents = parse_incidents(json).expect("parses");
    assert_eq!(
        incidents[0].components,
        vec![("claude.ai".to_string(), "under_maintenance".to_string())],
        "no transition → snapshot status fallback"
    );
}

#[test]
fn shorten_component_status_mapping() {
    assert_eq!(shorten_component_status("operational"), "operational");
    assert_eq!(shorten_component_status("degraded_performance"), "degraded");
    assert_eq!(shorten_component_status("partial_outage"), "partial outage");
    assert_eq!(shorten_component_status("major_outage"), "major outage");
    assert_eq!(shorten_component_status("under_maintenance"), "maintenance");
    // Unknown → underscores become spaces.
    assert_eq!(shorten_component_status("some_new_state"), "some new state");
}

#[test]
fn active_definition_excludes_resolved_and_completed() {
    let incidents = parse_incidents(FIXTURE).expect("fixture parses");
    assert!(!incidents[0].is_active(), "resolved is not active");
    assert!(incidents[1].is_active(), "in_progress is active");
    assert!(
        !incidents[2].is_active(),
        "resolved weird incident not active"
    );
}

#[test]
fn empty_response_yields_empty_vec() {
    let json = r#"{"page":{"id":"x"},"incidents":[]}"#;
    let incidents = parse_incidents(json).expect("empty response parses");
    assert!(incidents.is_empty());
}

#[test]
fn malformed_entry_is_skipped_not_panicked() {
    // First incident has no usable timestamp (started_at + created_at absent) →
    // skipped; the well-formed second survives.
    let json = r#"{
      "incidents": [
        { "id": "bad", "name": "no timestamps", "status": "resolved", "impact": "none",
          "incident_updates": [], "components": [] },
        { "id": "good", "name": "fine", "status": "resolved", "impact": "minor",
          "started_at": "2026-06-01T00:00:00.000Z", "resolved_at": "2026-06-01T00:10:00.000Z",
          "incident_updates": [], "components": [] }
      ]
    }"#;
    let incidents = parse_incidents(json).expect("parses with one bad entry");
    assert_eq!(incidents.len(), 1, "the timestamp-less entry is skipped");
    assert_eq!(incidents[0].id, "good");
}

#[test]
fn missing_optional_fields_default_gracefully() {
    // Minimal viable incident: only a start time and id.
    let json = r#"{
      "incidents": [
        { "id": "min", "started_at": "2026-06-01T00:00:00.000Z" }
      ]
    }"#;
    let incidents = parse_incidents(json).expect("parses minimal incident");
    assert_eq!(incidents.len(), 1);
    let inc = &incidents[0];
    assert_eq!(inc.title, "");
    assert!(inc.components.is_empty());
    assert!(inc.updates.is_empty());
    assert_eq!(inc.resolved_ms, None);
    // Empty status string → Other("") (renders as "update").
    assert_eq!(inc.phase, UpdatePhase::Other(String::new()));
}

#[test]
fn iso_ms_parse_correctness() {
    assert_eq!(iso_to_ms("1970-01-01T00:00:00.000Z"), Some(0));
    // 2026-06-06T18:37:44.116Z = 1780771064 s → ms.
    assert_eq!(
        iso_to_ms("2026-06-06T18:37:44.116Z"),
        Some(1_780_771_064_000)
    );
}

/// Statuspage tenants have been seen emitting `+00:00` instead of `Z`; a
/// format shift must not silently drop incidents.
#[test]
fn offset_form_timestamps_parse_like_z_form() {
    assert_eq!(
        iso_to_ms("2026-06-06T18:37:44.116+00:00"),
        iso_to_ms("2026-06-06T18:37:44.116Z")
    );
    let json = r#"{
      "incidents": [
        {
          "id": "off",
          "name": "offset form",
          "status": "resolved",
          "started_at": "2026-06-06T09:27:59.685+00:00",
          "resolved_at": "2026-06-06T10:14:41.534+00:00",
          "incident_updates": [
            { "status": "resolved", "body": "done", "display_at": "2026-06-06T10:14:41.534+00:00" }
          ]
        }
      ]
    }"#;
    let incidents = parse_incidents(json).expect("offset-form incident parses");
    assert_eq!(
        incidents.len(),
        1,
        "offset-form incident must not be dropped"
    );
    assert_eq!(
        Some(incidents[0].started_ms),
        iso_to_ms("2026-06-06T09:27:59.685Z"),
        "offset form must resolve to the same instant as the Z form"
    );
}

// ── fetch bounds ──────────────────────────────────────────────────────────────

/// A feed served promptly still parses in full under the end-to-end bound.
#[test]
fn a_feed_reads_a_body_served_within_the_bound() {
    let _home = crate::testutil::HomeSandbox::new();
    super::set_fetch_timeout_override(Some(std::time::Duration::from_millis(1000)));
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("local addr").to_string();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept one fetch");
        crate::testutil::read_request_head(&mut sock);
        let body = r#"{"incidents":[]}"#;
        let _ = std::io::Write::write_all(
            &mut sock,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
    });
    let incidents =
        super::fetch_feed_at(&format!("http://{addr}/")).expect("fetch within the bound");
    assert!(
        incidents.is_empty(),
        "the empty feed parses to no incidents"
    );
    super::set_fetch_timeout_override(None);
}

/// A body that never arrives fails the feed at the end-to-end bound instead of
/// hanging. Wrapped in a thread + `recv_timeout` so the pre-fix hang reads as a
/// red timeout, never a hung suite.
#[test]
fn a_stalled_body_fails_the_feed_at_the_end_to_end_bound() {
    let _home = crate::testutil::HomeSandbox::new();
    let bound = std::time::Duration::from_millis(200);
    super::set_fetch_timeout_override(Some(bound));
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("local addr").to_string();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept one fetch");
        crate::testutil::read_request_head(&mut sock);
        let _ = std::io::Write::write_all(
            &mut sock,
            b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n",
        );
        std::thread::sleep(std::time::Duration::from_secs(600));
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let url = format!("http://{addr}/");
    let started = std::time::Instant::now();
    std::thread::spawn(move || {
        let _ = tx.send(super::fetch_feed_at(&url));
    });
    let result = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the stalled body must fail the feed within the scaled bound");
    let elapsed = started.elapsed();
    crate::testutil::assert_one_budget_timeout(&result);
    assert!(
        elapsed >= std::time::Duration::from_millis(150),
        "the feed waits out the scaled bound, never a shorter hard-coded one: {elapsed:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "the feed fails at the 200 ms budget, not a longer hard-coded one: {elapsed:?}"
    );
    super::set_fetch_timeout_override(None);
}

/// The ruled numbers, pinned by equality: the scaled tests pin the SHAPE, these
/// pin what cloudy's 2026-10-21 ruling names, so retuning a constant cannot
/// ship green. `RECV_TIMEOUT` is gone by the same ruling (one budget).
#[test]
fn the_feed_bounds_are_the_ruled_numbers() {
    assert_eq!(
        super::FETCH_TIMEOUT,
        std::time::Duration::from_secs(30),
        "cloudy's 2026-10-21 ruling: status gets 30 s end to end"
    );
    assert_eq!(
        super::CONNECT_TIMEOUT,
        std::time::Duration::from_secs(5),
        "cloudy's 2026-10-21 ruling: connect is 5 s, then headers + body share the one budget"
    );
}

/// The feed carries ONE end-to-end budget and no separate header-phase deadline:
/// a re-added `timeout_recv_response` inside the builder would fail headers the
/// ruling says succeed (cloudy, 2026-10-21: "headers + body share 30 s"). Scoped
/// to the builder's span so a doc comment naming the method elsewhere in the
/// file cannot red it.
#[test]
fn the_feed_carries_one_end_to_end_budget_not_a_header_phase_bound() {
    let src = include_str!("../../src/status.rs");
    let builder = src
        .split_once("config_builder()")
        .and_then(|(_, rest)| rest.split_once(".build()").map(|(span, _)| span))
        .expect(
            "the feed builder span is found: \
             the_feed_carries_one_end_to_end_budget_not_a_header_phase_bound",
        );
    assert!(
        !builder.contains("timeout_recv_response"),
        "the one-budget ruling allows no separate header-phase deadline"
    );
}

/// Headers that take longer than the old header bound (10 s, scaled here to a
/// third of the 1000 ms budget, 333 ms) still succeed: they share the ONE
/// budget with the body, as the ruling names. Re-adding a per-phase header
/// bound reds here.
#[test]
fn headers_past_the_old_header_bound_still_succeed_under_the_one_budget() {
    let _home = crate::testutil::HomeSandbox::new();
    super::set_fetch_timeout_override(Some(std::time::Duration::from_millis(1000)));
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("local addr").to_string();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept one fetch");
        crate::testutil::read_request_head(&mut sock);
        // 400 ms: past 10 s scaled to this 1000 ms budget (333 ms), inside it.
        std::thread::sleep(std::time::Duration::from_millis(400));
        let body = r#"{"incidents":[]}"#;
        let _ = std::io::Write::write_all(
            &mut sock,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
    });
    let incidents = super::fetch_feed_at(&format!("http://{addr}/"))
        .expect("headers inside the one budget succeed, whatever their delay");
    assert!(
        incidents.is_empty(),
        "the empty feed parses to no incidents"
    );
    super::set_fetch_timeout_override(None);
}
