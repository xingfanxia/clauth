//! The daemon's own herdr follower behind the blocked push: a snapshot on
//! connect, each delivered agent-status change, retries while herdr is away,
//! and one latch across reconnects.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};

/// A hang detector, never the bound a test passes inside: every test below
/// ends on its own last event.
const HANG: Duration = Duration::from_secs(30);

fn accept_within(listener: &UnixListener, within: Duration) -> UnixStream {
    let until = Instant::now() + within;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).expect("blocking stream");
                return stream;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("no connection within {within:?}: {e}"),
        }
    }
}

fn request(stream: &UnixStream) -> serde_json::Value {
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .expect("a request line");
    serde_json::from_str(&line).expect("a JSON request")
}

/// One (re)connect as herdr answers it: `pane.list` with `panes` (a JSON
/// array), then the subscription, then each of `events` delivered. The
/// subscription stream comes back so the caller decides when it drops.
fn serve_connect(listener: &UnixListener, panes: &str, events: &[&str]) -> UnixStream {
    let mut list = accept_within(listener, HANG);
    let id = request(&list)["id"].as_str().expect("id").to_string();
    list.write_all(
        format!("{{\"id\":\"{id}\",\"result\":{{\"type\":\"pane_list\",\"panes\":{panes}}}}}\n")
            .as_bytes(),
    )
    .expect("pane.list answer");
    drop(list);

    let mut sub = accept_within(listener, HANG);
    let id = request(&sub)["id"].as_str().expect("id").to_string();
    sub.write_all(
        format!("{{\"id\":\"{id}\",\"result\":{{\"type\":\"subscription_started\"}}}}\n")
            .as_bytes(),
    )
    .expect("subscribe answer");
    for event in events {
        sub.write_all(event.as_bytes()).expect("delivered event");
        sub.write_all(b"\n").expect("newline");
    }
    sub
}

fn changed(pane: &str, status: &str) -> String {
    format!(
        "{{\"event\":\"pane.agent_status_changed\",\"data\":{{\"pane_id\":\"{pane}\",\"workspace_id\":\"ws-1\",\"agent_status\":\"{status}\"}}}}"
    )
}

#[test]
fn the_follower_hands_over_the_snapshot_then_each_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("herdr.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let server = std::thread::spawn(move || {
        serve_connect(
            &listener,
            r#"[{"pane_id":"pane-1","agent_status":"working"}]"#,
            &[&changed("pane-1", "blocked")],
        )
    });

    let path = socket.clone();
    let mut seen = Vec::new();
    follow_agent_status(
        &move || Some(path.clone()),
        Duration::from_millis(100),
        Some(Instant::now() + HANG),
        &mut |event| {
            seen.push(match event {
                AgentStatusSeen::Snapshot(panes) => format!("snapshot {panes:?}"),
                AgentStatusSeen::Changed { pane_id, status } => {
                    format!("changed {pane_id} {status}")
                }
            });
            if seen.len() == 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    );
    drop(server.join().expect("server"));

    assert_eq!(
        seen,
        [
            r#"snapshot [("pane-1", "working")]"#.to_string(),
            "changed pane-1 blocked".to_string(),
        ]
    );
}

/// The resolver calls one follow makes over 2 s at a 20 ms retry while
/// nothing answers, and what it handed over (nothing).
fn resolves_while_absent(resolved: Option<std::path::PathBuf>) -> (usize, usize) {
    let resolves = std::sync::atomic::AtomicUsize::new(0);
    let mut seen = 0;
    follow_agent_status(
        &|| {
            resolves.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            resolved.clone()
        },
        Duration::from_millis(20),
        Some(Instant::now() + Duration::from_secs(2)),
        &mut |_| {
            seen += 1;
            ControlFlow::Continue(())
        },
    );
    (resolves.load(std::sync::atomic::Ordering::Relaxed), seen)
}

/// No resolved socket (herdr not installed yet) is resolved again on every
/// retry; a resolved path with no server behind it is reconnected to as is,
/// with no further resolve, which is a herdr spawn in production.
#[test]
fn only_an_unresolved_socket_is_resolved_again() {
    let dir = tempfile::tempdir().expect("tempdir");

    let (unresolved, unresolved_seen) = resolves_while_absent(None);
    let (resolved, resolved_seen) = resolves_while_absent(Some(dir.path().join("no-herdr.sock")));

    // A floor, not a count: 2 s at a 20 ms retry is ~100 tries unloaded.
    assert!(unresolved >= 3, "resolved {unresolved} times");
    assert_eq!((resolved, unresolved_seen, resolved_seen), (1, 0, 0));
}

/// One latch spans the follow: a reconnect's snapshot repeating pane-1's
/// standing `blocked` hands nothing over, and a new pane's move still does.
#[test]
fn a_reconnect_repeating_a_standing_blocked_hands_over_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("herdr.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let server = std::thread::spawn(move || {
        // The first connection drops right after its snapshot: a lost herdr.
        drop(serve_connect(
            &listener,
            r#"[{"pane_id":"pane-1","agent_status":"blocked"}]"#,
            &[],
        ));
        serve_connect(
            &listener,
            r#"[{"pane_id":"pane-1","agent_status":"blocked"},{"pane_id":"pane-2","agent_status":"working"}]"#,
            &[&changed("pane-2", "blocked")],
        )
    });

    let path = socket.clone();
    let mut blocked = Vec::new();
    follow_blocked(
        &move || Some(path.clone()),
        Duration::from_millis(20),
        Some(Instant::now() + HANG),
        &mut |pane_id| {
            blocked.push(pane_id);
            if blocked.len() == 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    );
    drop(server.join().expect("server"));

    assert_eq!(blocked, ["pane-1".to_string(), "pane-2".to_string()]);
}
