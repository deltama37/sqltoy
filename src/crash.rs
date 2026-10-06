//! Named points where a process can abort, for crash-recovery tests.
//!
//! [`crash_point`] reads `SQLTOY_CRASH_AT` on every call. When the value equals
//! `name`, the process aborts without unwinding.

use std::process;

/// Aborts when `SQLTOY_CRASH_AT` equals `name`.
///
/// The names used by the commit protocol are `wal-partial`,
/// `before-wal-sync`, `after-wal-sync`, `mid-checkpoint`, and
/// `before-wal-truncate`.
pub fn crash_point(name: &str) {
    if std::env::var("SQLTOY_CRASH_AT").ok().as_deref() == Some(name) {
        process::abort();
    }
}
