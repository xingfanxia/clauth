//! Self-checking feature coverage test.
//!
//! Parses the README's `## Features` list, cross-references each feature
//! against `FEATURE_MAP`, and verifies every referenced test function
//! still exists in the test tree. Fails when a feature has no covering
//! test or a referenced test doesn't exist.
//!
//! Run: `cargo test features_have_test_coverage`.

use std::collections::HashSet;

/// (bolded lead of a README `## Features` bullet → test fn name prefixes
/// that cover it)
///
/// One row per README bullet, so each row is a bucket covering everything
/// that bullet claims; the exhaustive per-subsystem reference lives in
/// `wiki/`. A row passes when EVERY prefix matches at least one function in
/// the test tree (substring match on the function name), so a deleted or
/// renamed test still reds here. Add a row when you add a README bullet;
/// add a prefix when a bullet starts claiming something new.
const FEATURE_MAP: &[(&str, &[&str])] = &[
    (
        // switching, login, delete, the non-destructive swap, which-am-i,
        // and the re-login divergence prompt.
        "Switch",
        &[
            "auto_switch",
            "snapshot_chain",
            "resolves_started_profile",
            "authorize_url",
            "pkce_challenge",
            "base64url_nopad",
            "login_route",
            "reauth_confirmed",
            "login_api_mode",
            "delete_takes_yes_and_force",
            "diverged_",
            "classify_link_",
            "first_login_",
            "build_runtime_dir_writes_settings_not_symlink",
            "session_profile_",
            "matches_profile_by_refresh_token",
            "token_match_",
            "relogin_is_diverged",
            "overwrite_confirm",
            "overwrite_cancel",
            // rolling session token (#59): the arm/restore verbs and the
            // sidecar state they manage — what a switch installs.
            "rolling_gate_",
            "stamp_rolling_token_writes",
            "first_stamp_preserves_the_mint",
            "restore_static_mint_round_trip",
        ],
    ),
    (
        // usage bars, plan detection, per-row activity, stale-data cues,
        // the token dashboard + its cost lens, the status feed.
        "Monitor",
        &[
            "parses_",
            "retry_after",
            "cached_fallback_does_not_clobber",
            "mark_window_open",
            "window_lapsed",
            "gap_boundary",
            "steady_linear_drain_exact_rate",
            "oauth_profile",
            "api_profile",
            "failed_profile",
            "all_tabs_render",
            "empty_state_renders",
            "parses_core_fields",
            "collects_components_with_status",
            "component_status_",
            "dedup_keeps_worst_status",
            "status_selected_row_tint",
            "base_stats_parsed",
            "today_bucket_aggregates",
            "top_up_adds_new_day",
            "group_models_keeps",
            "model_display_name",
            "distill_keeps",
            "rate_strips",
            "cost_sums",
            "total_cost_counts_unpriced",
        ],
    ),
    (
        "Auto-switch",
        &[
            "auto_switch_",
            "wrap_off_",
            "find_recovered_",
            "sink_active_",
            // Interleaved auto-start: membership, gap arithmetic, the
            // history-series classifier, the per-tick election, and the chip.
            // ONE prefix, and every test of the feature is rooted at it, so a
            // deleted test reds this row while nothing else in the test tree
            // can match by accident — a bare `queue_` would also match the
            // `build_status_auto_start_queue_` status tests, and `queue` alone
            // matches every name carrying that substring.
            "auto_start_queue_",
        ],
    ),
    (
        "Run in parallel",
        &[
            "acquire_creates_runtime_and_pid_file",
            "build_runtime_dir_credentials_not_from_claude_home",
            "acquire_isolates_credentials_from_real_home",
        ],
    ),
    (
        // the MCP server's tools, the bundled hooks, plus the Plugin tab that
        // proves the wiring.
        "From inside Claude",
        &[
            "every_bundled_hook_command_parses_as_a_subcommand",
            "the_first_fire_is_a_baseline_and_a_move_is_announced_once",
            "installed_records",
            "marketplace_known",
            "manual_mcp_wiring",
            "wire_mcp_server",
            "global_entry_drifted",
            "session_scope_resolves_the_tier_through_the_which_tiers",
            "valid_switch_repoints_active_through_the_blocking_task",
            "unknown_target_is_rejected_without_stripping_live_creds",
            "divergence_overwrite_captures_relogin_into_outgoing",
        ],
    ),
    (
        // the daemon loop and its status feed, plus the token rotation it
        // drives on every tick.
        "Headless",
        &[
            "build_status",
            "switch_valid_profile",
            "snapshot_reply_is_one_line",
            // fallback configuration over the socket (CBAR-2)
            "fallback_add_remove",
            "set_threshold_validates",
            "add_appends",
            "set_wrap_off_toggles",
            // Daemon::tick loop body characterization (TECH-5)
            "tick_with_empty_queues",
            "drain_pending_switch_executes",
            "drain_pending_switch_skips",
            // UPS-18: `reload_if_changed_fires` went with the fork's codex
            // daemon block. The same behaviour is pinned by the drain test
            // (which calls reload_if_changed and asserts it fires) and by the
            // fingerprint tests that decide when it should.
            "drain_config_ops_threshold_does_not_suppress_external_reload",
            "reload_fingerprint",
            // single-fetcher lease (#27): exactly one instance fetches; every
            // other one stands down and hydrates from the shared cache.
            "standdown_",
            "one_holder_at_a_time",
            // singleton ceiling (#57): one active + one standby, no pile-up.
            "third_instance_is_redundant",
            "no_standby_exits_rather_than",
            "tick_stands_down_when_another",
            "held_lock_with_fresh_status",
            "rotate_one",
            "live_session_included",
            "force_true_bypasses",
            "rotation_guard_is_independent",
            // `--listen`: the REST API the Headless bullet claims serves the
            // feed and the switch to another machine. The bullet said that while
            // this list named none of it, so the routes, their auth, and the TLS
            // listener under them all counted as uncovered.
            "every_route_but_pair_refuses_an_unpaired_caller",
            "only_the_api_v1_prefix_is_served",
            "an_unknown_path_is_404_and_a_wrong_method_is_405",
            "status_serves_the_on_disk_feed_verbatim",
            "all_equals_one_reads_the_live_stores",
            "switch_relinks_and_reports_the_previous_account",
            "a_second_concurrent_switch_is_refused_immediately",
            "a_revoked_device_is_refused_on_its_next_request",
            "a_view_device_is_refused_every_control_route",
            "an_unknown_tier_device_is_refused_while_the_others_work",
            "the_connection_cap_admits_up_to_the_limit",
            "a_content_length_that_is_not_bare_digits",
            "cert_source_is_explicit_only_when_both_files_are_named",
            // the herdr panes, their terminal streams, the session history pages
            // and the prompt/keys routes the bullet names since T20.
            "the_fixture_panes_join_to_the_pinned_answer",
            "a_frame_arrives_over_the_bridge",
            "a_history_page_serves_the_records_verbatim_paged_backward",
            "the_pane_id_is_the_path_segment_decoded_once",
            "a_missing_certificate_fails_in_prepare_not_after_the_claim",
            // rolling session token (#59): the daemon leg — the tick that
            // re-stamps the sidecar and the gate it goes through.
            "claude_rolling_tick_",
            "restamp_",
            "rolling_token_forces_the_preemptive_leg",
        ],
    ),
    (
        // session browsing + resume, model routing, completions, and the
        // multi-instance state lock.
        "Quality-of-life",
        &[
            "sessions_json_has_exact_fields_newest_first_with_null_and_redaction",
            "resume_profile_choice_explicit_flag_forces_no_prompt",
            "info_prints_the_resume_command_workspace_and_storage",
            "profile_config_reads_models_table",
            "model_settings_round_trip",
            "build_settings_writes_model_knobs",
            "build_settings_clears_stale_model_knobs",
            "print_script_supports",
            "print_script_rejects",
            "install_bash_writes",
            "install_bash_is_idempotent",
            "install_fish_writes",
            "install_rejects_unsupported",
            "cross_thread_with_state_lock_serializes",
            "same_thread_reentrancy_does_not_deadlock",
            "poison_recovery_after_panicking_closure",
            "start_walk_",
            "start_auto_",
        ],
    ),
    (
        // the codex harness: capture + browser login, the per-session home,
        // the standby refresh with its no-replay memo and quarantine, the
        // usage leg, the chain walk, and the Overview's codex section.
        "Codex too",
        &[
            "codex_capture_",
            "codex_browser_",
            "the_callback_parses_an_error_into_the_closed_set",
            "the_code_exchange_sends_the_five_form_pairs",
            "a_shared_codex_home_links_the_table",
            "a_rollout_written_through_the_linked_root",
            "the_managed_config_verdict_refuses_the_chain_killers",
            "a_terminal_verdict_leaves_a_quarantine_record",
            "an_unwritable_memo_sends_nothing_and_keeps_the_kick",
            "standby_tick_rotates_every_due_chain_through_the_wire",
            "the_usage_fetch_sends_the_bearer",
            "a_quarantined_codex_member_is_walked_around",
            "apply_codex_switch_",
            "delete_codex_",
            "c_on_the_overview_cycles_the_harness_filter",
            "a_quarantined_codex_row_renders_the_broken_marker",
        ],
    ),
    // ── Fork additions ────────────────────────────────────────────────────
    // The README's `### Fork additions` bullets sit inside `## Features`, so
    // `extract_features` reads them too and each one needs its own row.
    (
        // capture + browser mint, the switch verb, the daemon's follow and
        // standby refresh, the isolated `clauth start`, and the codex chain.
        "Codex accounts.",
        &[
            // UPS-18: retargeted onto upstream's engine (#69). The fork's own
            // codex module is deleted, and with it every test named here
            // before — these are its reviewed replacements, same behaviours.
            //
            // capture / adopt
            "codex_capture_adopts_the_operator_slot",
            "codex_capture_refusals_name_the_fix",
            "codex_capture_refuses_a_claude_held_name",
            "codex_recapture_refuses_a_different_account",
            // the switch, and the two slots' independence
            "switch_codex_moves_only_the_codex_slot",
            "apply_codex_switch_moves_the_on_disk_marker",
            "the_codex_active_slot_is_independent_of_the_claude_one",
            "tui_switch_dispatches_codex_targets_to_the_codex_slot",
            // cross-harness refusals — a name belongs to exactly one roster
            "switch_profile_refuses_a_codex_name_as_a_codex_account",
            "the_claude_only_verbs_refuse_a_codex_name_and_list_the_claude_roster_alone",
            "the_refusal_tells_a_codex_name_apart_from_an_unknown_one",
            // login + refresh
            "login_codex_flag_and_its_browser_modifier",
            "refresh_failures_classify_the_way_codex_does",
            "the_auth_json_is_codexs_shape",
            // isolated start
            "codex_acquire_registers_and_teardown_keeps_the_durable_store",
            "an_isolated_codex_home_links_only_the_auth",
            "codex_start_refuses_a_quarantined_chain_by_name",
            // the codex chain, and its independence from the claude one
            "the_codex_chain_reads_only_the_codex_state",
            "a_spent_codex_active_moves_to_the_next_member",
            "apply_codex_switch_walks_at_the_codex_weekly_line",
            "claude_edits_never_touch_the_codex_chain",
            "membership_edits_route_by_harness",
            "a_quarantined_codex_member_is_walked_around",
            // UPS-18's own one-time migration onto the split
            "the_run_moves_the_roster_the_slot_the_chain_and_the_stores",
            "a_rename_never_overwrites_a_store_that_already_exists",
            // `use-reset`: spending a banked usage-limit reset — which credit,
            // the wire, and the confirm gate that stands before any request.
            "use_reset_picks_the_available_credit_that_expires_first",
            "use_reset_list_and_consume_send_codexs_request",
            "use_reset_off_a_terminal_without_yes_refuses_before_any_request",
            "use_reset_spends_the_credit_it_named_and_only_after_a_yes",
        ],
    ),
    (
        // CDX-5: identity injection, the mid-stream 429 rotate-and-replay,
        // and the passive tick that stands down while it runs.
        "Injection proxy.",
        &[
            "e2e_injects_identity_and_relays_the_sse_response",
            "e2e_429_rotates_to_the_next_account_and_replays",
            // The codex leg keeps running while the proxy serves (the CDX-5
            // stand-down was removed in the UPS-19 audit).
            "codex_usage_tick_walks_the_codex_chain_while_the_proxy_is_serving",
        ],
    ),
    (
        "`clauth doctor`.",
        &[
            "freshness_tracks_the_1s_write_cadence_not_the_refresh_interval",
            "exit_code_is_nonzero_only_when_something_failed",
            "skew_classifies_version_and_schema_mismatches",
            "render_shows_a_fix_only_when_not_passing",
        ],
    ),
    (
        // every verb the socket answers, plus the one-line reply contract.
        "Daemon control socket.",
        &[
            "switch_valid_profile_enqueues_and_acks",
            "refresh_one_enqueues_only_that_profile",
            "fallback_add_remove_enqueue_canonical_name",
            "fallback_move_parses_dir_and_rejects_bad_dir",
            "set_last_resort_validates_bool_and_enqueues",
            "set_threshold_validates_range_and_type",
            "set_wrap_off_requires_bool",
            "set_weekly_threshold_validates_range_on_the_socket",
            "per_member_weekly_and_gate_commands_validate_and_enqueue",
            "rename_valid_enqueues_canonical_op_and_acks",
            "snapshot_reply_is_one_line_around_a_pretty_status_file",
            "error_replies_carry_stable_error_code",
        ],
    ),
    (
        // TOK-3: the pure snapshot builder behind `~/.clauth/tokens.json`.
        "`tokens.json` feed.",
        &[
            "snapshot_has_schema_version_and_all_four_periods",
            "week_and_month_windows_filter_daily_models",
            "incomplete_split_marks_period_and_cost_as_floor",
            "caps_period_at_eight_rows_folding_the_tail_into_others",
            "lifetime_totals_and_cost_count_cache",
        ],
    ),
    (
        // the additive keys the menu-bar clients read.
        "Fork-only `status.json` fields.",
        &[
            "build_status_publishes_codex_fields",
            "build_status_keeps_the_two_active_slots_independent",
            // UPS-18: the fork's expiring/broken test is gone with the claude
            // auth_broken flag it drove codex from; the broken half is pinned
            // by upstream's own entry test, and there is no codex `expiring`
            // arm any more (the standby leg rotates on the token's own clock).
            "codex_entries_fall_back_to_the_id_token_plan_and_publish_broken",
            "build_status_forecast_publishes_next_target_and_last_resort",
            "published_entries_deserialize_into_the_typed_contract",
        ],
    ),
    (
        // FORK_BUILD compiles the updater out: it never replaces the binary
        // and never spawns the background check.
        "No self-update.",
        &[
            "fork_build_never_self_replaces_even_off_cargo",
            "fork_build_spawn_returns_none",
        ],
    ),
];

