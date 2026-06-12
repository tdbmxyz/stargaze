//! Runtime logging knobs shared by all pipeline stages.
//!
//! Periodic progress logs (frame counts every few hundred frames) are
//! useful when diagnosing a stalled pipeline but add a line every couple
//! of seconds during a healthy session, so they are off by default and
//! enabled with `--log-progress` on either binary. One-shot lifecycle
//! logs (first frame, stage start/stop) are not affected.

use std::sync::atomic::{AtomicBool, Ordering};

static PROGRESS_LOGGING: AtomicBool = AtomicBool::new(false);

/// Enables or disables periodic progress logs process-wide.
pub fn set_progress_logging(enabled: bool) {
    PROGRESS_LOGGING.store(enabled, Ordering::Relaxed);
}

/// Returns whether periodic progress logs should be emitted.
#[must_use]
pub fn progress_logging() -> bool {
    PROGRESS_LOGGING.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_logging_defaults_off_and_toggles() {
        assert!(!progress_logging());
        set_progress_logging(true);
        assert!(progress_logging());
        set_progress_logging(false);
        assert!(!progress_logging());
    }
}
