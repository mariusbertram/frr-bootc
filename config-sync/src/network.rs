use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nmstate::NetworkState;

use crate::{has_extension, is_empty_source, list_names, Logger};

/// Applies nmstate desired-state documents (`*.yml`/`*.yaml`) from `src`
/// (the mounted `network-config` ConfigMap) via the `nmstate` crate
/// directly - no `nmstatectl` subprocess, and no NetworkManager
/// `.nmconnection` keyfile support (nmstate-only, by design).
///
/// `cache_dir` holds a copy of the last successfully-applied content of
/// each state file (keyed by file name) - a poll tick whose content
/// matches the cached copy byte-for-byte skips `NetworkState::apply()`
/// entirely rather than calling it unconditionally on every tick. This
/// was tried once, reverted (assuming `nmstate`'s own diff against live
/// state made unconditional reapply harmless), and reinstated after that
/// assumption turned out wrong: reading `nmstate`'s own source
/// (`query_apply/net_state.rs`), `apply()` retrieves current state and
/// merges it against desired on *every* call, with no caching of its
/// own - so any interface whose desired state never actually converges
/// (e.g. a rename that can't take effect against a device with
/// dependents already enslaved to it) is a *permanent* mismatch from
/// `nmstate`'s point of view, reapplied/reactivated via NetworkManager
/// on every single tick forever, regardless of whether the ConfigMap
/// changed. Reactivating a connection every `POLL_INTERVAL` for no
/// reason risks exactly the kind of transient disruption that made
/// `frr-reload.py` bounce BGP sessions on the FRR side - here it can
/// bounce a connected route long enough for BGP nexthop tracking to
/// invalidate paths depending on it. Skipping `apply()` entirely when
/// the ConfigMap hasn't changed removes that risk outright, at the cost
/// of this daemon no longer self-correcting out-of-band drift (e.g. a
/// manual `nmcli`/`ip` change, or `nmstate` itself never achieving
/// convergence in the first place) on its own - only an actual
/// ConfigMap change re-asserts the desired state. That trade-off is
/// deliberate: reapplying unconditionally doesn't fix a `nmstate`-side
/// convergence failure either when it's a structural mismatch rather
/// than a transient race `nmstate`'s own internal retry-and-verify loop
/// would eventually win - retrying forever pays the disruption cost for
/// a fix that never lands.
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

    log.log("sync complete");
    Ok(())
}

/// Best-effort: the state itself already applied successfully by the
/// time this runs, so a failure to cache it doesn't fail the sync - it
/// just means the next tick re-applies unnecessarily instead of
/// skipping, which is exactly today's behavior and therefore never
/// worse than not having the cache at all.
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
}
