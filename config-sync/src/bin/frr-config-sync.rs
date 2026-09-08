//! Syncs FRR configuration from the mounted "frr-config" ConfigMap
//! (`/run/config/frr`, a read-only virtiofs mount) into `/etc/frr`, then
//! reloads or restarts FRR as needed. Runs as a persistent daemon
//! (`frr-config-sync.service`, `Type=simple`, started once at boot, no
//! `.timer`) that re-applies on its own internal
//! [`config_sync::POLL_INTERVAL`] - see [`config_sync::frr::sync`] for
//! what one iteration actually does (staging, vty-socket validation,
//! rsync, then either `frr-reload.py --reload` or a full `systemctl
//! restart frr.service` if the enabled daemon set changed).
//!
//! A single sync attempt failing does *not* stop this process - it logs
//! the error, writes it to the status file the console dashboard reads,
//! and tries again next tick, exactly like the old timer-triggered
//! oneshot script did.
//!
//! Logs verbosely at every stage on purpose (`journalctl -u
//! frr-config-sync.service`): this VM has no dashboard/alerting beyond
//! the console (see console/src/main.rs), console access is the only
//! way in, and a vague "something failed" here used to mean piecing the
//! actual cause back together by hand.

use std::path::Path;
use std::process::ExitCode;

use config_sync::{Lock, Logger, SystemRunner};

const LOCK_PATH: &str = "/run/frr-config-sync.lock";
const STATUS_PATH: &str = "/run/frr-config-sync.status";
const SRC: &str = "/run/config/frr";
const DST: &str = "/etc/frr";

fn main() -> ExitCode {
    let log = Logger::new("frr-config-sync");

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

    // Held for the process's entire lifetime - run_forever never
    // returns, so this is never explicitly dropped, only released on
    // process exit.
    let _lock = lock;

    config_sync::run_forever(Path::new(STATUS_PATH), &log, || {
        config_sync::frr::sync(Path::new(SRC), Path::new(DST), &SystemRunner, &log)
    })
}
