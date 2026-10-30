//! Adapter operational defaults.

use std::time::Duration;

/// One end-to-end budget for each public operation, including all CLI calls.
pub(crate) const OPERATION_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum idle pause between nonblocking pipe and child-status checks.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(5);
