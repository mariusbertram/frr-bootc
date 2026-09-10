use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

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
///
/// That said, `apply()` returning `Ok(())` only means `nmstate`'s own
/// (short, ~5s) verify-and-retry loop was satisfied - not that every
/// interface actually ended up admin+oper up on the live kernel.
/// Confirmed live: the very first apply after a boot/redeploy can race
/// `NetworkManager` itself still starting up (`NetworkManager-wait-
/// online.service` timing out is a symptom of the same race, not its
/// cause - nothing brings up a connection early enough for it to succeed
/// in this deliberately-deferred-to-network-config-sync design) and
/// report success while a physical interface stays down. Caching that as
/// "successfully applied" would mean nothing ever retries it, since the
/// ConfigMap content itself never changes - so `interfaces_converged`
/// checks each desired-up interface's actual kernel operstate before
/// caching, and skips the cache write (not the whole sync, which still
/// reports success - `nmstate` itself is not wrong that it applied
/// everything it could) when convergence hasn't actually happened yet.
/// The next tick then retries exactly as if nothing had been cached at
/// all, giving this the self-healing the trade-off above gives up in the
/// steady state, without paying its cost once things are actually
/// converged.
const SYSFS_NET_DIR: &str = "/sys/class/net";

/// `interfaces_converged`'s check, right after `apply()` returns, can
/// itself race NetworkManager still bringing an interface up on the
/// kernel side - confirmed live: every desired-up interface, including
/// the physical NIC itself, reporting `operstate: down` a mere ~2s after
/// "applied successfully". `apply()`'s own verify-and-retry loop only
/// confirms NetworkManager's *own* bookkeeping, not the kernel's; without
/// retrying the operstate check itself, that race meant deferring a full
/// `POLL_INTERVAL` (2 minutes) to the next tick for what's actually just
/// NetworkManager catching up within a couple of seconds. 15 attempts
/// 1s apart covers a good multiple of that observed race without coming
/// anywhere near `POLL_INTERVAL`'s cost when it's genuinely not
/// converging (e.g. a structural mismatch `nmstate` can't resolve) -
/// that case still correctly falls through to "not caching, retry next
/// tick".
const CONVERGENCE_POLL_ATTEMPTS: u32 = 15;
const CONVERGENCE_POLL_DELAY: Duration = Duration::from_secs(1);

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

        if wait_for_convergence(
            &state,
            Path::new(SYSFS_NET_DIR),
            log,
            CONVERGENCE_POLL_ATTEMPTS,
            CONVERGENCE_POLL_DELAY,
        ) {
            cache_applied(&cache_path, &yaml, log);
        } else {
            log.log(format!(
                "{name} applied but not every interface is up yet after {CONVERGENCE_POLL_ATTEMPTS} attempts - not caching, will retry next tick"
            ));
        }
    }

    log.log("sync complete");
    Ok(())
}

/// Every interface `state` wants up that is *not* actually up on the
/// live kernel right now (`/sys/class/net/<name>/operstate` == "up"), not
/// just according to `nmstate`/NetworkManager's own bookkeeping - see
/// `sync`'s doc comment for why this matters. Empty means fully
/// converged. An interface `nmstate` hasn't been asked to bring up isn't
/// checked at all, so a document that only ever touches a subset of
/// interfaces (e.g. adding one tenant VLAN) doesn't need every *other*
/// interface to happen to be up too. Pure (no logging, no sleeping) so
/// `wait_for_convergence` can poll it repeatedly without spamming a log
/// line per interface per attempt.
fn not_up_interfaces(state: &NetworkState, sysfs_net_dir: &Path) -> Vec<String> {
    let mut not_up = Vec::new();
    for iface in state.interfaces.iter() {
        if !iface.is_up() {
            continue;
        }
        let path = sysfs_net_dir.join(iface.name()).join("operstate");
        match fs::read_to_string(&path) {
            Ok(operstate) if operstate.trim() == "up" => {}
            Ok(operstate) => not_up.push(format!(
                "{} not up yet (operstate: {})",
                iface.name(),
                operstate.trim()
            )),
            Err(e) => not_up.push(format!(
                "{} operstate unreadable ({}): {e}",
                iface.name(),
                path.display()
            )),
        }
    }
    not_up
}

