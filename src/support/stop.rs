//! Ends what a server call is running early: the client cancelled the request, closed its input or the
//! server got SIGTERM.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Ends a running call early (see the module docs). Every clone sees the same state.
#[derive(Debug, Clone, Default)]
pub struct Stop(Arc<StopState>);

#[derive(Debug, Default)]
struct StopState {
    /// The request running now was cancelled.
    cancelled: AtomicBool,
    /// The input ended or the server got SIGTERM: whatever runs now or later is stopped.
    ended: AtomicBool,
    /// The server got SIGTERM: it stops serving too.
    terminated: AtomicBool,
}

impl Stop {
    /// A new request starts: the cancel of the one before no longer counts.
    pub fn begin(&self) {
        self.0.cancelled.store(false, Ordering::SeqCst);
    }

    /// Stops the request running now.
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
    }

    /// Stops what runs now and every call after it: the input ended.
    pub fn end(&self) {
        self.0.ended.store(true, Ordering::SeqCst);
    }

    /// Stops what runs now and every call after it, and the server: it got SIGTERM.
    pub fn terminate(&self) {
        self.0.terminated.store(true, Ordering::SeqCst);
        self.end();
    }

    /// True when the request running now was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// True once the server got SIGTERM.
    pub fn is_terminated(&self) -> bool {
        self.0.terminated.load(Ordering::SeqCst)
    }

    /// True when a call running now is to stop.
    pub fn is_stopped(&self) -> bool {
        self.is_cancelled() || self.0.ended.load(Ordering::SeqCst)
    }
}
