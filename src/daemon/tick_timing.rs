//! Where a slow main-loop tick spends its time, in `daemon.log`.
//!
//! The main loop rewrites `status.json` once a tick; a tick that runs long
//! freezes the feed and ccsbar reads the daemon as dead after 15s. Observed
//! 2026-09-23/24 without an answer from the log: the stalls were only visible
//! from outside, with `sample`. Two reports close that:
//!
//! - a finished tick that took [`SLOW_TICK`] or longer logs each step's time;
//! - a tick still running [`STALL_REPORT_AFTER`] past the last heartbeat logs
//!   the step it is in, from a thread of its own, so a tick that never finishes
//!   still names where it is.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::logline::logline;

/// A finished tick at least this long logs its per-step times.
pub(super) const SLOW_TICK: Duration = Duration::from_secs(3);
/// A tick running this long past the last heartbeat is reported while it runs.
pub(super) const STALL_REPORT_AFTER: Duration = Duration::from_secs(8);
const STALL_POLL: Duration = Duration::from_secs(2);

/// The steps of one tick, in order. `Idle` is the wait between ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Step {
    Idle,
    Reload,
    FollowLiveLogin,
    DuplicateLogins,
    DayClaims,
    DrainSwitch,
    DrainSwitchOff,
    DrainConfigOps,
    StatusBuild,
    StatusWrite,
    PluginHeal,
    HerdrHeal,
}

impl Step {
    const ALL: [Step; 12] = [
        Step::Idle,
        Step::Reload,
        Step::FollowLiveLogin,
        Step::DuplicateLogins,
        Step::DayClaims,
        Step::DrainSwitch,
        Step::DrainSwitchOff,
        Step::DrainConfigOps,
        Step::StatusBuild,
        Step::StatusWrite,
        Step::PluginHeal,
        Step::HerdrHeal,
    ];

    pub(super) fn name(self) -> &'static str {
        match self {
            Step::Idle => "idle",
            Step::Reload => "reload",
            Step::FollowLiveLogin => "follow_live_login",
            Step::DuplicateLogins => "duplicate_logins",
            Step::DayClaims => "day_claims",
            Step::DrainSwitch => "drain_switch",
            Step::DrainSwitchOff => "drain_switch_off",
            Step::DrainConfigOps => "drain_config_ops",
            Step::StatusBuild => "status_build",
            Step::StatusWrite => "status_write",
            Step::PluginHeal => "plugin_heal",
            Step::HerdrHeal => "herdr_heal",
        }
    }

    fn from_u8(v: u8) -> Step {
        Step::ALL.get(usize::from(v)).copied().unwrap_or(Step::Idle)
    }
}

/// The step the main loop is in now, read by the stall reporter.
static CURRENT: AtomicU8 = AtomicU8::new(Step::Idle as u8);

pub(super) fn current() -> Step {
    Step::from_u8(CURRENT.load(Ordering::Relaxed))
}

/// Per-step wall time for one tick.
pub(super) struct TickTimer {
    started: Instant,
    step: Step,
    step_started: Instant,
    spent: Vec<(Step, Duration)>,
}

impl TickTimer {
    pub(super) fn start() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            step: Step::Idle,
            step_started: now,
            spent: Vec::with_capacity(Step::ALL.len()),
        }
    }

    /// Close the running step and open `step`.
    pub(super) fn enter(&mut self, step: Step) {
        let now = Instant::now();
        if self.step != Step::Idle {
            self.spent.push((self.step, now - self.step_started));
        }
        self.step = step;
        self.step_started = now;
        CURRENT.store(step as u8, Ordering::Relaxed);
    }

    /// Close the tick; the log line when it was slow.
    pub(super) fn finish(mut self) -> Option<String> {
        self.enter(Step::Idle);
        slow_tick_line(self.started.elapsed(), &self.spent)
    }
}

/// `None` under [`SLOW_TICK`]; otherwise the total and every step's time, in
/// order. Pure, so the format is tested.
pub(super) fn slow_tick_line(total: Duration, spent: &[(Step, Duration)]) -> Option<String> {
    if total < SLOW_TICK {
        return None;
    }
    let steps: Vec<String> = spent
        .iter()
        .map(|(step, d)| format!("{} {}ms", step.name(), d.as_millis()))
        .collect();
    Some(format!(
        "clauth daemon: slow tick {}ms: {}",
        total.as_millis(),
        steps.join(", ")
    ))
}

/// What the stall reporter says for a heartbeat `age_ms` old while the loop is
/// in `step`, given the heartbeat it last reported (`reported`). `Some(line)`
/// once per stalled heartbeat; `None` otherwise. Pure, so the once-only rule is
/// tested.
pub(super) fn stall_line(
    heartbeat_ms: u64,
    age_ms: u64,
    step: Step,
    reported: u64,
) -> Option<String> {
    if age_ms < STALL_REPORT_AFTER.as_millis() as u64 || heartbeat_ms == reported {
        return None;
    }
    Some(format!(
        "clauth daemon: tick stalled {}s so far in {} (status.json is not being rewritten)",
        age_ms / 1000,
        step.name()
    ))
}

/// Report a stalled tick while it is still stalled. Logs once per stalled
/// heartbeat, and once more when that tick finally lands.
pub(super) fn spawn_stall_reporter(heartbeat: Arc<AtomicU64>) {
    let spawned = std::thread::Builder::new()
        .name("clauth-daemon-stallwatch".into())
        .spawn(move || {
            let mut reported: u64 = 0;
            loop {
                std::thread::sleep(STALL_POLL);
                let beat = heartbeat.load(Ordering::Relaxed);
                let now = crate::usage::now_ms();
                if reported != 0 && beat != reported {
                    logline!(
                        "clauth daemon: stalled tick finished after {}s",
                        beat.saturating_sub(reported) / 1000
                    );
                    reported = 0;
                }
                if let Some(line) = stall_line(beat, now.saturating_sub(beat), current(), reported)
                {
                    logline!("{line}");
                    reported = beat;
                }
            }
        });
    if let Err(e) = spawned {
        logline!("clauth daemon: failed to spawn the stall reporter: {e}");
    }
}

#[cfg(test)]
#[path = "../../tests/inline/daemon_tick_timing.rs"]
mod tests;