/// Polls `not_up_interfaces` up to `attempts` times, `delay` apart,
/// giving NetworkManager a chance to actually finish bringing an
/// interface up on the kernel side before this tick gives up on it - see
/// `CONVERGENCE_POLL_ATTEMPTS`'s doc comment for why a single check right
/// after `apply()` isn't enough. Only logs on the final failing attempt,
/// not every one, to avoid a log line per interface per attempt.
fn wait_for_convergence(
    state: &NetworkState,
    sysfs_net_dir: &Path,
    log: &Logger,
    attempts: u32,
    delay: Duration,
) -> bool {
    for attempt in 1..=attempts {
        let not_up = not_up_interfaces(state, sysfs_net_dir);
        if not_up.is_empty() {
            return true;
        }
        if attempt == attempts {
            for reason in &not_up {
                log.log(reason.clone());
            }
        } else {
            thread::sleep(delay);
        }
    }
    false
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

    fn up_interface_state(name: &str) -> NetworkState {
        NetworkState::new_from_yaml(&format!(
            "interfaces:\n  - name: {name}\n    type: ethernet\n    state: up\n"
        ))
        .unwrap()
    }

    #[test]
    fn not_up_interfaces_empty_when_kernel_reports_up() {
        let state = up_interface_state("foo");
        let sysfs = tempfile::tempdir().unwrap();
        fs::create_dir_all(sysfs.path().join("foo")).unwrap();
        fs::write(sysfs.path().join("foo/operstate"), "up\n").unwrap();

        assert!(not_up_interfaces(&state, sysfs.path()).is_empty());
    }

    /// The regression this guards: the very first `apply()` after a boot
    /// can report success (nmstate's own short verify-and-retry loop is
    /// satisfied) while a physical interface hasn't actually come up yet
    /// on the kernel side - if that gets cached as "successfully
    /// applied" anyway, nothing ever retries it again, since the
    /// ConfigMap content itself never changes.
    #[test]
    fn not_up_interfaces_lists_interface_when_kernel_reports_down() {
        let state = up_interface_state("foo");
        let sysfs = tempfile::tempdir().unwrap();
        fs::create_dir_all(sysfs.path().join("foo")).unwrap();
        fs::write(sysfs.path().join("foo/operstate"), "down\n").unwrap();

        assert_eq!(not_up_interfaces(&state, sysfs.path()).len(), 1);
    }

    #[test]
    fn not_up_interfaces_lists_interface_when_operstate_missing() {
        let state = up_interface_state("foo");
        let sysfs = tempfile::tempdir().unwrap();

        assert_eq!(not_up_interfaces(&state, sysfs.path()).len(), 1);
    }

    #[test]
    fn not_up_interfaces_ignores_interfaces_not_desired_up() {
        let state = NetworkState::new_from_yaml(
            "interfaces:\n  - name: foo\n    type: ethernet\n    state: down\n",
        )
        .unwrap();
        let sysfs = tempfile::tempdir().unwrap();

        // Nothing in `sysfs` at all - would fail if this interface were
        // checked, so this only passes because `not_up_interfaces`
        // correctly skips interfaces the desired state didn't ask to be up.
        assert!(not_up_interfaces(&state, sysfs.path()).is_empty());
    }

    /// The regression this guards: the very first `apply()` after a
    /// boot/redeploy raced NetworkManager still bringing the interface up
    /// on the kernel side - confirmed live, every interface reporting
    /// `operstate: down` a mere ~2s after "applied successfully" - and a
    /// single immediate check (the old `interfaces_converged` call site)
    /// gave up on that instantly, deferring a full `POLL_INTERVAL` (2
    /// minutes) for what's actually just NetworkManager catching up
    /// within a couple of seconds. `wait_for_convergence` must retry
    /// instead of failing on the first attempt.
    #[test]
    fn wait_for_convergence_retries_until_kernel_reports_up() {
        let state = up_interface_state("foo");
        let sysfs = tempfile::tempdir().unwrap();
        let iface_dir = sysfs.path().join("foo");
        fs::create_dir_all(&iface_dir).unwrap();
        fs::write(iface_dir.join("operstate"), "down\n").unwrap();
        let log = Logger::new("test");

        let operstate_path = iface_dir.join("operstate");
        let updater = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            fs::write(operstate_path, "up\n").unwrap();
        });

        let converged =
            wait_for_convergence(&state, sysfs.path(), &log, 50, Duration::from_millis(5));
        updater.join().unwrap();

        assert!(converged);
    }

    /// A structural mismatch `nmstate` can never resolve (as opposed to
    /// the transient race above) must still fall through to "not
    /// converged" once the retry budget is spent, not retry forever
    /// within a single tick.
    #[test]
    fn wait_for_convergence_gives_up_after_max_attempts() {
        let state = up_interface_state("foo");
        let sysfs = tempfile::tempdir().unwrap();
        fs::create_dir_all(sysfs.path().join("foo")).unwrap();
        fs::write(sysfs.path().join("foo/operstate"), "down\n").unwrap();
        let log = Logger::new("test");

        let converged =
            wait_for_convergence(&state, sysfs.path(), &log, 3, Duration::from_millis(1));

        assert!(!converged);
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
