use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nmstate::NetworkState;

use crate::{has_extension, is_empty_source, list_names, Logger};

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

    log.log("sync complete");
    Ok(())
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
