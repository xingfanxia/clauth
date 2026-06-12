//! Feature → test traceability map (committed deliverable for task 7b).
//!
//! Every item in the README "Features" list (plus the adjacent advertised
//! surfaces — `which`, status feed, self-update, file perms) is mapped here to
//! the test fn(s) and file that cover it. Three holes were newly filled and are
//! marked **NEW**; everything else was already covered.
//!
//! This module is doc-only: it persists the map in-tree so the coverage
//! contract is reviewable from the source, not just from a report. Keep it in
//! sync when a Features-list item or its covering test changes.
//!
//! | README feature | Test(s) | File | Status |
//! |---|---|---|---|
//! | One-key / CLI switching | `auto_switch_*`, `snapshot_chain_*` (switch path); CLI resolve `resolves_started_profile_in_runtime_session` | `fallback.rs`, `which.rs` | pre-existing |
//! | Automatic token refresh (lazy rotate on 401, `t` force-rotate) | `rotate_one_no_stamp_when_no_refresh_token`, `live_session_excluded_when_force_false`, `force_true_bypasses_diverged_active_when_no_active_profile`, `rotation_guard_is_independent_across_profiles` | `oauth.rs` | pre-existing |
//! | Live usage bars / 5h+7d util / reset time | `parses_*` time helpers, `retry_after_*`, `cached_fallback_does_not_clobber_store`, `mark_window_open_*`, `window_lapsed_*` | `fetch.rs`, `scheduler.rs` | pre-existing |
//! | Per-row activity / burn-rate | `gap_boundary_*`, `*_gap_*`, `steady_drain_no_gap_no_cut` | `burn.rs` | pre-existing |
//! | Plan detection (Pro/Max 5x·20x/Team/Enterprise) | `oauth_profile`, `api_profile`, `failed_profile` (subscription/plan parse) | `showcase.rs`, `which.rs::oauth_profile` | pre-existing |
//! | Per-account breakdown (Usage tab windows + env merge) | `all_tabs_render`, `empty_state_renders`, render-tab suite | `tui_render_mod.rs`, `tui_render_tabs.rs` | pre-existing |
//! | Auto-switch on exhaustion / fallback chain / thresholds / wrap-off / sink | `auto_switch_*`, `wrap_off_*`, `find_recovered_*`, `sink_active_*` | `fallback.rs` | pre-existing |
//! | Stale-data cues | `all_tabs_render` + `fetch_status` model in showcase seed | `tui_render_mod.rs`, `showcase.rs` | pre-existing |
//! | **Account-change `[Y/n]` overwrite path** | `relogin_is_diverged_and_not_first_login`, `overwrite_confirm_captures_relogin_into_profile`, `overwrite_cancel_leaves_stored_and_live_untouched` | `claude.rs` | **NEW** |
//! | Multi-instance safe (file lock, reload, off-UI HTTP) | `cross_thread_with_state_lock_serializes`, `same_thread_reentrancy_does_not_deadlock`, `poison_recovery_after_panicking_closure` | `lock.rs` | pre-existing |
//! | Non-destructive (only API keys + declared env touched) | `diverged_*`, `classify_link_*`, `first_login_*`, env-merge in `build_runtime_dir_writes_settings_not_symlink` | `claude.rs`, `runtime.rs` | pre-existing |
//! | **Isolated launch (`clauth start`, no leakage)** | `acquire_creates_runtime_and_pid_file`, `build_runtime_dir_credentials_not_from_claude_home`, `*_preserves_live_runtime_credentials`, `acquire_isolates_credentials_from_real_home` | `runtime.rs` | partial pre-existing + **NEW** black-box (`acquire_isolates_credentials_from_real_home`) |
//! | `start` signal/exit semantics | `status_code_*` | `start.rs` | pre-existing |
//! | `clauth which [--json]` | full `which.rs` suite (token-match, session resolution, attribution, JSON path) | `which.rs` | pre-existing |
//! | **Shell completions (`completions install [shell]`)** | `print_script_supports_bash_zsh_fish`, `print_script_rejects_unsupported_shell`, `install_bash_writes_script_and_sources_it_in_rc`, `install_bash_is_idempotent_across_reruns`, `install_fish_writes_into_fish_completions_dir`, `install_rejects_unsupported_shell` | `completions.rs` | **NEW** |
//! | In-app help (`?` keybinding ref) | `all_tabs_render` (help-modal render), footer hint rows | `tui_render_mod.rs` | pre-existing |
//! | Theme / Config tab / divergence default | `theme_set_tier_round_trips`, `global_config_*`, `next_divergence_default_cycles_round_trip`, `divergence_default_*` | `tui_app.rs` | pre-existing |
//! | Status incident feed | full `status_parse.rs` (20 fns) | `status_parse.rs` | pre-existing |
//! | Self-update integrity | full `update.rs` (sha256 / sums / opt-out) | `update.rs` | pre-existing |
//! | File perms 0600/0700 | `credential_and_cache_files_have_restricted_permissions`, `usage_cache_write_creates_restricted_file_and_dir` | `profile.rs` | pre-existing |
