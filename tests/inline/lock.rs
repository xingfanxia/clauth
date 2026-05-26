use super::*;

use std::sync::{Arc, Barrier};
use std::time::Instant;

/// Two threads entering `with_state_lock` simultaneously must serialize their
/// closures — no two intervals may overlap.
#[test]
fn cross_thread_with_state_lock_serializes() {
    const THREADS: usize = 4;
    let barrier = Arc::new(Barrier::new(THREADS));
    let intervals = Arc::new(std::sync::Mutex::new(Vec::<(u64, u64)>::new()));
    let epoch = Instant::now();

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let intervals = Arc::clone(&intervals);
            std::thread::spawn(move || {
                // All threads reach this point before any calls with_state_lock,
                // maximizing the chance of concurrent entry.
                barrier.wait();
                with_state_lock(|| {
                    let start = epoch.elapsed().as_nanos() as u64;
                    // Small sleep to widen the interval so overlaps are detectable.
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    let end = epoch.elapsed().as_nanos() as u64;
                    intervals.lock().unwrap().push((start, end));
                    Ok(())
                })
                .expect("with_state_lock failed");
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    let intervals = intervals.lock().unwrap();
    assert_eq!(
        intervals.len(),
        THREADS,
        "each thread must record one interval"
    );

    // Verify no two intervals overlap. Intervals [a_start, a_end) and
    // [b_start, b_end) overlap when a_start < b_end && b_start < a_end.
    for i in 0..intervals.len() {
        for j in (i + 1)..intervals.len() {
            let (a_start, a_end) = intervals[i];
            let (b_start, b_end) = intervals[j];
            assert!(
                a_end <= b_start || b_end <= a_start,
                "intervals overlap: [{a_start}, {a_end}) and [{b_start}, {b_end})"
            );
        }
    }
}

/// Same-thread nested `with_state_lock` calls must not deadlock.
#[test]
fn same_thread_reentrancy_does_not_deadlock() {
    let result = with_state_lock(|| with_state_lock(|| with_state_lock(|| Ok(42u32))));
    assert_eq!(result.unwrap(), 42);
}
