//! Crash checkpoints.
//!
//! Every write, fsync, link and projection commit boundary in `rewrite` and
//! `open_with` calls `hit` with a label. In normal builds that is a no-op.
//! With the `fault-injection` feature, a test can `arm` the crate so the
//! n-th checkpoint returns an error, which the code treats exactly like a
//! failed syscall: it must clean up, leave the source byte-identical and
//! leave no half-written output under its final name. The fault matrix in the
//! test suite sweeps `n` over every checkpoint a run reaches.

#[cfg(feature = "fault-injection")]
mod imp {
    use std::cell::{Cell, RefCell};

    use crate::{Error, Result};

    thread_local! {
        static ARMED: Cell<Option<u64>> = const { Cell::new(None) };
        static SEEN: Cell<u64> = const { Cell::new(0) };
        static LABELS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    }

    /// Fail the `n`-th checkpoint (0-based) this thread reaches from now on,
    /// and reset the count. Only one checkpoint fails per arming.
    pub fn arm(n: u64) {
        ARMED.with(|a| a.set(Some(n)));
        SEEN.with(|s| s.set(0));
        LABELS.with(|l| l.borrow_mut().clear());
    }

    /// Disarm and return how many checkpoints were reached since [`arm`].
    pub fn disarm() -> u64 {
        ARMED.with(|a| a.set(None));
        SEEN.with(|s| s.get())
    }

    /// Labels of every checkpoint reached since [`arm`], in order.
    pub fn labels() -> Vec<&'static str> {
        LABELS.with(|l| l.borrow().clone())
    }

    pub(crate) fn hit(label: &'static str) -> Result<()> {
        let seen = SEEN.with(|s| {
            let v = s.get();
            s.set(v + 1);
            v
        });
        LABELS.with(|l| l.borrow_mut().push(label));
        if ARMED.with(|a| a.get()) == Some(seen) {
            ARMED.with(|a| a.set(None));
            return Err(Error::Log(format!(
                "injected crash at checkpoint {seen} ({label})"
            )));
        }
        Ok(())
    }
}

#[cfg(feature = "fault-injection")]
pub use imp::{arm, disarm, labels};

#[cfg(feature = "fault-injection")]
pub(crate) use imp::hit;

/// Checkpoint in builds without fault injection: always passes.
#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub(crate) fn hit(_label: &'static str) -> crate::Result<()> {
    Ok(())
}
