#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]

//! `run` recursion-guard coverage. With `CLAUTH_MCP_DEPTH >= 1` the delegate must
//! short-circuit to an `is_error` envelope BEFORE any `claude` spawn (the
//! fork-bomb cap). We assert the error envelope without faking a `claude` binary;
//! the guard returns before `spawn_blocking`/`ProfileRuntime::acquire` runs.

use super::*;

/// Drive the async `run` tool with `CLAUTH_MCP_DEPTH = depth` on a current-thread
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
            .run(Parameters(RunArgs {
                profile: "any".to_string(),
                prompt: "hello".to_string(),
                model: None,
                cwd: None,
                env: None,
                args: None,
                timeout_secs: None,
                isolated: None,
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
    result.expect("run returns a tool result, never a transport error")
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
//   1. concurrent-different-profile: `run` two different profiles at once; each
//      gets its own runtime + PID namespace and they complete without contention.
//   2. same-profile rotation safety: with an interactive session of profile P
//      live, `run` P; the delegate shares P's runtime + `RotationGuard` flock and
//      gets a fresh token chain only after the live watchdog reconciles.
//   3. happy path: a valid prompt returns `{is_error:false, result, ...}` parsed
//      from `claude -p --output-format json`, and the child inherits
//      `CLAUTH_MCP_DEPTH=1` + `--strict-mcp-config`.
