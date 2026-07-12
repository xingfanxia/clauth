//! Timestamped stderr lines for daemon-visible events.
//!
//! `daemon.log` is just the daemon's redirected stderr, and its lines used to
//! carry no timestamps — the 2026-07-09 broken-active incident had to be
//! reconstructed by guessing hours of switch/quarantine ordering from bare
//! lines. Every daemon-reachable event line now goes through [`logline!`]:
//! the daemon flips [`enable_timestamps`] once at boot, so its log gains an
//! ISO-8601 UTC prefix, while the same messages stay bare on CLI/TUI stderr
//! where a timestamp is interactive noise.

use std::sync::atomic::{AtomicBool, Ordering};

static STAMP: AtomicBool = AtomicBool::new(false);

/// Turn on timestamp prefixes for [`logline!`] — called once at the top of
/// `daemon::serve()` (covering the lock-holder AND the standing-by path).
/// Sticky for the process lifetime; never flipped back.
pub(crate) fn enable_timestamps() {
    STAMP.store(true, Ordering::Relaxed);
}

/// One line as it will hit stderr — split from [`line`] so the format is
/// pinned by a unit test without capturing stderr.
pub(crate) fn render(stamped: bool, now_secs: i64, msg: &str) -> String {
    if stamped {
        format!("{} {msg}", crate::usage::epoch_secs_to_iso(now_secs))
    } else {
        msg.to_string()
    }
}

/// `logline!` backend — call the macro, not this.
pub(crate) fn line(args: std::fmt::Arguments<'_>) {
    eprintln!(
        "{}",
        render(
            STAMP.load(Ordering::Relaxed),
            crate::usage::now_epoch_secs(),
            &args.to_string(),
        )
    );
}

/// `eprintln!` for daemon-visible event lines: identical output on CLI/TUI
/// stderr, ISO-8601-UTC-prefixed once the daemon has called
/// [`enable_timestamps`].
macro_rules! logline {
    ($($arg:tt)*) => {
        $crate::logline::line(::std::format_args!($($arg)*))
    };
}
pub(crate) use logline;

#[cfg(test)]
#[path = "../tests/inline/logline.rs"]
mod tests;
