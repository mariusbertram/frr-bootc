//! Thin wrappers around `systemctl`. No dbus/zbus dependency: these are
//! infrequent (once per REFRESH_SECS), and shelling out to the same CLI an
//! operator would run is a smaller, more obviously-correct surface than
//! hand-rolling a systemd D-Bus client for three property reads.

use std::process::{Command, Stdio};

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

/// Health of a periodic sync unit pair (a oneshot .service + the .timer
/// driving it): Failed if the service's last run failed, TimerNotActive if
/// the timer itself isn't active (so it'll never run again until that's
/// fixed), Ok otherwise. This is the main "is this VM actually being kept
/// in sync" signal - frr-config-sync/network-config-sync/bootc-image-sync
/// all fail silently otherwise (journalctl only, no alerting).
pub fn sync_health(service: &str, timer: &str) -> SyncHealth {
    if is_failed(service) {
        SyncHealth::Failed
    } else if !is_active(timer) {
        SyncHealth::TimerNotActive
    } else {
        SyncHealth::Ok
    }
}
