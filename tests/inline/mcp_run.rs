#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]

//! `delegate` recursion-guard coverage. With `CLAUTH_MCP_DEPTH >= 1` the delegate must
//! short-circuit to an `is_error` envelope BEFORE any `claude` spawn (the
//! fork-bomb cap). We assert the error envelope without faking a `claude` binary;
//! the guard returns before `spawn_blocking`/`ProfileRuntime::acquire` runs.

use super::*;
use crate::testutil::HomeSandbox;

/// Drive the async `delegate` tool with `CLAUTH_MCP_DEPTH = depth` on a current-thread
/// runtime, restoring the prior env value before returning.
///
/// # Safety
/// `set_var`/`remove_var` are unsafe in Rust 2024 (not thread-safe). The lock
/// only serializes tests that also take it (the env/FS tests, now including
/// `update.rs`'s `with_no_update_env`); a test mutating env without it could
/// still race. Restored before the function returns, so no other thread that
/// holds the lock observes a torn value.
fn run_with_depth(depth: &str) -> CallToolResult {
    let _guard = crate::profile::HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let saved = std::env::var(MCP_DEPTH_ENV).ok();
    // SAFETY: test-only, serialized by the lock above, restored unconditionally.
    unsafe { std::env::set_var(MCP_DEPTH_ENV, depth) };

    let server = ClauthServer::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let result = rt.block_on(async {
        server
            .delegate(Parameters(DelegateArgs {
                profile: "any".to_string(),
                prompt: "hello".to_string(),
                model: None,
                cwd: None,
                env: None,
                args: None,
                timeout_secs: None,
                isolated: None,
                background: None,
            }))
            .await
    });

    // SAFETY: same as above — restore the prior value.
    unsafe {
        match &saved {
            Some(v) => std::env::set_var(MCP_DEPTH_ENV, v),
            None => std::env::remove_var(MCP_DEPTH_ENV),
        }
    }
    result.expect("delegate returns a tool result, never a transport error")
}

#[test]
fn depth_guard_refuses_at_depth_one_without_spawning() {
    let result = run_with_depth("1");

    assert_eq!(
        result.is_error,
        Some(true),
        "delegate at depth 1 is a tool error"
    );

    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("error envelope text");
    let envelope: serde_json::Value = serde_json::from_str(&text).expect("parse envelope");
    assert_eq!(envelope["is_error"], serde_json::Value::Bool(true));
    assert_eq!(envelope["profile"], "any");
    assert!(
        envelope["result"].as_str().unwrap().contains("depth"),
        "the refusal reason names the depth cap",
    );
}

#[test]
fn depth_guard_also_refuses_above_one() {
    let result = run_with_depth("3");
    assert_eq!(result.is_error, Some(true));
}

// TODO(manual/integration): the live-spawn paths cannot be unit-tested without a
// real `claude` on PATH, and we deliberately do NOT fake one (a fake binary
// would assert nothing about the real envelope contract). Verify by hand:
//   1. concurrent-different-profile: `delegate` two different profiles at once; each
//      gets its own runtime + PID namespace and they complete without contention.
//   2. same-profile rotation safety: with an interactive session of profile P
//      live, `delegate` P; the delegate shares P's runtime + `RotationGuard` flock and
//      gets a fresh token chain only after the live watchdog reconciles.
//   3. happy path: a valid prompt returns `{is_error:false, result, ...}` parsed
//      from `claude -p --output-format json`, and the child inherits
//      `CLAUTH_MCP_DEPTH=1` + `--strict-mcp-config`.

// ---- background delegation + delegate_result ----

/// Drive `delegate_result` on a current-thread runtime under a home sandbox the
/// caller has already entered.
fn call_delegate_result(job_id: &str, wait_secs: Option<u64>) -> CallToolResult {
    let server = ClauthServer::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(async {
        server
            .delegate_result(Parameters(DelegateResultArgs {
                job_id: job_id.to_string(),
                wait_secs,
            }))
            .await
    })
    .expect("delegate_result returns a tool result, never a transport error")
}

#[test]
fn delegate_result_unknown_job_is_error() {
    let _home = HomeSandbox::new();
    let result = call_delegate_result("d-doesnotexist-0", Some(0));
    assert_eq!(
        result.is_error,
        Some(true),
        "unknown job_id is a tool error"
    );
}

