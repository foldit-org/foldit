//! Platform-specific socket functionality

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-process serial counter, appended to socket names so two
/// listeners created within the same millisecond on the same host get
/// distinct paths. Concurrent test binaries (cargo test runs tests in
/// separate threads of one process, both calling
/// `ensure_plugin_registered`) hit this race in practice.
static SOCKET_SERIAL: AtomicU64 = AtomicU64::new(0);

/// Generate a platform-appropriate socket name for a plugin.
///
/// Returns a unique socket name with platform-specific prefix:
/// - Windows: `@foldit-runner-{plugin}-{timestamp}-{serial}`
/// - Unix: `/tmp/foldit-runner-{plugin}-{timestamp}-{serial}`
///
/// # Panics
///
/// Panics if the system clock is before the Unix epoch.
#[must_use]
#[allow(clippy::unwrap_used)]
pub fn socket_name_for_plugin(plugin_id: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let serial = SOCKET_SERIAL.fetch_add(1, Ordering::Relaxed);

    if cfg!(windows) {
        format!("@foldit-runner-{plugin_id}-{timestamp}-{serial}")
    } else {
        format!("/tmp/foldit-runner-{plugin_id}-{timestamp}-{serial}")
    }
}
