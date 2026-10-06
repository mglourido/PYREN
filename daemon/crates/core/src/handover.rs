//! Whether this start is a hand-over between users rather than a boot.
//!
//! Every module that can put a remembered setting back at startup does so
//! only when asked to (`restoreModeOnStart` and friends), because a daemon
//! coming up at boot has no business moving the fans to wherever somebody
//! last left them. A start that follows a change of user is not that case:
//! the daemon let go of the hardware a moment ago precisely so it could
//! come back with the other person's settings, and "their settings" that
//! stop at the config file - the curve loaded, the mode not applied - is
//! not what switching to them means.
//!
//! So the daemon binary marks such a start here, once, before any module
//! is built, and the modules read it next to their own opt-in. A flag
//! rather than a constructor argument because it is the same answer for
//! every module, and none of their tests should have to say it.

use std::sync::atomic::{AtomicBool, Ordering};

static IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// Marks this start as a hand-over. Call before constructing any module.
pub fn mark() {
    IN_PROGRESS.store(true, Ordering::SeqCst);
}

/// True when the settings on disk were just made someone's on purpose, and
/// are to be applied whether or not restoring at boot is switched on.
pub fn in_progress() -> bool {
    IN_PROGRESS.load(Ordering::SeqCst)
}
