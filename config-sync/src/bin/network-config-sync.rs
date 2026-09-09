//! Applies network configuration from the mounted "network-config"
//! ConfigMap (`/run/config/network`, a read-only virtiofs mount) via the
//! `nmstate` crate. Runs as a persistent daemon
//! (`network-config-sync.service`, `Type=simple`, `Wants=`/`After=
//! NetworkManager.service`, no `.timer`) that re-applies on its own
//! internal [`config_sync::POLL_INTERVAL`] - see
//! [`config_sync::network::sync`] for what one iteration actually does.
//! nmstate documents only (`*.yml`/`*.yaml`) - no NetworkManager
//! `.nmconnection` keyfile support.
//!
//! A single sync attempt failing does *not* stop this process - it logs
//! the error, writes it to the status file the console dashboard reads,
//! and tries again next tick, exactly like the old timer-triggered
//! oneshot script did.
//!
//! Logs verbosely at every stage on purpose (`journalctl -u
//! network-config-sync.service`), same reasoning as frr-config-sync.rs.

use std::path::Path;
use std::process::ExitCode;

use config_sync::{Lock, Logger};

const LOCK_PATH: &str = "/run/network-config-sync.lock";
const STATUS_PATH: &str = "/run/network-config-sync.status";
const SRC: &str = "/run/config/network";

fn main() -> ExitCode {
    let log = Logger::new("network-config-sync");

    let lock = match Lock::try_acquire(Path::new(LOCK_PATH)) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            log.err("another instance is already running, exiting");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            log.err(format!("failed to acquire lock {LOCK_PATH}: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let _lock = lock;

    config_sync::run_forever(Path::new(STATUS_PATH), &log, || {
        config_sync::network::sync(Path::new(SRC), &log)
    })
}
