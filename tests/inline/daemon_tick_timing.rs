use super::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn a_fast_tick_logs_nothing() {
    assert_eq!(
        slow_tick_line(ms(400), &[(Step::StatusBuild, ms(390))]),
        None
    );
}

#[test]
fn a_slow_tick_names_every_step_in_order() {
    let line = slow_tick_line(
        ms(9_200),
        &[
            (Step::Reload, ms(12)),
            (Step::StatusBuild, ms(9_100)),
            (Step::StatusWrite, ms(40)),
        ],
    )
    .expect("a 9s tick is slow");
    assert_eq!(
        line,
        "clauth daemon: slow tick 9200ms: reload 12ms, status_build 9100ms, status_write 40ms"
    );
}

#[test]
fn a_stall_is_reported_once_per_heartbeat_and_only_past_the_threshold() {
    let after = STALL_REPORT_AFTER.as_millis() as u64;
    assert_eq!(stall_line(1_000, after - 1, Step::StatusBuild, 0), None);
    let line = stall_line(1_000, after + 2_000, Step::StatusBuild, 0).expect("stalled");
    assert!(line.contains("status_build"), "{line}");
    assert!(
        line.contains(&format!("{}s", (after + 2_000) / 1000)),
        "{line}"
    );
    assert_eq!(
        stall_line(1_000, after + 4_000, Step::StatusBuild, 1_000),
        None,
        "the same stalled heartbeat is reported once"
    );
}

#[test]
fn the_timer_charges_each_step_its_own_time() {
    let mut timer = TickTimer::start();
    timer.enter(Step::Reload);
    std::thread::sleep(ms(5));
    timer.enter(Step::StatusBuild);
    assert_eq!(current(), Step::StatusBuild);
    let _ = timer.finish();
    assert_eq!(current(), Step::Idle);
}