#[test]
fn features_have_test_coverage() {
    let readme = include_str!("../../README.md");

    let features = extract_features(readme);
    assert!(
        !features.is_empty(),
        "no `## Features` section or bullet items found in README"
    );

    let test_fns = collect_test_functions();

    let mut uncovered: Vec<String> = Vec::new();
    let mut rows: Vec<String> = Vec::new();

    for feature in &features {
        let entry = lookup(feature);
        match entry {
            Some(prefixes) => {
                let matched = matched_tests(prefixes, &test_fns);
                let unmatched = unmatched_prefixes(prefixes, &test_fns);

                let tests_str = if matched.is_empty() {
                    "—".to_string()
                } else {
                    matched.join(", ")
                };

                if unmatched.is_empty() {
                    rows.push(format!("| {} | {} | ✅ |", feature, tests_str));
                } else {
                    let detail = format!("missing: {}", unmatched.join(", "));
                    rows.push(format!("| {} | {} | ❌ {} |", feature, tests_str, detail));
                    uncovered.push(format!("  {feature}: {detail}"));
                }
            }
            None => {
                rows.push(format!(
                    "| {} | — | ❌ no mapping in FEATURE_MAP |",
                    feature
                ));
                uncovered.push(format!("  {feature}: add an entry to FEATURE_MAP"));
            }
        }
    }

    println!("\nFeature → Test Coverage Table\n");
    println!("| Feature | Tests | Status |");
    println!("|---|---|---|");
    for row in &rows {
        println!("{row}");
    }
    println!();

    assert!(
        uncovered.is_empty(),
        "Features without test coverage:\n{uncovered}",
        uncovered = uncovered.join("\n")
    );
}

