#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]

//! Startup-path pin: `mcp::startup` over a broken plugin registration must
//! reach the heal's `claude` spawn. Deleting the `heal_detached` call from
//! `startup` reds here; the daemon twin
//! `tick_heals_a_broken_plugin_registration` pins the tick call site.
//! Unix-only: the fake `claude` is a shell shim.

#[cfg(unix)]
use crate::testutil::{HomeSandbox, git_shim, heal_env, lightweight_tag, stateful_heal_shim};

#[cfg(unix)]
#[test]
fn startup_heals_a_broken_plugin_registration() {
    use crate::testutil::{FakeClaude, HomeSandbox, join_background_tasks};

    let home = HomeSandbox::new();
    let fake = FakeClaude::new(&home);
    crate::plugin_host::reset_heal_throttle_for_test();
    // This test pins the claude heal; the herdr twin shares the startup call
    // site, so its throttle is armed to keep this test spawn-free.
    crate::herdr::arm_heal_throttle_for_test();
    crate::testutil::seed_broken_plugin_registration();

    let _marker = super::startup();
    join_background_tasks();

    assert!(
        !fake.log().is_empty(),
        "startup over a broken registration must reach the heal"
    );
}

/// The herdr heal at startup is a NETWORK update, so it reads the saved
/// `[update]` toggle fresh: a server started with `auto_update = false` on
/// disk reinstalls nothing, and one started with it back on reaches the fake
/// install — proving the saved-off call never claimed the throttle floor.
#[cfg(unix)]
#[test]
fn startup_herdr_heal_follows_the_saved_update_toggle() {
    use std::ffi::OsStr;

    use crate::testutil::join_background_tasks;

    let home = HomeSandbox::new();
    let stale = crate::herdr::plugin_list_json(
        r#"{"enabled":true,"plugin_id":"clauth","source":{"kind":"github","owner":"uwuclxdy","repo":"clauth","resolved_commit":"aaaaaaaaaaaaaaaa"}}"#,
    );
    let shim = stateful_heal_shim(home.home());
    git_shim(home.home());
    let tags = lightweight_tag("v0.15.1", "bbbbbbbbbbbbbbbb");
    let _env = heal_env(
        &home,
        &shim,
        &stale,
        &stale,
        &tags,
        &[("HERDR_SHIM_STATE", OsStr::new("1"))],
    );
    // The claude heal shares the startup call site; its throttle is armed so
    // this test stays spawn-free beside the herdr heal it pins.
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::reset_heal_throttle_for_test();

    let mut state = crate::profile::load_app_state().expect("load state");
    state.update.auto_update = false;
    crate::profile::save_app_state(&state).expect("persist off toggle");

    let _marker = super::startup();
    join_background_tasks();
    assert!(
        !home.home().join("heal.log").exists(),
        "a server started with the saved toggle off reinstalls nothing"
    );

    // The saved-off call must not have claimed the throttle: back on, a
    // server started fresh reads the new value and installs.
    let mut state = crate::profile::load_app_state().expect("load state");
    state.update.auto_update = true;
    crate::profile::save_app_state(&state).expect("persist on toggle");

    let _marker = super::startup();
    join_background_tasks();
    let log = std::fs::read_to_string(home.home().join("heal.log")).unwrap_or_default();
    assert_eq!(
        log.trim(),
        "plugin install uwuclxdy/clauth/herdr-plugin --ref v0.15.1 --yes",
        "a server started with the saved toggle on reaches the fake install"
    );
}

// ── the marker-registration wait ─────────────────────────────────────────────

/// A `MintSpec` with every identity field neutral — the mint only reads the
/// owner stamp.
fn neutral_mint() -> super::MintSpec {
    super::MintSpec {
        profile: "test".to_string(),
        started_at: crate::usage::now_ms(),
        endpoint: None,
        provider: None,
        isolation: crate::runtime::Isolation::Shared,
        origin: super::DelegateOrigin::default(),
    }
}

/// A `DelegateArgs` naming one unknown profile, every other field unset: the
/// handler reaches the marker wait before it resolves (and refuses) the target.
fn delegate_args(target: &str) -> super::DelegateArgs {
    super::DelegateArgs {
        profiles: Some(vec![target.to_string()]),
        prompt: Some("no-op: the target is refused before any spawn".to_string()),
        model: None,
        cwd: None,
        env: None,
        args: None,
        session_id: None,
        subagent_type: None,
        allowed_tools: None,
        permission_mode: None,
        result: None,
        isolated: None,
        background: None,
    }
}

