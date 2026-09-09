use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nmstate::NetworkState;

use crate::{has_extension, is_empty_source, list_names, Logger};

/// Mirrors `files/etc/sysctl.d/73-frr-bootc-vrf-strict-mode.conf`'s
/// value. `files/etc/modules-load.d/vrf.conf` loads the `vrf` kernel
/// module at boot so that static file actually applies from boot - this
/// is a defensive fallback re-applying the same value after every
/// successful sync, in case that ever isn't the case (e.g. a kernel
/// where `vrf` isn't a loadable module the same way).
const VRF_STRICT_MODE_PATH: &str = "/proc/sys/net/vrf/strict_mode";

/// Applies nmstate desired-state documents (`*.yml`/`*.yaml`) from `src`
/// (the mounted `network-config` ConfigMap) via the `nmstate` crate
/// directly - no `nmstatectl` subprocess, and no NetworkManager
/// `.nmconnection` keyfile support (nmstate-only, by design).
///
/// `cache_dir` holds a copy of the last successfully-applied content of
/// each state file (keyed by file name) - a poll tick whose content
/// matches the cached copy byte-for-byte skips `NetworkState::apply()`
/// entirely rather than calling it unconditionally on every tick. Same
/// reasoning as `frr::sync_inner`'s `daemons_changed`/`changed` gate:
/// nmstate's own diff engine computes its changes against the actual
/// live NetworkManager/kernel state (not against our cache), so a no-op
/// apply is generally a true no-op there - but "generally" isn't a
/// guarantee, and reapplying identical desired state every
/// `POLL_INTERVAL` for no reason is the same unnecessary-churn pattern
/// that made `frr-reload.py` bounce BGP sessions on an unchanged
/// ConfigMap. Skipping it entirely when nothing changed removes that
/// risk outright, at the cost of this daemon no longer self-correcting
/// out-of-band drift (e.g. a manual `nmcli`/`ip` change) on its own -
/// only an actual ConfigMap change re-asserts the desired state. That
/// trade-off is deliberate: this VM's network config is meant to be
/// managed exclusively through the ConfigMap, not hand-edited live.
pub fn sync(src: &Path, cache_dir: &Path, log: &Logger) -> Result<()> {
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

        let yaml = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        let cache_path = cache_dir.join(&name);
        if fs::read_to_string(&cache_path).is_ok_and(|cached| cached == yaml) {
            log.log(format!("{name} unchanged since last apply, skipping"));
            continue;
        }

        log.log(format!("applying nmstate state {name}"));
        let state = NetworkState::new_from_yaml(&yaml)
            .with_context(|| format!("failed to parse {name} as an nmstate desired state"))?;
        state
            .apply()
            .with_context(|| format!("failed to apply {name}"))?;
        log.log(format!("applied {name} successfully"));

        cache_applied(&cache_path, &yaml, log);
    }

    apply_vrf_strict_mode(Path::new(VRF_STRICT_MODE_PATH), log);

    log.log("sync complete");
    Ok(())
}

/// Best-effort, same reasoning as `apply_vrf_strict_mode`: the state
/// itself already applied successfully by the time this runs, so a
/// failure to cache it doesn't fail the sync - it just means the next
/// tick re-applies unnecessarily instead of skipping, which is exactly
/// today's behavior and therefore never worse than not having the cache
/// at all.
fn cache_applied(cache_path: &Path, yaml: &str, log: &Logger) {
    if let Some(parent) = cache_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            log.err(format!(
                "failed to create cache directory {}: {e}",
                parent.display()
            ));
            return;
        }
    }
    if let Err(e) = fs::write(cache_path, yaml) {
        log.err(format!(
            "failed to cache applied state at {}: {e}",
            cache_path.display()
        ));
    }
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

/// Kubernetes ConfigMap mounts present each file as a symlink through a
/// hidden `..data -> ..<timestamp>/` indirection (see
/// `frr::copy_dir_contents`'s doc comment for the full picture).
/// `DirEntry::file_type()` is `lstat`-based and doesn't follow symlinks,
/// so checking `.is_file()` on it treated every `*.yml`/`*.yaml`
/// symlink as "not a file" and silently applied nothing at all - use
/// `fs::metadata` (follows symlinks) instead, and skip hidden entries
/// outright rather than trying to resolve them as state files.
fn statefile_paths(src: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if fs::metadata(&path).is_ok_and(|m| m.is_file()) && has_extension(&path, &["yml", "yaml"])
        {
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
        let cache = tempfile::tempdir().unwrap();
        let log = Logger::new("test");
        sync(src.path(), cache.path(), &log).unwrap();
    }

    /// The regression this guards: `sync` used to call
    /// `NetworkState::apply()` unconditionally on every poll tick, even
    /// when the ConfigMap hadn't changed since the last successful
    /// apply - the same "reapply on every tick regardless of change"
    /// pattern that made `frr-reload.py` bounce BGP sessions every
    /// `POLL_INTERVAL`. This test can't run against a real
    /// NetworkManager, so it proves the skip indirectly: with the cache
    /// already holding byte-identical content, `sync` must return
    /// `Ok(())` without ever reaching `NetworkState::apply()` - if it
    /// did, this would fail (or hang) in a sandbox with no
    /// NetworkManager D-Bus service to talk to.
    #[test]
    fn cached_unchanged_state_skips_apply() {
        let src = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let yaml = "interfaces: []\n";
        fs::write(src.path().join("nmstate.yml"), yaml).unwrap();
        fs::write(cache.path().join("nmstate.yml"), yaml).unwrap();
        let log = Logger::new("test");

        sync(src.path(), cache.path(), &log).unwrap();
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

    /// Kubernetes ConfigMap mounts present `nmstate.yml` as a symlink
    /// through a hidden `..data -> ..<timestamp>/` indirection, not as
    /// a plain file directly in the mount directory - `DirEntry::
    /// file_type()` (lstat-based) doesn't follow that, so it used to
    /// see the symlink as "not a file" and applied nothing at all.
    #[test]
    fn statefile_paths_follows_configmap_style_symlinks() {
        let src = tempfile::tempdir().unwrap();
        let timestamp_dir = src.path().join("..2026_01_01_00_00_00.000000000");
        fs::create_dir(&timestamp_dir).unwrap();
        fs::write(timestamp_dir.join("nmstate.yml"), "interfaces: []\n").unwrap();
        std::os::unix::fs::symlink(
            timestamp_dir.file_name().unwrap(),
            src.path().join("..data"),
        )
        .unwrap();
        std::os::unix::fs::symlink("..data/nmstate.yml", src.path().join("nmstate.yml")).unwrap();

        let paths = statefile_paths(src.path()).unwrap();
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["nmstate.yml"]);
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