/// Extract feature names from the README's `## Features` bullet list.
fn extract_features(readme: &str) -> Vec<String> {
    let mut in_features = false;
    let mut features = Vec::new();

    for line in readme.lines() {
        if line.starts_with("## Features") {
            in_features = true;
            continue;
        }
        if in_features {
            if line.starts_with("## ") {
                break;
            }
            // `- 🔄 **Feature name** description...`; the emoji is optional,
            // so match the first bold run on the bullet rather than its start.
            if let Some(rest) = line.strip_prefix("- ")
                && let Some(open) = rest.find("**")
                && let Some(name) = rest[open + 2..].split("**").next()
            {
                let name = name.trim();
                if !name.is_empty() {
                    features.push(name.to_string());
                }
            }
        }
    }

    features
}

/// Look up the test prefixes for a feature name.
fn lookup(feature: &str) -> Option<&'static [&'static str]> {
    FEATURE_MAP
        .iter()
        .find(|(key, _)| *key == feature)
        .map(|(_, prefixes)| *prefixes)
}

/// Return all test function names that match at least one prefix.
fn matched_tests(prefixes: &[&str], test_fns: &HashSet<String>) -> Vec<String> {
    let mut names: Vec<String> = test_fns
        .iter()
        .filter(|name| prefixes.iter().any(|p| name.contains(p)))
        .cloned()
        .collect();
    names.sort();
    names
}

/// Return prefixes that match zero test functions.
fn unmatched_prefixes<'a>(prefixes: &'a [&str], test_fns: &HashSet<String>) -> Vec<&'a str> {
    prefixes
        .iter()
        .filter(|p| !test_fns.iter().any(|name| name.contains(*p)))
        .copied()
        .collect()
}

/// Scan `tests/inline/*.rs` for function definitions.
fn collect_test_functions() -> HashSet<String> {
    let mut names = HashSet::new();
    let test_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/inline");

    let dir = match std::fs::read_dir(&test_dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "warning: cannot read tests/inline/: {e} — \
                 using empty function set"
            );
            return names;
        }
    };

    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for raw_line in content.lines() {
            let line = raw_line.trim();
            // Match `fn name(`, `fn name <`, or `fn name` at end
            if let Some(rest) = line
                .strip_prefix("fn ")
                .or_else(|| line.strip_prefix("pub fn "))
                .or_else(|| line.strip_prefix("pub(crate) fn "))
            {
                let rest = rest.trim_start();
                let name = rest.split(['(', '<', ' ', '!']).next().unwrap_or("").trim();
                if !name.is_empty() && !name.starts_with('_') {
                    names.insert(name.to_string());
                }
            }
        }
    }

    names
}