/// The mint wait is bounded by a deadline derived from the state flock it waits
/// behind: [`state_lock_timeout`] plus the poll margin, so shrinking the flock
/// bound shrinks the wait (cloudy's ruling: the waiter's bound is derived from
/// the flock timeout, never an independent number).
#[test]
fn the_marker_wait_derives_from_the_state_flock_timeout() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_millis(400)));
    assert_eq!(
        super::jobs::server_marker_wait(),
        std::time::Duration::from_millis(650),
        "the wait is the flock timeout (400 ms) plus the fixed 250 ms poll margin"
    );
}

/// A delegate handler waiting for the marker registration must yield to the
/// runtime: the server runs on ONE current-thread runtime, so the blocking wait
/// this replaces stalled every other request for its whole bound. A second task
/// on that runtime answers while the delegate waits.
#[test]
fn a_delegate_waits_off_the_runtime_for_the_marker() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let _home = crate::testutil::HomeSandbox::new();
    // A 2 s flock timeout makes the wait's bound 2.25 s.
    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_secs(2)));
    super::jobs::mark_server_registration_in_flight();
    // The registration settles only once the second task below opens the gate:
    // the delegate's wait can end early only if the runtime ran that task, so a
    // wait that blocks the runtime reaches its bound instead, never a timer race.
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let reg = std::thread::spawn(move || {
        let _ = gate_rx.recv();
        let _guard = super::jobs::hold_server_marker().expect("hold the marker");
        let _ = release_rx.recv();
    });

    let saved = std::env::var(super::MCP_DEPTH_ENV).ok();
    // SAFETY: test-only, serialized by the sandbox's HOME_TEST_LOCK.
    unsafe { std::env::remove_var(super::MCP_DEPTH_ENV) };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    let answered = std::sync::Arc::new(AtomicBool::new(false));
    let answered_task = answered.clone();
    let started = std::time::Instant::now();
    let result = rt.block_on(async move {
        // The "second request": any work the one runtime thread must keep doing
        // while a delegate waits on the registration.
        let second = tokio::spawn(async move {
            answered_task.store(true, Ordering::SeqCst);
            let _ = gate_tx.send(());
        });
        let server = super::ClauthServer::new();
        let result = server
            .delegate_with(
                delegate_args("no-such-profile"),
                super::ProgressSink::none(),
            )
            .await
            .expect("a refusal is still a tool result");
        assert!(
            answered.load(Ordering::SeqCst),
            "the shared runtime answered another request while the delegate waited for the marker"
        );
        second.await.expect("the second task runs");
        result
    });
    let elapsed = started.elapsed();
    // SAFETY: restore the prior value under the same held lock.
    unsafe {
        match &saved {
            Some(v) => std::env::set_var(super::MCP_DEPTH_ENV, v),
            None => std::env::remove_var(super::MCP_DEPTH_ENV),
        }
    }

    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    assert!(
        text.contains("no-such-profile"),
        "the call is refused after the wait: {text}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "the handler's wait ended when the registration settled, not at its 2.25 s bound: {elapsed:?}"
    );

    release_tx.send(()).expect("release the marker");
    reg.join().expect("registration thread");
}

/// A mint taken after the (bounded) wait, while the registration is deciding,
/// carries the server owner once it settles — no record mints ownerless.
#[test]
fn a_mint_waits_for_the_marker_registration_and_carries_the_owner() {
    let _home = crate::testutil::HomeSandbox::new();
    // A 2 s flock timeout makes the wait's bound 2.25 s, far past the 300 ms the
    // gate below stays shut, so a late CI timer cannot reach the bound.
    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_secs(2)));
    super::jobs::mark_server_registration_in_flight();
    // The registration settles only once the test opens the gate, so the "not
    // yet" check below is decided by the gate, never by two timers racing.
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let reg = std::thread::spawn(move || {
        let _ = gate_rx.recv();
        let _guard = super::jobs::hold_server_marker().expect("hold the marker");
        let _ = release_rx.recv();
    });

    let (mint_tx, mint_rx) = std::sync::mpsc::channel::<u32>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        rt.block_on(super::jobs::wait_for_server_marker_async(
            super::jobs::marker_wait_deadline(),
        ));
        let spec = super::mint_spec(&neutral_mint(), super::jobs::RecordKind::Collectable);
        let _ = mint_tx.send(spec.owner_pid);
    });

    assert!(
        mint_rx
            .recv_timeout(std::time::Duration::from_millis(300))
            .is_err(),
        "the mint waits while the registration is still deciding"
    );
    gate_tx.send(()).expect("let the registration settle");
    let owner = mint_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the mint returns once the marker lands");
    assert_eq!(
        owner,
        std::process::id(),
        "the record carries the server owner"
    );

    release_tx.send(()).expect("release the marker");
    reg.join().expect("registration thread");
}