#[test]
fn delegate_result_invalid_job_id_is_error() {
    let _home = HomeSandbox::new();
    let result = call_delegate_result("../escape", Some(0));
    assert_eq!(result.is_error, Some(true), "path-unsafe job_id refused");
}

#[test]
fn delegate_result_running_reports_status() {
    let _home = HomeSandbox::new();
    jobs::write_running("d-run-0", "work", 1).unwrap();
    let result = call_delegate_result("d-run-0", Some(0));
    assert_ne!(result.is_error, Some(true), "a running job is not an error");
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("status text");
    assert!(text.contains("running"), "running status surfaced");
}

#[test]
fn delegate_result_done_returns_envelope_and_evicts() {
    let _home = HomeSandbox::new();
    let env = serde_json::json!({ "profile": "work", "is_error": false, "result": "all done" });
    jobs::write_done("d-done-0", "work", 1, env).unwrap();

    let result = call_delegate_result("d-done-0", Some(0));
    assert_ne!(result.is_error, Some(true));
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("envelope text");
    assert!(text.contains("all done"), "envelope result delivered");
    assert!(
        jobs::read("d-done-0").is_none(),
        "done job evicted on fetch"
    );
}

#[test]
fn background_depth_guard_refuses_without_writing_job() {
    let _home = HomeSandbox::new();
    let saved = std::env::var(MCP_DEPTH_ENV).ok();
    // SAFETY: test-only, serialized by HOME_TEST_LOCK (held by the sandbox),
    // restored unconditionally below.
    unsafe { std::env::set_var(MCP_DEPTH_ENV, "1") };

    let server = ClauthServer::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let result = rt.block_on(async {
        server
            .delegate(Parameters(DelegateArgs {
                profile: "any".to_string(),
                prompt: "hello".to_string(),
                model: None,
                cwd: None,
                env: None,
                args: None,
                timeout_secs: None,
                isolated: None,
                background: Some(true),
            }))
            .await
    });

    // SAFETY: restore the prior value.
    unsafe {
        match &saved {
            Some(v) => std::env::set_var(MCP_DEPTH_ENV, v),
            None => std::env::remove_var(MCP_DEPTH_ENV),
        }
    }

    let result = result.expect("delegate returns a tool result, never a transport error");
    assert_eq!(
        result.is_error,
        Some(true),
        "depth-1 background delegate refuses"
    );
    let job_count = jobs::jobs_dir()
        .ok()
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|rd| rd.flatten().count())
        .unwrap_or(0);
    assert_eq!(
        job_count, 0,
        "a refused background delegate writes no job file"
    );
}

// ---- mcp-await-job job_id extraction (shape-agnostic) ----

#[test]
fn find_job_id_extracts_from_nested_mcp_result() {
    // Mirrors the host's documented mcp_result shape: the background response
    // envelope is JSON-encoded as the content block's text.
    let inner = serde_json::json!({ "job_id": "d-42-0", "profile": "work", "status": "running" });
    let payload = serde_json::json!({
        "tool_name": "mcp__plugin_clauth_clauth__delegate",
        "tool_response": {
            "type": "mcp_result",
            "content": [{ "type": "text", "text": inner.to_string() }],
        }
    });
    assert_eq!(find_job_id(&payload).as_deref(), Some("d-42-0"));
}

#[test]
fn find_job_id_finds_direct_field() {
    let payload = serde_json::json!({ "tool_response": { "job_id": "d-1-2" } });
    assert_eq!(find_job_id(&payload).as_deref(), Some("d-1-2"));
}

#[test]
fn find_job_id_none_for_sync_envelope() {
    // a sync delegate response carries no job_id, so the hook no-ops.
    let inner = serde_json::json!({ "profile": "work", "is_error": false, "result": "done" });
    let payload = serde_json::json!({
        "tool_response": { "content": [{ "type": "text", "text": inner.to_string() }] }
    });
    assert_eq!(find_job_id(&payload), None);
}

#[test]
fn find_job_id_none_for_plain_text() {
    let payload =
        serde_json::json!({ "tool_response": { "content": [{ "text": "no json here" }] } });
    assert_eq!(find_job_id(&payload), None);
}
