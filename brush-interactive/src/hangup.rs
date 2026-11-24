//! Tracks a hangup requested for the current interactive session.
//!
//! The signal handler itself only records the request; the input backends poll it and return
//! control to the interactive loop, which saves history and runs the normal end-of-session
//! path. That keeps signal-context work to a single atomic store.

use std::sync::atomic::{AtomicBool, Ordering};

static HANGUP: AtomicBool = AtomicBool::new(false);

/// Clears a previously recorded hangup. Called when an interactive session starts.
pub(crate) fn reset() {
    HANGUP.store(false, Ordering::SeqCst);
}

/// Returns whether a hangup was requested for this session.
pub(crate) fn requested() -> bool {
    HANGUP.load(Ordering::SeqCst)
}

/// Records that a hangup was requested.
pub(crate) fn request() {
    HANGUP.store(true, Ordering::SeqCst);
}
