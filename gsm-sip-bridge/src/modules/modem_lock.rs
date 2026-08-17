//! A modem lock you cannot wait on forever (specs/039-at-stall-watchdog).
//!
//! Several threads in one process contend for a single AT port: the
//! registration/renewal path, the modem SMS sweep, and the VoLTE attach and
//! attachment checks. They were serialised with a plain `std::sync::Mutex`,
//! which has no timed acquisition — so a holder that wedged took every waiter
//! with it, permanently and with no diagnostic.
//!
//! That was not hypothetical. The SMS sweep runs every 20 seconds and holds
//! this lock while it issues AT commands; the renewal path acquires it once an
//! hour. A sweep that blocked forever inside an AT read therefore also blocked
//! the next renewal forever — the registration lapsed, and the line went
//! unreachable by a completely different route than the one first diagnosed.
//!
//! Bounding the wait does not make the *holder* recover; the stall watchdog
//! handles that. What it does is stop one stuck activity silently converting
//! into several, and turn "blocked forever with no log line" into an ordinary
//! error the caller already knows how to retry with backoff.

use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// How long to wait for the modem before giving up.
///
/// Derived from the longest legitimate hold, which is one SMS sweep step: an
/// `open_with_retry` (4 attempts, 300ms linear backoff, ~1.8s worst case) plus
/// one or two AT commands at the 5s default timeout. 20s leaves room for that
/// without being so long that a waiter is effectively unbounded.
///
/// Note this is deliberately *shorter* than the watchdog's renewal budget: a
/// renewal that cannot get the modem should fail and retry on its own backoff,
/// which is a normal recoverable outcome, rather than sitting on the lock long
/// enough to be mistaken for a stall.
pub const MODEM_LOCK_TIMEOUT: Duration = Duration::from_secs(20);

/// A mutual-exclusion lock over one modem, with a bounded acquire.
///
/// Built from `Mutex<bool>` + `Condvar` rather than wrapping the guarded value,
/// because every call site here guards *the physical port*, not a Rust value —
/// the port is reached through a path, opened afresh by whoever holds the lock.
#[derive(Debug, Default)]
pub struct ModemLock {
    held: Mutex<bool>,
    released: Condvar,
}

impl ModemLock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire the modem, waiting at most [`MODEM_LOCK_TIMEOUT`].
    ///
    /// Returns `None` if the wait expired, which the caller should treat as an
    /// ordinary "could not do this now" failure.
    pub fn lock_timeout(&self, timeout: Duration) -> Option<ModemGuard<'_>> {
        let guard = self.held.lock().unwrap_or_else(|e| e.into_inner());
        let (mut guard, wait) = self
            .released
            .wait_timeout_while(guard, timeout, |held| *held)
            .unwrap_or_else(|e| e.into_inner());
        if wait.timed_out() && *guard {
            return None;
        }
        *guard = true;
        Some(ModemGuard { lock: self })
    }

    /// Acquire with the default timeout.
    pub fn lock(&self) -> Option<ModemGuard<'_>> {
        self.lock_timeout(MODEM_LOCK_TIMEOUT)
    }

    fn release(&self) {
        let mut guard: MutexGuard<'_, bool> = self.held.lock().unwrap_or_else(|e| e.into_inner());
        *guard = false;
        // One waiter is enough: the lock is exclusive, so waking the rest would
        // only have them contend and go back to sleep.
        self.released.notify_one();
    }
}

/// Releases the modem on drop, including on an early return or an unwind.
#[derive(Debug)]
pub struct ModemGuard<'a> {
    lock: &'a ModemLock,
}

impl Drop for ModemGuard<'_> {
    fn drop(&mut self) {
        self.lock.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn an_uncontended_lock_is_acquired_immediately() {
        let l = ModemLock::new();
        let started = Instant::now();
        assert!(l.lock().is_some());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn the_lock_is_released_on_drop_and_can_be_retaken() {
        let l = ModemLock::new();
        {
            let _g = l.lock().expect("first acquire");
        }
        assert!(l.lock().is_some(), "must be reacquirable after the drop");
    }

    #[test]
    fn a_waiter_gives_up_rather_than_blocking_on_a_wedged_holder() {
        // The whole point: before this, a holder stuck in an AT read took every
        // other user of the modem down with it, forever and silently.
        let l = Arc::new(ModemLock::new());
        let held = Arc::clone(&l);
        let (tx, rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _g = held.lock().expect("holder acquires");
            tx.send(()).expect("signal acquired");
            // Stands in for a wedged AT read.
            std::thread::sleep(Duration::from_secs(2));
        });
        rx.recv().expect("holder acquired the lock");

        let started = Instant::now();
        assert!(
            l.lock_timeout(Duration::from_millis(200)).is_none(),
            "the waiter must give up, not block until the holder finishes"
        );
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(150) && waited < Duration::from_secs(1),
            "should wait roughly its timeout, waited {waited:?}"
        );
        holder.join().expect("holder thread");
    }

    #[test]
    fn a_waiter_acquires_once_the_holder_releases() {
        let l = Arc::new(ModemLock::new());
        let held = Arc::clone(&l);
        let (tx, rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let g = held.lock().expect("holder acquires");
            tx.send(()).expect("signal acquired");
            std::thread::sleep(Duration::from_millis(100));
            drop(g);
        });
        rx.recv().expect("holder acquired the lock");
        assert!(
            l.lock_timeout(Duration::from_secs(5)).is_some(),
            "a waiter with headroom must get the lock once it is free"
        );
        holder.join().expect("holder thread");
    }

    #[test]
    fn the_default_timeout_outlasts_one_sweep_step_but_is_not_unbounded() {
        // Pins the derivation in the constant's docs: an open_with_retry
        // (~1.8s) plus a couple of AT commands at the 5s default.
        let worst_hold =
            Duration::from_millis(1800) + crate::modules::at_commander::DEFAULT_TIMEOUT * 2;
        assert!(
            MODEM_LOCK_TIMEOUT > worst_hold,
            "{MODEM_LOCK_TIMEOUT:?} must exceed one legitimate hold ({worst_hold:?})"
        );
        assert!(
            MODEM_LOCK_TIMEOUT < Duration::from_secs(60),
            "a 'bounded' wait that long is not meaningfully bounded"
        );
    }
}
