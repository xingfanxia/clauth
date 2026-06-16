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
//! - `partition_due`: `last_fetched` → `activity`.
//! - `apply_usage`: `usage_store` → `usage_status` → `config`.
//! - rotation/save sites: `config` → state flock → `activity`.
//!
//! Standalone leaves (`refetch_queue`, the `pending_*` sets, …) are never nested
//! with another tracked lock; they are ranked above the rest so that a future
//! accidental nesting under any held lock still *increases* the rank rather than
//! inverting it.

use std::ops::{Deref, DerefMut};
use std::sync::{LockResult, Mutex, PoisonError};

/// Sealed so only the rank markers defined in [`rank`] can implement [`Rank`].
/// Nothing outside this module can name a fresh `Rank` type, which is what makes
/// arbitrary-rank [`RankedMutex`] / [`RankGuard`] construction impossible.
mod sealed {
    pub(crate) trait Sealed {}
}

/// A position in the global lock order. Implemented only by the zero-sized
/// markers in [`rank`]; the sealed supertrait blocks any other implementation.
/// `VALUE` is the rank's u16 weight — lower = acquired earlier (outer).
pub(crate) trait Rank: sealed::Sealed {
    // Only read inside the `cfg(debug_assertions)` rank check in
    // `RankGuard::enter`; release builds compile that read out, leaving the
    // const unreferenced. The order it encodes is still load-bearing.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    const VALUE: u16;
}

/// Global lock order. Lower value = outer. Gaps leave room to insert future
/// locks without renumbering; only the relative order matters. Each rank is an
/// uninhabited marker implementing [`Rank`]; the raw u16 weights are private to
/// this module so the order can't be forged elsewhere.
pub(crate) mod rank {
    use super::{Rank, sealed::Sealed};

    /// Defines a rank marker, its `Rank::VALUE`, and the `Sealed` impl in one
    /// shot so a new rank can never be added half-sealed.
    macro_rules! ranks {
        ($($(#[$m:meta])* $name:ident = $value:literal;)*) => {$(
            $(#[$m])*
            pub(crate) enum $name {}
            impl Sealed for $name {}
            impl Rank for $name {
                const VALUE: u16 = $value;
            }
        )*};
    }

    ranks! {
        /// `RotationGuard` (per-profile rotation flock). Held across HTTP, outermost.
        Rotation = 100;
        LastFetched = 200;
        /// `/profile` re-fetch TTL clock in `usage::fetch`. Leaf — acquired and
        /// released inside the profile-fetch decision, never under another lock.
        ProfileTtl = 210;
        Tokens = 250;
        ThirdParty = 260;
        ThirdPartyUsageStore = 270;
        ThirdPartyStatus = 280;
        UsageStore = 300;
        UsageStatus = 350;
        Config = 400;
        /// `with_state_lock` (cross-process state flock). Inner of `config`.
        State = 500;
        Activity = 600;
        // Standalone leaves — never nested with another tracked lock.
        NextRefresh = 1100;
        RefetchQueue = 1200;
        PendingSwitch = 1500;
        PendingSwitchOff = 1700;
    }
}

#[cfg(debug_assertions)]
thread_local! {
    /// Ranks currently held by this thread, in acquisition order.
    static HELD: std::cell::RefCell<Vec<u16>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Tracks one held rank on the current thread; pops it on drop. Used directly by
/// the two file-lock guards ([`crate::lock`]'s state flock and
/// [`crate::runtime::RotationGuard`]) — not [`Mutex`]es but still in the order.
#[must_use]
pub(crate) struct RankGuard {
    #[cfg(debug_assertions)]
    rank: u16,
}

impl RankGuard {
    /// Enter rank `R`, asserting it is strictly greater than the highest rank the
    /// current thread already holds. No-op in release builds. `R` can only name a
    /// marker from [`rank`], so the rank entered is always a real position in the
    /// global order.
    #[inline]
    pub(crate) fn enter<R: Rank>() -> Self {
        #[cfg(debug_assertions)]
        {
            let rank = R::VALUE;
            HELD.with(|h| {
                let mut h = h.borrow_mut();
                debug_assert!(
                    h.last().is_none_or(|&top| rank > top),
                    "lock-order violation: acquiring rank {rank} while holding {:?} \
                     (would invert the global lock order and risk deadlock)",
                    h.as_slice(),
                );
                h.push(rank);
            });
            Self { rank }
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
pub(crate) struct RankedMutex<T, R: Rank> {
    inner: Mutex<T>,
    _rank: std::marker::PhantomData<R>,
}

impl<T, R: Rank> RankedMutex<T, R> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(value),
            _rank: std::marker::PhantomData,
        }
    }

    /// Acquire the lock. Enters the rank first, so a misordered acquisition
    /// trips the debug assertion before it can block on the inner mutex.
    pub(crate) fn lock(&self) -> LockResult<RankedGuard<'_, T>> {
        let rank = RankGuard::enter::<R>();
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
