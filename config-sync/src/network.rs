use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nmstate::NetworkState;

use crate::{has_extension, is_empty_source, list_names, Logger};

/// Mirrors `files/etc/sysctl.d/73-frr-bootc-vrf-strict-mode.conf`'s
/// value - that static file alone can't reliably apply it (the sysctl
/// node only exists once the `vrf` kernel module is loaded, which
/// happens on first VRF interface creation, normally *after*
/// systemd-sysctl.service already ran at boot), so it's re-applied here
/// after every successful sync instead.
const VRF_STRICT_MODE_PATH: &str = "/proc/sys/net/vrf/strict_mode";

/// Applies nmstate desired-state documents (`*.yml`/`*.yaml`) from `src`
/// (the mounted `network-config` ConfigMap) via the `nmstate` crate
/// directly - no `nmstatectl` subprocess, and no NetworkManager
/// `.nmconnection` keyfile support (nmstate-only, by design).
pub fn sync(src: &Path, log: &Logger) -> Result<()> {
    if is_empty_source(src) {
        log.log(format!(
            "{} is empty or not mounted, nothing to sync",
            src.display()
        ));
        return Ok(());
    }

    let names = list_names(src)?;
    log.log(format!(
        "starting sync from {} (files: {})",
        src.display(),
        names.join(" ")
    ));

    let mut statefiles = statefile_paths(src)?;
    statefiles.sort();

    for path in &statefiles {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        log.log(format!("applying nmstate state {name}"));

        let yaml = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let state = NetworkState::new_from_yaml(&yaml)
            .with_context(|| format!("failed to parse {name} as an nmstate desired state"))?;
        state
            .apply()
            .with_context(|| format!("failed to apply {name}"))?;

        log.log(format!("applied {name} successfully"));
    }

    apply_vrf_strict_mode(Path::new(VRF_STRICT_MODE_PATH), log);

    log.log("sync complete");
    Ok(())
}

/// Best-effort - not part of the sync's success/failure outcome, since
/// the actual network config already applied successfully by the time
/// this runs. `NotFound` (no VRF exists anywhere on the system yet, so
/// the `vrf` kernel module hasn't loaded and the sysctl node doesn't
/// exist) is expected and unremarkable on a VM with no VRF-using tenant
/// configured yet - anything else is worth a log line since it means a
/// VRF *does* exist but hardening it failed.
fn apply_vrf_strict_mode(path: &Path, log: &Logger) {
    match fs::write(path, "1\n") {
        Ok(()) => log.log("net.vrf.strict_mode=1 (VRF isolation hardening)"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            log.log("net.vrf.strict_mode not available yet (no VRF interfaces present) - skipping");
        }
        Err(e) => log.err(format!("failed to set net.vrf.strict_mode=1: {e}")),
    }
}

fn statefile_paths(src: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file() && has_extension(&path, &["yml", "yaml"]) {
            paths.push(path);
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_source_is_a_noop() {
        let src = tempfile::tempdir().unwrap();
        let log = Logger::new("test");
        sync(src.path(), &log).unwrap();
    }

    #[test]
    fn only_yml_and_yaml_files_are_picked_up() {
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("a.yml"), "interfaces: []\n").unwrap();
        fs::write(src.path().join("b.yaml"), "interfaces: []\n").unwrap();
        fs::write(src.path().join("c.nmconnection"), "[connection]\n").unwrap();
        fs::write(src.path().join("README"), "not config\n").unwrap();

        let mut paths = statefile_paths(src.path()).unwrap();
        paths.sort();
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.yml", "b.yaml"]);
    }

    #[test]
    fn vrf_strict_mode_writes_1_when_the_sysctl_node_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("strict_mode");
        fs::write(&path, "0\n").unwrap();
        let log = Logger::new("test");

        apply_vrf_strict_mode(&path, &log);

        assert_eq!(fs::read_to_string(&path).unwrap(), "1\n");
    }

    #[test]
    fn vrf_strict_mode_missing_node_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-such-vrf-module/strict_mode");
        let log = Logger::new("test");

        apply_vrf_strict_mode(&path, &log);
    }
}