/// Past the wait bound the record mints the ownerless legacy shape and logs the
/// give-up ONCE, the whole line pinned by equality (a substring pin lets the
/// derived duration clause drift): a wedged registration never hangs a delegate
/// call.
#[test]
fn a_mint_past_the_marker_wait_mints_ownerless_and_logs() {
    let _home = crate::testutil::HomeSandbox::new();
    // 1 s (not 100 ms): the log renders the bound with `{:.0}s`, and a
    // sub-second override would round the interpolated figure to "0s" —
    // degenerate. 1 s + the 250 ms margin still renders "1s".
    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_secs(1)));
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    super::jobs::mark_server_registration_in_flight();
    // Never settle the registration: the wait gives up at its derived bound.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    rt.block_on(super::jobs::wait_for_server_marker_async(
        super::jobs::marker_wait_deadline(),
    ));
    let spec = super::mint_spec(&neutral_mint(), super::jobs::RecordKind::Collectable);
    assert_eq!(
        spec.owner_pid, 0,
        "past the wait bound, the mint is ownerless"
    );
    assert_eq!(spec.owner_started_at, 0);
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth: server marker not registered within 1s; the job record mints ownerless"
                .to_string()
        ],
        "the give-up line, by equality"
    );
}

/// A registration that FAILS still settles the lifecycle: otherwise every mint
/// after it waits out the full bound and mints ownerless. Panics settle too (a
/// drop guard), which the same guard covers and this failure path pins.
#[test]
fn a_failed_marker_registration_still_settles() {
    let home = crate::testutil::HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).expect("create .clauth");
    // `mcp_live` as a regular file: `mkdir_700` refuses, so the registration
    // fails inside the state-lock hold.
    std::fs::write(clauth.join("mcp_live"), b"not a directory").expect("block the live dir");
    super::jobs::mark_server_registration_in_flight();
    assert!(
        super::jobs::hold_server_marker().is_err(),
        "the blocked live dir fails the registration"
    );
    assert!(
        super::jobs::marker_registration_settled(),
        "a failed registration settles, so a mint does not wait the full bound"
    );
    assert!(
        super::jobs::server_owner_pid() == 0,
        "no owner stamp without a held marker"
    );
}

/// After `delegate_with`'s marker wait times out (the give-up line logged once),
/// the mint it then reaches must NOT arm a second full bound: the wait lives at
/// exactly one entry point. A mint after a timed-out wait logs the give-up line
/// once, not twice, and spends one bound, not two.
#[test]
fn a_mint_reached_after_the_delegate_wait_timed_out_does_not_wait_a_second_bound() {
    let _home = crate::testutil::HomeSandbox::new();
    // 1 s: the log renders the bound with `{:.0}s`, and a sub-second override
    // would round the interpolated figure to "0s" — degenerate.
    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_secs(1)));
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    super::jobs::mark_server_registration_in_flight();
    // Never settle: the delegate's ONE wait gives up at its derived bound.
    let started = std::time::Instant::now();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    rt.block_on(super::jobs::wait_for_server_marker_async(
        super::jobs::marker_wait_deadline(),
    ));
    // The mint the timed-out wait then reaches must not arm a fresh bound.
    let handoff = super::Handoff::blocking(neutral_mint());
    handoff.mark_spawned();
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "one bound (1.25 s), never two (2.5 s): {elapsed:?}"
    );
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth: server marker not registered within 1s; the job record mints ownerless"
                .to_string()
        ],
        "the give-up line exactly once, from the delegate wait alone"
    );
}
