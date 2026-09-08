//! Syncs FRR configuration from the mounted "frr-config" ConfigMap
//! (`/run/config/frr`, a read-only virtiofs mount) into `/etc/frr`, then
//! reloads or restarts FRR as needed. Triggered at boot and periodically
//! by frr-config-sync.timer - see [`config_sync::frr::sync`] for the
//! full control flow (staging, vty-socket validation, rsync, then either
//! `frr-reload.py --reload` or a full `systemctl restart frr.service` if
//! the enabled daemon set changed).
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
const SRC: &str = "/run/config/frr";
const DST: &str = "/etc/frr";

fn main() -> ExitCode {
    let log = Logger::new("frr-config-sync");

    let lock = match Lock::try_acquire(Path::new(LOCK_PATH)) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            log.log("another run is already in progress, skipping");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            log.err(format!("failed to acquire lock {LOCK_PATH}: {e}"));
            return ExitCode::FAILURE;
        }
    };

    let result = config_sync::frr::sync(Path::new(SRC), Path::new(DST), &SystemRunner, &log);
    drop(lock);

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log.err(format!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}
