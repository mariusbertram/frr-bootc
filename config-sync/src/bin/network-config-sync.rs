use std::path::Path;
use std::process::ExitCode;

use config_sync::{Lock, Logger};

const LOCK_PATH: &str = "/run/network-config-sync.lock";
const SRC: &str = "/run/config/network";

fn main() -> ExitCode {
    let log = Logger::new("network-config-sync");

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

    let result = config_sync::network::sync(Path::new(SRC), &log);
    drop(lock);

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log.err(format!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}
