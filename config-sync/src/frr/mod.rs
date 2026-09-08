use std::fs;
use std::io;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::{is_empty_source, list_names, redact_passwords, Logger, Runner};

const FRR_RELOAD: &str = "/usr/libexec/frr/frr-reload.py";

/// Syncs FRR configuration from `src` (the mounted ConfigMap) into `dst`
/// (`/etc/frr`), then reloads or restarts FRR as needed. A faithful port
/// of the former `frr-config-sync` bash script's control flow.
pub fn sync(src: &Path, dst: &Path, runner: &impl Runner, log: &Logger) -> Result<()> {
    sync_inner(src, dst, runner, log)
}

fn sync_inner(src: &Path, dst: &Path, runner: &impl Runner, log: &Logger) -> Result<()> {
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

    // The set of enabled daemons only takes effect on (re)start of
    // frr.service - a plain reload cannot add/remove daemon processes.
    let daemons_changed = !files_equal(&src.join("daemons"), &dst.join("daemons"));
    if daemons_changed {
        log.log("daemons file differs from the currently running one - will restart frr.service, not reload");
    }

    let stage_parent = dst.parent().unwrap_or_else(|| Path::new("/etc"));
    let stage = tempfile::Builder::new()
        .prefix("frr.staged.")
        .tempdir_in(stage_parent)
        .context("failed to create staging directory")?;

    copy_dir_contents(src, stage.path()).context("failed to stage source config")?;

    let staged_conf = stage.path().join("frr.conf");
    if staged_conf.is_file() {
        log.log("validating staged frr.conf (vtysh -f <staged> -C)");
        if let Err(e) = validate(&staged_conf, runner) {
            log.err("staged frr.conf failed validation, keeping the currently running config unchanged.");
            log.err("full 'vtysh -C' output:");
            log.err_block(&redact_passwords(&e.to_string()));
            bail!("frr.conf validation failed");
        }
        log.log("validation passed");
    }

    chown_and_chmod(stage.path(), runner)?;

    log.log(format!("rsyncing staged config into {}", dst.display()));
    let stage_src = format!("{}/", stage.path().display());
    let dst_arg = format!("{}/", dst.display());
    let rsync_out = runner
        .run("rsync", &["-avi", "--delete", &stage_src, &dst_arg])
        .context("failed to spawn rsync")?;
    if !rsync_out.status.success() {
        log.err(format!("rsync into {} failed:", dst.display()));
        log.err_block(&String::from_utf8_lossy(&rsync_out.stderr));
        bail!("rsync failed");
    }
    let rsync_stdout = String::from_utf8_lossy(&rsync_out.stdout).into_owned();
    let changed: Vec<&str> = rsync_stdout
        .lines()
        .filter(|l| l.chars().next().is_some_and(|c| "<>ch*".contains(c)))
        .collect();
    log.log(format!("rsync complete, {} file(s) changed", changed.len()));
    if !changed.is_empty() {
        log.block(&changed.join("\n"));
    }

    if daemons_changed {
        restart_frr(runner, log, false)?;
    } else {
        reload_frr(dst, runner, log)?;
    }

    log.log("sync complete");
    Ok(())
}

/// Validates a staged `frr.conf` via `vtysh -f <file> -C` (dry-run: check
/// syntax only, apply nothing).
///
/// An earlier version of this function validated directly over a vty
/// Unix socket instead (`configure terminal`, feed every line, always
/// `abort` at the end) to avoid the `vtysh` subprocess entirely. That
/// was wrong and has been reverted: FRR's real candidate/commit/abort
/// transactional datastore is exposed over mgmtd's separate protobuf
/// "Frontend Interface" socket (`mgmtd_fe.sock`), not over a daemon's
/// plain-text `.vty` socket - `mgmtd.vty` is the same kind of legacy,
/// immediate-apply line CLI every other daemon exposes. Concretely, that
/// meant: `configure terminal` over the vty socket applied each command
/// *live* as it was typed (not into any undoable candidate), `abort` is
/// not a real vty CLI command there (so it likely did nothing but
/// silently fail), and a rejected line partway through left every
/// command before it already applied to the running daemons - the
/// opposite of validation. `vtysh -C` is FRR's own, actually offline,
/// actually side-effect-free syntax checker.
fn validate(conf_path: &Path, runner: &impl Runner) -> Result<()> {
    let conf_str = conf_path.to_string_lossy().into_owned();
    let out = runner
        .run("vtysh", &["-f", &conf_str, "-C"])
        .context("failed to spawn vtysh")?;
    if out.status.success() {
        return Ok(());
    }
    let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
    output.push_str(&String::from_utf8_lossy(&out.stderr));
    bail!(output)
}

