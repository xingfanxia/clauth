use super::*;

use std::sync::{Arc, Barrier};
use std::time::Instant;

/// Two threads entering `with_state_lock` simultaneously must serialize their
/// closures — no two intervals may overlap.
#[test]
fn cross_thread_with_state_lock_serializes() {
    // Sandbox-pinned: the lock path resolves through the process-global home
    // override, and without holding the sandbox lock a concurrently-running
    // sandboxed test can swap that override mid-test — two of the threads
    // below would then flock DIFFERENT files and legitimately overlap
    // (observed as a rare parallel-run flake, 2026-07-09).
    let _home = crate::testutil::HomeSandbox::new();
    const THREADS: usize = 4;
    let barrier = Arc::new(Barrier::new(THREADS));
    let intervals = Arc::new(std::sync::Mutex::new(Vec::<(u64, u64)>::new()));
    let epoch = Instant::now();

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let intervals = Arc::clone(&intervals);
            std::thread::spawn(move || {
                // All threads rendezvous here to maximize concurrent entry.
                barrier.wait();
                with_state_lock(|| {
                    let start = epoch.elapsed().as_nanos() as u64;
                    // Sleep widens the interval so overlaps are detectable.
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

    // [a_start, a_end) and [b_start, b_end) overlap when a_start < b_end && b_start < a_end.
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

/// A panic inside the closure unwinds through `StateLock::Drop`, which closes
/// the flock `File` and releases `THREAD_LOCK` (poisoning it). The next
/// acquisition must recover via `into_inner()`, observe the cleared slot, and
/// re-flock — the lock must not be permanently wedged.
#[test]
fn poison_recovery_after_panicking_closure() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let _guard = StateLock::acquire().expect("acquire before panic");
        panic!("closure blew up while holding the state lock");
    }));
    assert!(panicked.is_err(), "the inner closure must have panicked");

    // DEPTH resets to 0 — Drop ran during unwind.
    DEPTH.with(|d| assert_eq!(d.get(), 0, "depth must reset to 0 after unwind"));

    // THREAD_LOCK poisoned + slot None; fresh acquire must recover and re-flock.
    let result = with_state_lock(|| Ok(7u32));
    assert_eq!(result.unwrap(), 7, "lock must be reusable after a panic");

    // Reentrancy must still work post-recovery.
    let again = with_state_lock(|| with_state_lock(|| Ok(8u32)));
    assert_eq!(again.unwrap(), 8, "reentrancy still works post-recovery");
}
