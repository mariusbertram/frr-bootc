//! Thin wrappers around `systemctl` plus journalctl access. No dbus/zbus
//! dependency: these are infrequent (once per REFRESH_SECS), and shelling
//! out to the same CLI an operator would run is a smaller,
//! more obviously-correct surface than hand-rolling a systemd D-Bus
//! client for three property reads.

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

/// Why a sync unit is unhealthy - the old `SyncHealth::Failed` carried no
/// distinction between "the daemon process is gone", "its status file
/// went stale" and "the last sync attempt reported an error", all three
/// of which have very different fixes, and the dashboard could only say
/// "FAILED" plus point at journalctl. The reason text is shown right in
/// the Sync Services panel instead.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum SyncHealth {
    /// Process up, status file fresh and reporting success. Carries how
    /// long ago the last sync attempt ran (the status file's mtime) so
    /// the panel can show "ok, 2m ago" - "ok" alone can't distinguish a
    /// sync that ran a minute ago from one that hasn't run since boot.
    Ok {
        age_secs: u64,
    },
    Failed {
        reason: String,
    },
    TimerNotActive,
}

impl SyncHealth {
    /// Short status word for the Sync Services table's status column.
    pub fn status_word(&self) -> &'static str {
        match self {
            SyncHealth::Ok { .. } => "ok",
            SyncHealth::Failed { .. } => "FAILED",
            SyncHealth::TimerNotActive => "timer not active",
        }
    }

    /// The human-readable "why" line for the table's detail column (and
    /// the health verdict) - for a failure, the actual cause; for Ok, how
    /// stale the last attempt is.
    pub fn detail(&self) -> String {
        match self {
            SyncHealth::Ok { age_secs } => format!("last attempt {}", fmt_age(*age_secs)),
            SyncHealth::Failed { reason } => reason.clone(),
            SyncHealth::TimerNotActive => "timer disabled - it will never run again".to_string(),
        }
    }
}

/// Seconds -> "just now"/"2m"/"1h 3m"-style compact age, used for status
/// ages and (potentially) anywhere else a duration is shown compactly.
pub fn fmt_age(secs: u64) -> String {
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        _ => format!("{}h {}m ago", secs / 3600, secs % 3600 / 60),
    }
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
        SyncHealth::Failed {
            reason: "last run failed".to_string(),
        }
    } else if !is_active(timer) {
        SyncHealth::TimerNotActive
    } else {
        SyncHealth::Ok { age_secs: 0 }
    }
}

/// Failed if the daemon process itself isn't running (systemd gave up
/// restarting it, or it never started), or if its status file is
/// missing/stale/reports an error; Ok only when the process is up *and*
/// its most recent attempt reported success recently.
fn sync_health_daemon(service: &str, status_file: &str) -> SyncHealth {
    if !is_active(service) {
        return SyncHealth::Failed {
            reason: "process not running".to_string(),
        };
    }
    match read_recent_ok(status_file) {
        StatusRead::Ok { age_secs } => SyncHealth::Ok { age_secs },
        StatusRead::Missing => SyncHealth::Failed {
            reason: "status file missing (never synced?)".to_string(),
        },
        StatusRead::Stale(age_secs) => SyncHealth::Failed {
            reason: format!("status stale ({}), daemon may be stuck", fmt_age(age_secs)),
        },
        StatusRead::Error(content) => SyncHealth::Failed {
            // The status file's content IS the daemon's own error line
            // from its last attempt - the most direct answer to "why is
            // this red" there is, no journalctl detour needed.
            reason: content,
        },
    }
}

/// The outcome of reading a daemon's status file, each case mapped to a
/// distinct failure reason by the caller - the point of the enum is that
/// "missing", "stale" and "reports an error" are three different
/// problems an operator would triage differently.
#[derive(Debug)]
enum StatusRead {
    Ok { age_secs: u64 },
    Missing,
    Stale(u64),
    Error(String),
}

fn read_recent_ok(status_file: &str) -> StatusRead {
    let Ok(meta) = fs::metadata(status_file) else {
        return StatusRead::Missing;
    };
    let Ok(modified) = meta.modified() else {
        return StatusRead::Missing;
    };
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    if age > MAX_STATUS_AGE {
        return StatusRead::Stale(age.as_secs());
    }
    match fs::read_to_string(status_file) {
        Ok(s) if s.trim() == "ok" => StatusRead::Ok {
            age_secs: age.as_secs(),
        },
        Ok(s) => StatusRead::Error(s.trim().to_string()),
        Err(_) => StatusRead::Error("status file unreadable".to_string()),
    }
}

/// The last `lines` journal lines of a unit, newest last - what the
/// console's journal popup shows when a sync service is selected. Best
/// effort: any failure (journalctl missing/broken) yields `None` and the
/// popup says so rather than erroring. stdout/stderr are explicitly
/// captured (never inherited) for the same frame-corruption reason every
/// other subprocess call here nulls or captures its output.
pub fn recent_journal(unit: &str, lines: usize) -> Option<String> {
    let out = Command::new("journalctl")
        .args([
            "-u",
            unit,
            "-n",
            &lines.to_string(),
            "--no-pager",
            "-o",
            "short",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    Some(text.trim_end().to_string())
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
    fn missing_status_file_reports_missing() {
        let path = temp_path("missing");
        let _ = fs::remove_file(&path);
        assert!(matches!(
            read_recent_ok(path.to_str().unwrap()),
            StatusRead::Missing
        ));
    }

    #[test]
    fn fresh_ok_status_is_ok_with_age() {
        let path = temp_path("fresh-ok");
        fs::write(&path, "ok\n").unwrap();
        assert!(matches!(
            read_recent_ok(path.to_str().unwrap()),
            StatusRead::Ok { .. }
        ));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn fresh_error_status_carries_the_error_content() {
        let path = temp_path("fresh-error");
        fs::write(&path, "error: vtysh -C failed\n").unwrap();
        match read_recent_ok(path.to_str().unwrap()) {
            StatusRead::Error(content) => {
                assert_eq!(content, "error: vtysh -C failed")
            }
            other => panic!("expected Error, got {other:?}"),
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn stale_ok_status_reports_stale_with_age() {
        let path = temp_path("stale-ok");
        fs::write(&path, "ok\n").unwrap();
        set_mtime(&path, SystemTime::now() - StdDuration::from_secs(3600));
        match read_recent_ok(path.to_str().unwrap()) {
            StatusRead::Stale(age) => assert!(age >= 3600),
            other => panic!("expected Stale, got {other:?}"),
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn failure_reasons_are_distinct_per_cause() {
        let process = SyncHealth::Failed {
            reason: "process not running".to_string(),
        };
        let stale = SyncHealth::Failed {
            reason: "status stale (6m ago), daemon may be stuck".to_string(),
        };
        // The whole point of carrying reasons: three red rows must not be
        // indistinguishable "FAILED"s anymore.
        assert_ne!(process.detail(), stale.detail());
        assert_eq!(
            SyncHealth::TimerNotActive.detail(),
            "timer disabled - it will never run again"
        );
        assert_eq!(
            SyncHealth::Ok { age_secs: 120 }.detail(),
            "last attempt 2m ago"
        );
    }

    #[test]
    fn fmt_age_buckets() {
        assert_eq!(fmt_age(10), "just now");
        assert_eq!(fmt_age(120), "2m ago");
        assert_eq!(fmt_age(3900), "1h 5m ago");
    }
}
