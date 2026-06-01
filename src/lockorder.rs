//! Globally-ordered locks that enforce a single acquisition order in code.
//!
//! Every shared lock in clauth carries a *rank* — its position in one global
//! order. A thread may only acquire a lock whose rank is strictly greater than
//! the highest rank it already holds. Acquiring out of that order is the
//! classic lock-order-inversion that deadlocks, so we assert it the moment a
//! lock is taken. What used to be prose ("`usage_store` before `config`", "never
//! two leaf mutexes at once", "`RotationGuard` outermost") is now an executable
//! check that fails loudly in tests and dev runs.
//!
//! The assertion and its bookkeeping are `cfg(debug_assertions)`-only: release
//! builds compile the rank stack out entirely, so [`RankedMutex`] is a
//! zero-overhead wrapper around [`std::sync::Mutex`] in production.
//!
//! ## Deriving the order
//!
//! Only *nested* holdings constrain the order — a sequential acquire-then-drop
//! imposes nothing. The order below is the transitive closure of every nested
//! holding in the codebase:
//!
//! - `RotationGuard` is held across the OAuth HTTP round trip — outermost.
//! - `partition_due`: `last_fetched` → `usage_store` → `activity` → `learned`.
//! - `apply_usage`: `usage_store` → `usage_status` → `config`.
//! - rotation/save sites: `config` → state flock → `activity`.
//! - `update_learner`: `learned` → `ok_count` → `cache_hit` → `last_429`.
//!
//! Standalone leaves (`refetch_queue`, the `pending_*` sets, …) are never nested
//! with another tracked lock; they are ranked above the rest so that a future
//! accidental nesting under any held lock still *increases* the rank rather than
//! inverting it.

use std::ops::{Deref, DerefMut};
use std::sync::{LockResult, Mutex, PoisonError};

/// Global lock order. Lower value = acquired earlier (outer). Gaps leave room to
/// insert future locks without renumbering; only the relative order matters.
pub(crate) mod rank {
    /// `RotationGuard` (per-profile rotation flock). Held across HTTP, outermost.
    pub(crate) const ROTATION: u16 = 100;
    pub(crate) const LAST_FETCHED: u16 = 200;
    pub(crate) const TOKENS: u16 = 250;
    pub(crate) const USAGE_STORE: u16 = 300;
    pub(crate) const USAGE_STATUS: u16 = 350;
    pub(crate) const CONFIG: u16 = 400;
    /// `with_state_lock` (cross-process state flock). Inner of `config`.
    pub(crate) const STATE: u16 = 500;
    pub(crate) const ACTIVITY: u16 = 600;
    pub(crate) const LEARNED: u16 = 700;
    pub(crate) const OK_COUNT: u16 = 800;
    pub(crate) const CACHE_HIT: u16 = 900;
    pub(crate) const LAST_429: u16 = 1000;
    // Standalone leaves — never nested with another tracked lock.
    pub(crate) const NEXT_REFRESH: u16 = 1100;
    pub(crate) const REFETCH_QUEUE: u16 = 1200;
    pub(crate) const LAST_ROTATED_WINDOW: u16 = 1300;
    pub(crate) const PENDING_WINDOW_ROTATION: u16 = 1400;
    pub(crate) const PENDING_SWITCH: u16 = 1500;
    pub(crate) const PENDING_AUTO_START: u16 = 1600;
}

#[cfg(debug_assertions)]
thread_local! {
    /// Ranks currently held by this thread, in acquisition order.
    static HELD: std::cell::RefCell<Vec<u16>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Tracks one held rank on the current thread; pops it on drop. Used directly by
/// the two file-lock guards ([`crate::lock`]'s state flock and
/// [`crate::runtime::RotationGuard`]) which are not [`Mutex`]es but still
/// participate in the global order.
pub(crate) struct RankGuard {
    #[cfg(debug_assertions)]
    rank: u16,
}

impl RankGuard {
    /// Enter `rank`, asserting it is strictly greater than the highest rank the
    /// current thread already holds. No-op in release builds.
    #[inline]
    pub(crate) fn enter(_rank: u16) -> Self {
        #[cfg(debug_assertions)]
        {
            HELD.with(|h| {
                let mut h = h.borrow_mut();
                debug_assert!(
                    h.last().is_none_or(|&top| _rank > top),
                    "lock-order violation: acquiring rank {_rank} while holding {:?} \
                     (would invert the global lock order and risk deadlock)",
                    h.as_slice(),
                );
                h.push(_rank);
            });
            Self { rank: _rank }
        }
        #[cfg(not(debug_assertions))]
        {
            Self {}
        }
    }
}

impl Drop for RankGuard {
    #[inline]
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        HELD.with(|h| {
            let mut h = h.borrow_mut();
            // Strict RAII makes this the stack top, but pop the last matching
            // entry defensively so a stray drop can't corrupt the stack.
            if let Some(pos) = h.iter().rposition(|&r| r == self.rank) {
                h.remove(pos);
            }
        });
    }
}

/// A [`Mutex`] carrying a compile-time rank in the global lock order. `lock()`
/// enters the rank (asserting order) before acquiring the inner mutex and holds
/// it for the guard's lifetime. Drop-in for [`std::sync::Mutex`]: `lock()`
/// returns a [`LockResult`] and the guard derefs to `T`.
pub(crate) struct RankedMutex<T, const RANK: u16> {
    inner: Mutex<T>,
}

impl<T, const RANK: u16> RankedMutex<T, RANK> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(value),
        }
    }

    /// Acquire the lock. Enters the rank first, so a misordered acquisition
    /// trips the debug assertion before it can block on the inner mutex.
    pub(crate) fn lock(&self) -> LockResult<RankedGuard<'_, T>> {
        let rank = RankGuard::enter(RANK);
        match self.inner.lock() {
            Ok(guard) => Ok(RankedGuard { guard, _rank: rank }),
            Err(poison) => Err(PoisonError::new(RankedGuard {
                guard: poison.into_inner(),
                _rank: rank,
            })),
        }
    }
}

/// Guard for a [`RankedMutex`]. Derefs to `T`. Releases the inner mutex first,
/// then the held rank (field declaration order), so the rank outlives the lock
/// it represents by an instant — never the reverse.
pub(crate) struct RankedGuard<'a, T> {
    guard: std::sync::MutexGuard<'a, T>,
    _rank: RankGuard,
}

impl<T> Deref for RankedGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for RankedGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}
