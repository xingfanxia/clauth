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
