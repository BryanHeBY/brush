//! Tracks a hangup requested for the current interactive session.
//!
//! The signal handler itself only records the request; the input backends poll it and return
//! control to the interactive loop, which saves history and runs the normal end-of-session
//! path. That keeps signal-context work to a single atomic store.

use std::sync::{
    Arc, LazyLock,
    atomic::{AtomicBool, Ordering},
};

static HANGUP: LazyLock<Arc<AtomicBool>> = LazyLock::new(|| Arc::new(AtomicBool::new(false)));
/// Reedline consumes (swaps back to false) the flag it is given; keep a separate record so the
/// interactive loop still knows the session was hung up.
#[cfg(any(unix, reedline_backend))]
static BREAK: LazyLock<Arc<AtomicBool>> = LazyLock::new(|| Arc::new(AtomicBool::new(false)));

/// Returns a handle that a line editor can poll for a hangup.
#[cfg(reedline_backend)]
pub(crate) fn signal() -> Arc<AtomicBool> {
    BREAK.clone()
}

/// Clears a previously recorded hangup. Called when an interactive session starts.
pub(crate) fn reset() {
    HANGUP.store(false, Ordering::SeqCst);
    #[cfg(any(unix, reedline_backend))]
    BREAK.store(false, Ordering::SeqCst);
}

/// Returns whether a hangup was requested for this session.
pub(crate) fn requested() -> bool {
    HANGUP.load(Ordering::SeqCst)
}

/// Records that a hangup was requested.
#[cfg(any(unix, reedline_backend))]
pub(crate) fn request() {
    HANGUP.store(true, Ordering::SeqCst);
    BREAK.store(true, Ordering::SeqCst);
}
