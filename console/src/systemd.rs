//! Thin wrappers around `systemctl`. No dbus/zbus dependency: these are
//! infrequent (once per REFRESH_SECS), and shelling out to the same CLI an
//! operator would run is a smaller, more obviously-correct surface than
//! hand-rolling a systemd D-Bus client for three property reads.

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

// systemctl is-active/is-failed print the state word to stdout in
// addition to signaling it via the exit code - only the exit code is
// used here, but with the default inherited stdio, that printed word
// would go straight to the real terminal (not through ratatui, which
// only controls what it itself writes) and land wherever the cursor
// happened to be, stomping the frame mid-render. Every call here
// explicitly nulls both stdout and stderr for exactly that reason - this
// bit the dashboard for real (a wall of stray "active"/"inactive"/"ok"
// fragments scattered across the screen, reported live).
fn quiet(cmd: &mut Command) -> &mut Command {
    cmd.stdout(Stdio::null()).stderr(Stdio::null())
}

pub fn is_active(unit: &str) -> bool {
    quiet(Command::new("systemctl").args(["is-active", unit]))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn is_failed(unit: &str) -> bool {
    // is-failed exits 0 (and prints "failed") exactly when the unit is in
    // the failed state - that exit code IS the answer, no stdout parsing
    // needed.
    quiet(Command::new("systemctl").args(["is-failed", unit]))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub enum SyncHealth {
    Ok,
    Failed,
    TimerNotActive,
}

/// How to determine a sync unit's health - the two shapes this codebase
/// uses side by side (see `snapshot::SYNC_UNITS`).
pub enum SyncKind {
    /// A oneshot `.service` driven by a `.timer` (bootc-image-sync).
    Timer { timer: &'static str },
    /// A persistent `Type=simple`/`Restart=always` daemon with no timer
    /// (frr-config-sync, network-config-sync) that writes a one-line
    /// status (`ok`/`error: ...`) to `status_file` after every sync
    /// attempt, since the process's own exit code no longer reflects
    /// that - it doesn't exit on a failed attempt, it retries.
    Daemon { status_file: &'static str },
}

/// How stale `status_file` can be before a `Daemon`-kind unit counts as
/// unhealthy even if the process is still running - a bit over 2x
/// `config_sync::POLL_INTERVAL` (2min), so one slow-but-fine cycle
/// doesn't flip this red, but a genuinely stuck daemon does.
const MAX_STATUS_AGE: Duration = Duration::from_secs(300);

/// Health of a sync unit, dispatched on its `SyncKind`. This is the main
/// "is this VM actually being kept in sync" signal -
/// frr-config-sync/network-config-sync/bootc-image-sync all fail
/// silently otherwise (journalctl only, no alerting).
pub fn sync_health(service: &str, kind: &SyncKind) -> SyncHealth {
    match kind {
        SyncKind::Timer { timer } => sync_health_timer(service, timer),
        SyncKind::Daemon { status_file } => sync_health_daemon(service, status_file),
    }
}

/// Failed if the service's last run failed, TimerNotActive if the timer
/// itself isn't active (so it'll never run again until that's fixed),
/// Ok otherwise.
fn sync_health_timer(service: &str, timer: &str) -> SyncHealth {
    if is_failed(service) {
        SyncHealth::Failed
    } else if !is_active(timer) {
        SyncHealth::TimerNotActive
    } else {
        SyncHealth::Ok
    }
}

/// Failed if the daemon process itself isn't running (systemd gave up
/// restarting it, or it never started), or if its status file is
/// missing/stale/reports an error; Ok only when the process is up *and*
/// its most recent attempt reported success recently.
fn sync_health_daemon(service: &str, status_file: &str) -> SyncHealth {
    if !is_active(service) {
        return SyncHealth::Failed;
    }
    if read_recent_ok(status_file) {
        SyncHealth::Ok
    } else {
        SyncHealth::Failed
    }
}

fn read_recent_ok(status_file: &str) -> bool {
    let Ok(meta) = fs::metadata(status_file) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    if SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age > MAX_STATUS_AGE)
    {
        return false;
    }
    fs::read_to_string(status_file)
        .map(|s| s.trim() == "ok")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration as StdDuration, SystemTime};

    /// A unique path under the system temp dir - avoids pulling in the
    /// `tempfile` crate just for these few tests.
    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "frr-console-test-{}-{name}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    fn set_mtime(path: &std::path::Path, when: SystemTime) {
        let file = fs::File::open(path).unwrap();
        file.set_modified(when).unwrap();
    }

    #[test]
    fn missing_status_file_is_not_recent_ok() {
        let path = temp_path("missing");
        let _ = fs::remove_file(&path);
        assert!(!read_recent_ok(path.to_str().unwrap()));
    }

    #[test]
    fn fresh_ok_status_is_recent_ok() {
        let path = temp_path("fresh-ok");
        fs::write(&path, "ok\n").unwrap();
        assert!(read_recent_ok(path.to_str().unwrap()));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn fresh_error_status_is_not_recent_ok() {
        let path = temp_path("fresh-error");
        fs::write(&path, "error: boom\n").unwrap();
        assert!(!read_recent_ok(path.to_str().unwrap()));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn stale_ok_status_is_not_recent_ok() {
        let path = temp_path("stale-ok");
        fs::write(&path, "ok\n").unwrap();
        set_mtime(&path, SystemTime::now() - StdDuration::from_secs(3600));
        assert!(!read_recent_ok(path.to_str().unwrap()));
        let _ = fs::remove_file(&path);
    }
}