fn chown_and_chmod(path: &Path, runner: &impl Runner) -> Result<()> {
    let path_str = path.to_string_lossy().into_owned();

    let out = runner
        .run("chown", &["-R", "frr:frr", &path_str])
        .context("failed to spawn chown")?;
    if !out.status.success() {
        bail!(
            "chown -R frr:frr {path_str} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let out = runner
        .run("chmod", &["-R", "u=rwX,g=rX,o=", &path_str])
        .context("failed to spawn chmod")?;
    if !out.status.success() {
        bail!(
            "chmod -R u=rwX,g=rX,o= {path_str} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    Ok(())
}

fn restart_frr(runner: &impl Runner, log: &Logger, fallback: bool) -> Result<()> {
    log.log(if fallback {
        "restarting frr.service (fallback)"
    } else {
        "restarting frr.service"
    });
    let out = runner
        .run("systemctl", &["restart", "frr.service"])
        .context("failed to spawn systemctl restart frr.service")?;
    if out.status.success() {
        log.log(if fallback {
            "frr.service restarted successfully (fallback)"
        } else {
            "frr.service restarted successfully"
        });
        Ok(())
    } else {
        log.err(if fallback {
            "fallback systemctl restart frr.service ALSO failed. Recent frr.service log:"
        } else {
            "systemctl restart frr.service failed. Recent frr.service log:"
        });
        if let Ok(journal) = runner.run(
            "journalctl",
            &["-u", "frr.service", "-n", "50", "--no-pager"],
        ) {
            log.err_block(&String::from_utf8_lossy(&journal.stdout));
        }
        bail!("systemctl restart frr.service failed");
    }
}

fn reload_frr(dst: &Path, runner: &impl Runner, log: &Logger) -> Result<()> {
    log.log("reloading frr configuration (frr-reload.py --reload)");
    let conf = dst.join("frr.conf");
    let conf_str = conf.to_string_lossy().into_owned();
    let out = runner
        .run(FRR_RELOAD, &["--reload", "--stdout", &conf_str])
        .context("failed to spawn frr-reload.py")?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();

    if out.status.success() {
        log.log("frr-reload.py succeeded");
        if !stdout.trim().is_empty() {
            log.block(&redact_passwords(&stdout));
        }
        Ok(())
    } else {
        log.err("frr-reload.py failed, full output:");
        log.err_block(&redact_passwords(&stdout));
        log.log("falling back to systemctl restart frr.service");
        restart_frr(runner, log, true)
    }
}

fn files_equal(a: &Path, b: &Path) -> bool {
    matches!((fs::read(a), fs::read(b)), (Ok(a), Ok(b)) if a == b)
}

/// Kubernetes ConfigMap/Secret mounts use a hidden `..data ->
/// ..<timestamp>/` symlink indirection for atomic updates (the same
/// mechanism referenced in the Containerfile's top comment on why
/// inotify doesn't work here) - every visible entry (`frr.conf`,
/// `daemons`, ...) is itself a symlink through `..data`, not a regular
/// file. `DirEntry::file_type()` is `lstat`-based and does not follow
/// symlinks, so checking `.is_file()`/`.is_dir()` on it treats every one
/// of those entries as neither, silently skipping all of them - which
/// used to mean staging a directory with nothing in it at all, and then
/// `rsync --delete`ing that empty staging directory over the real
/// `/etc/frr`. `fs::metadata` (unlike `symlink_metadata`/`file_type()`)
/// follows symlinks, so it resolves each convenience symlink to what it
/// actually points at. Hidden entries (`..data`, the timestamped
/// directory itself) are skipped outright rather than resolved, so only
/// the convenience symlinks get copied - resolved to their real content
/// - not the Kubernetes-internal machinery backing them.
fn copy_dir_contents(src: &Path, dst: &Path) -> io::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let meta = fs::metadata(&path)?;
        let dst_path = dst.join(&name);
        if meta.is_dir() {
            fs::create_dir_all(&dst_path)?;
            copy_dir_contents(&path, &dst_path)?;
        } else if meta.is_file() {
            fs::copy(&path, &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::process::{ExitStatus, Output};

    struct FakeRunner {
        calls: RefCell<Vec<(String, Vec<String>)>>,
        responses: RefCell<Vec<(String, io::Result<Output>)>>,
    }

    impl FakeRunner {
        fn new() -> Self {
            FakeRunner {
                calls: RefCell::new(Vec::new()),
                responses: RefCell::new(Vec::new()),
            }
        }

        fn on(self, cmd: &str, output: Output) -> Self {
            self.responses
                .borrow_mut()
                .push((cmd.to_string(), Ok(output)));
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(|(cmd, _)| cmd.clone())
                .collect()
        }
    }

    fn ok_output(stdout: &str) -> Output {
        success_output(stdout, true)
    }

    fn fail_output(stdout: &str) -> Output {
        success_output(stdout, false)
    }

    fn success_output(stdout: &str, ok: bool) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: ExitStatus::from_raw(if ok { 0 } else { 1 << 8 }),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, cmd: &str, args: &[&str]) -> io::Result<Output> {
            self.calls.borrow_mut().push((
                cmd.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
            let mut responses = self.responses.borrow_mut();
            if let Some(pos) = responses.iter().position(|(c, _)| c == cmd) {
                let (_, result) = responses.remove(pos);
                return result;
            }
            Ok(ok_output(""))
        }
    }

    fn write_file(dir: &Path, name: &str, contents: &str) {
        fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn empty_source_is_a_noop() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new();
        let log = Logger::new("test");

        sync_inner(src.path(), dst.path(), &runner, &log).unwrap();
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn daemons_diff_triggers_restart_not_reload() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "daemons", "bgpd=yes\n");
        write_file(dst.path(), "daemons", "bgpd=no\n");

        let runner = FakeRunner::new();
        let log = Logger::new("test");

        sync_inner(src.path(), dst.path(), &runner, &log).unwrap();

        let calls = runner.calls();
        assert!(calls.contains(&"systemctl".to_string()));
        assert!(!calls.contains(&FRR_RELOAD.to_string()));
    }

    #[test]
    fn matching_daemons_triggers_reload_not_restart() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "daemons", "bgpd=yes\n");
        write_file(dst.path(), "daemons", "bgpd=yes\n");
        fs::write(dst.path().join("frr.conf"), "").unwrap();

        let runner = FakeRunner::new();
        let log = Logger::new("test");

        sync_inner(src.path(), dst.path(), &runner, &log).unwrap();

        let calls = runner.calls();
        assert!(calls.contains(&FRR_RELOAD.to_string()));
        assert!(!calls.contains(&"systemctl".to_string()));
    }

    #[test]
    fn rsync_failure_aborts_before_any_reload_or_restart() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "daemons", "bgpd=yes\n");

        let runner = FakeRunner::new().on("rsync", fail_output(""));
        let log = Logger::new("test");

        let result = sync_inner(src.path(), dst.path(), &runner, &log);
        assert!(result.is_err());

        let calls = runner.calls();
        assert!(!calls.contains(&"systemctl".to_string()));
        assert!(!calls.contains(&FRR_RELOAD.to_string()));
    }

    #[test]
    fn reload_failure_falls_back_to_restart() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "daemons", "bgpd=yes\n");
        write_file(dst.path(), "daemons", "bgpd=yes\n");

        let runner = FakeRunner::new()
            .on(FRR_RELOAD, fail_output(""))
            .on("systemctl", ok_output(""));
        let log = Logger::new("test");

        sync_inner(src.path(), dst.path(), &runner, &log).unwrap();

        let calls = runner.calls();
        assert!(calls.contains(&FRR_RELOAD.to_string()));
        assert!(calls.contains(&"systemctl".to_string()));
    }

    #[test]
    fn chown_failure_aborts_before_rsync() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "daemons", "bgpd=yes\n");

        let runner = FakeRunner::new().on("chown", fail_output(""));
        let log = Logger::new("test");

        let result = sync_inner(src.path(), dst.path(), &runner, &log);
        assert!(result.is_err());
        assert!(!runner.calls().contains(&"rsync".to_string()));
    }

    #[test]
    fn validation_calls_vtysh_dry_run_on_the_staged_file() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("frr.conf");
        fs::write(
            &conf,
            "router bgp 65001\n neighbor 10.0.0.1 remote-as 65002\n",
        )
        .unwrap();
        let runner = FakeRunner::new().on("vtysh", ok_output(""));

        validate(&conf, &runner).unwrap();

        let calls = runner.calls.borrow();
        let (_, args) = calls.iter().find(|(cmd, _)| cmd == "vtysh").unwrap();
        assert_eq!(
            args,
            &vec![
                "-f".to_string(),
                conf.to_string_lossy().into_owned(),
                "-C".to_string()
            ]
        );
    }

    #[test]
    fn validation_failure_reports_vtysh_output() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("frr.conf");
        fs::write(&conf, "bogus line\n").unwrap();
        let runner = FakeRunner::new().on("vtysh", fail_output("% Unknown command: bogus line\n"));

        let err = validate(&conf, &runner).unwrap_err();
        assert!(err.to_string().contains("Unknown command"));
    }

    #[test]
    fn validation_failure_blocks_rsync_in_full_sync() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_file(src.path(), "daemons", "bgpd=yes\n");
        write_file(src.path(), "frr.conf", "bogus line\n");

        let runner = FakeRunner::new().on("vtysh", fail_output("% Unknown command\n"));
        let log = Logger::new("test");

        let result = sync_inner(src.path(), dst.path(), &runner, &log);
        assert!(result.is_err());
        assert!(!runner.calls().contains(&"rsync".to_string()));
        assert!(!dst.path().join("frr.conf").exists());
    }

    /// Kubernetes ConfigMap mounts don't put plain files directly in the
    /// mount directory - each visible name is a symlink through a
    /// hidden `..data -> ..<timestamp>/` indirection, e.g.:
    ///   frr.conf -> ..data/frr.conf
    ///   ..data -> ..2026_01_01_00_00_00.000000000
    ///   ..2026_01_01_00_00_00.000000000/frr.conf   (the real file)
    /// `copy_dir_contents` used to check `DirEntry::file_type()`
    /// (lstat-based, doesn't follow symlinks), so it saw every one of
    /// these entries as neither a file nor a directory and silently
    /// staged nothing at all.
    fn write_configmap_style(dir: &Path, files: &[(&str, &str)]) {
        let timestamp_dir = dir.join("..2026_01_01_00_00_00.000000000");
        fs::create_dir(&timestamp_dir).unwrap();
        for (name, content) in files {
            fs::write(timestamp_dir.join(name), content).unwrap();
        }
        std::os::unix::fs::symlink(timestamp_dir.file_name().unwrap(), dir.join("..data")).unwrap();
        for (name, _) in files {
            std::os::unix::fs::symlink(format!("..data/{name}"), dir.join(name)).unwrap();
        }
    }

    #[test]
    fn copy_dir_contents_follows_configmap_style_symlinks() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_configmap_style(
            src.path(),
            &[
                ("frr.conf", "router bgp 65001\n"),
                ("daemons", "bgpd=yes\n"),
            ],
        );

        copy_dir_contents(src.path(), dst.path()).unwrap();

        assert_eq!(
            fs::read_to_string(dst.path().join("frr.conf")).unwrap(),
            "router bgp 65001\n"
        );
        assert_eq!(
            fs::read_to_string(dst.path().join("daemons")).unwrap(),
            "bgpd=yes\n"
        );
        // The hidden Kubernetes-internal indirection itself is not
        // duplicated into the staging directory - only the resolved
        // convenience symlinks are.
        assert!(!dst.path().join("..data").exists());
    }

    #[test]
    fn full_sync_validates_configmap_style_staged_frr_conf() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        write_configmap_style(
            src.path(),
            &[
                ("frr.conf", "router bgp 65001\n"),
                ("daemons", "bgpd=yes\n"),
            ],
        );
        write_file(dst.path(), "daemons", "bgpd=yes\n");

        let runner = FakeRunner::new();
        let log = Logger::new("test");

        // Before the copy_dir_contents fix, a ConfigMap-style symlinked
        // frr.conf was silently skipped while staging: `frr.conf` never
        // looked like a file to the old lstat-based check, so
        // `staged_conf.is_file()` was false and validation never ran at
        // all - vtysh would never have been invoked here, and rsync
        // --delete would have run for real against an empty staging
        // directory, wiping out the previously-synced /etc/frr.
        sync_inner(src.path(), dst.path(), &runner, &log).unwrap();
        assert!(runner.calls().contains(&"vtysh".to_string()));
    }
}
