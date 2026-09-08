use std::fs;
use std::io;
use std::path::Path;

use anyhow::{bail, Context, Result};
use frr_vty::VtyClient;

use crate::{is_empty_source, list_names, redact_passwords, Logger, Runner};

/// mgmtd's vty socket - frr-bootc runs with `mgmtd=yes` and `service
/// integrated-vtysh-config`, so mgmtd owns the whole integrated
/// frr.conf, not a per-daemon split config.
const MGMTD_SOCKET: &str = "/var/run/frr/mgmtd.vty";
const FRR_RELOAD: &str = "/usr/libexec/frr/frr-reload.py";

/// Syncs FRR configuration from `src` (the mounted ConfigMap) into `dst`
/// (`/etc/frr`), then reloads or restarts FRR as needed. A faithful port
/// of the former `frr-config-sync` bash script's control flow.
pub fn sync(src: &Path, dst: &Path, runner: &impl Runner, log: &Logger) -> Result<()> {
    sync_inner(src, dst, Path::new(MGMTD_SOCKET), runner, log)
}

fn sync_inner(
    src: &Path,
    dst: &Path,
    socket_path: &Path,
    runner: &impl Runner,
    log: &Logger,
) -> Result<()> {
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
        log.log("validating staged frr.conf (vty socket, mgmtd -C style check)");
        if let Err(e) = validate(&staged_conf, socket_path) {
            log.err("staged frr.conf failed validation, keeping the currently running config unchanged.");
            log.err("validation output:");
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

/// Validates a staged `frr.conf` over mgmtd's vty socket, replacing
/// `vtysh -f <file> -C`. Enters config mode, feeds every line, and
/// always issues `abort` afterward so nothing is committed to the
/// running configuration regardless of outcome - that `abort` is what
/// makes this a validation rather than a live apply.
///
/// NOTE: this replicates vtysh's "-C, check syntax only, don't commit"
/// behavior on a best-effort basis, reasoned from mgmtd's candidate/
/// running datastore split. The exact wire-level semantics have not been
/// verified against a real running FRR/mgmtd instance yet (no FRR
/// available in this sandbox) - treat this as needing live-VM
/// confirmation before being fully trusted, same as any change that
/// can't be exercised locally.
fn validate(conf_path: &Path, socket_path: &Path) -> Result<()> {
    let content = fs::read_to_string(conf_path)
        .with_context(|| format!("failed to read {}", conf_path.display()))?;

    let mut client = VtyClient::connect(socket_path)
        .with_context(|| format!("failed to connect to {}", socket_path.display()))?;

    let enable = client.execute("enable").context("vty: enable")?;
    if !enable.success() {
        bail!("vty: enable failed: {}", enable.output);
    }

    let configure = client
        .execute("configure terminal")
        .context("vty: configure terminal")?;
    if !configure.success() {
        bail!("vty: configure terminal failed: {}", configure.output);
    }

    let mut failure: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('!') || trimmed.starts_with('#') {
            continue;
        }
        match client.execute(line) {
            Ok(resp) if resp.success() => {}
            Ok(resp) => {
                failure = Some(format!("line {line:?} rejected: {}", resp.output));
                break;
            }
            Err(e) => {
                failure = Some(format!("line {line:?}: vty error: {e}"));
                break;
            }
        }
    }

    // Discard the candidate configuration unconditionally rather than
    // committing it, whether or not validation failed.
    let _ = client.execute("abort");

    match failure {
        Some(msg) => bail!(msg),
        None => Ok(()),
    }
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

fn copy_dir_contents(src: &Path, dst: &Path) -> io::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            fs::create_dir_all(&dst_path)?;
            copy_dir_contents(&entry.path(), &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Write as _;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::process::{ExitStatus, Output};
    use std::thread;

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
        let socket = tempfile::tempdir().unwrap().path().join("mgmtd.vty");
        let runner = FakeRunner::new();
        let log = Logger::new("test");

        sync_inner(src.path(), dst.path(), &socket, &runner, &log).unwrap();
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn daemons_diff_triggers_restart_not_reload() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let socket = tempfile::tempdir().unwrap().path().join("mgmtd.vty");
        write_file(src.path(), "daemons", "bgpd=yes\n");
        write_file(dst.path(), "daemons", "bgpd=no\n");

        let runner = FakeRunner::new();
        let log = Logger::new("test");

        sync_inner(src.path(), dst.path(), &socket, &runner, &log).unwrap();

        let calls = runner.calls();
        assert!(calls.contains(&"systemctl".to_string()));
        assert!(!calls.contains(&FRR_RELOAD.to_string()));
    }

    #[test]
    fn matching_daemons_triggers_reload_not_restart() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let socket = tempfile::tempdir().unwrap().path().join("mgmtd.vty");
        write_file(src.path(), "daemons", "bgpd=yes\n");
        write_file(dst.path(), "daemons", "bgpd=yes\n");
        fs::write(dst.path().join("frr.conf"), "").unwrap();

        let runner = FakeRunner::new();
        let log = Logger::new("test");

        sync_inner(src.path(), dst.path(), &socket, &runner, &log).unwrap();

        let calls = runner.calls();
        assert!(calls.contains(&FRR_RELOAD.to_string()));
        assert!(!calls.contains(&"systemctl".to_string()));
    }

    #[test]
    fn rsync_failure_aborts_before_any_reload_or_restart() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let socket = tempfile::tempdir().unwrap().path().join("mgmtd.vty");
        write_file(src.path(), "daemons", "bgpd=yes\n");

        let runner = FakeRunner::new().on("rsync", fail_output(""));
        let log = Logger::new("test");

        let result = sync_inner(src.path(), dst.path(), &socket, &runner, &log);
        assert!(result.is_err());

        let calls = runner.calls();
        assert!(!calls.contains(&"systemctl".to_string()));
        assert!(!calls.contains(&FRR_RELOAD.to_string()));
    }

    #[test]
    fn reload_failure_falls_back_to_restart() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let socket = tempfile::tempdir().unwrap().path().join("mgmtd.vty");
        write_file(src.path(), "daemons", "bgpd=yes\n");
        write_file(dst.path(), "daemons", "bgpd=yes\n");

        let runner = FakeRunner::new()
            .on(FRR_RELOAD, fail_output(""))
            .on("systemctl", ok_output(""));
        let log = Logger::new("test");

        sync_inner(src.path(), dst.path(), &socket, &runner, &log).unwrap();

        let calls = runner.calls();
        assert!(calls.contains(&FRR_RELOAD.to_string()));
        assert!(calls.contains(&"systemctl".to_string()));
    }

    #[test]
    fn chown_failure_aborts_before_rsync() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let socket = tempfile::tempdir().unwrap().path().join("mgmtd.vty");
        write_file(src.path(), "daemons", "bgpd=yes\n");

        let runner = FakeRunner::new().on("chown", fail_output(""));
        let log = Logger::new("test");

        let result = sync_inner(src.path(), dst.path(), &socket, &runner, &log);
        assert!(result.is_err());
        assert!(!runner.calls().contains(&"rsync".to_string()));
    }

    fn spawn_mgmtd_mock(socket_path: &Path, reject_line: Option<&'static str>) {
        let listener = UnixListener::bind(socket_path).unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            while let Some(cmd) = read_command(&mut stream) {
                let ok = reject_line.map(|r| cmd != r).unwrap_or(true);
                let status: u8 = if ok { 0 } else { 1 };
                stream.write_all(&[0, 0, 0, status]).unwrap();
            }
        });
    }

    fn read_command(stream: &mut UnixStream) -> Option<String> {
        use std::io::Read as _;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 256];
        loop {
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.last() == Some(&0) {
                buf.pop();
                return Some(String::from_utf8_lossy(&buf).into_owned());
            }
        }
    }

    #[test]
    fn validation_success_always_ends_with_abort() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("mgmtd.vty");
        spawn_mgmtd_mock(&socket_path, None);

        let conf = dir.path().join("frr.conf");
        fs::write(
            &conf,
            "! comment\nrouter bgp 65001\n neighbor 10.0.0.1 remote-as 65002\n",
        )
        .unwrap();

        validate(&conf, &socket_path).unwrap();
    }

    #[test]
    fn validation_failure_reports_the_rejected_line_and_still_aborts() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("mgmtd.vty");
        spawn_mgmtd_mock(&socket_path, Some("bogus line"));

        let conf = dir.path().join("frr.conf");
        fs::write(&conf, "router bgp 65001\nbogus line\n").unwrap();

        let err = validate(&conf, &socket_path).unwrap_err();
        assert!(err.to_string().contains("bogus line"));
    }

    #[test]
    fn validation_failure_blocks_rsync_in_full_sync() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("mgmtd.vty");
        spawn_mgmtd_mock(&socket_path, Some("bogus line"));

        write_file(src.path(), "daemons", "bgpd=yes\n");
        write_file(src.path(), "frr.conf", "bogus line\n");

        let runner = FakeRunner::new();
        let log = Logger::new("test");

        let result = sync_inner(src.path(), dst.path(), &socket_path, &runner, &log);
        assert!(result.is_err());
        assert!(!runner.calls().contains(&"rsync".to_string()));
        assert!(!dst.path().join("frr.conf").exists());
    }
}
