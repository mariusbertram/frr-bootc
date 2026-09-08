pub mod frr;
pub mod network;

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::{Command, Output};

/// Abstracts subprocess execution so the parts of config-sync that still
/// shell out (rsync, chown/chmod, systemctl, frr-reload.py, journalctl)
/// can be exercised in tests against a fake instead of the real tools.
pub trait Runner {
    fn run(&self, cmd: &str, args: &[&str]) -> io::Result<Output>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, cmd: &str, args: &[&str]) -> io::Result<Output> {
        Command::new(cmd).args(args).output()
    }
}

/// A non-blocking exclusive flock, mirroring the bash scripts'
/// `exec {FD}>lock; flock -n "$LOCKFD"`. The held file is released (and
/// the flock dropped with it) when this value is dropped.
pub struct Lock(#[allow(dead_code)] fs::File);

impl Lock {
    /// Returns `Ok(None)` if another run already holds the lock - the
    /// caller should log "another run is already in progress, skipping"
    /// and exit 0, exactly like the bash scripts did.
    pub fn try_acquire(path: &Path) -> io::Result<Option<Lock>> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret == 0 {
            Ok(Some(Lock(file)))
        } else {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(err)
            }
        }
    }
}

/// A `log()`/`err()` pair with a fixed prefix, matching each bash
/// script's own `log()`/`err()` helpers so journalctl output is
/// unchanged in shape.
pub struct Logger(&'static str);

impl Logger {
    pub const fn new(prefix: &'static str) -> Self {
        Logger(prefix)
    }

    pub fn log(&self, msg: impl AsRef<str>) {
        println!("{}: {}", self.0, msg.as_ref());
    }

    pub fn err(&self, msg: impl AsRef<str>) {
        eprintln!("{}: ERROR: {}", self.0, msg.as_ref());
    }

    /// Echoes possibly-multi-line command/tool output, indented under
    /// the prefix - matches `... | sed 's/^/prefix:   /'` in the bash
    /// scripts, used for output that is being relayed rather than being
    /// this program's own log message.
    pub fn block(&self, text: &str) {
        for line in text.lines() {
            println!("{}:   {line}", self.0);
        }
    }

    pub fn err_block(&self, text: &str) {
        for line in text.lines() {
            eprintln!("{}:   {line}", self.0);
        }
    }
}

/// True if `path` doesn't exist, isn't a directory, or has no entries -
/// the "nothing to sync" check both bash scripts start with.
pub(crate) fn is_empty_source(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => true,
    }
}

/// Sorted file names directly inside `path`, for the "files: a b c"
/// startup log line both scripts print.
pub(crate) fn list_names(path: &Path) -> io::Result<Vec<String>> {
    let mut names: Vec<String> = fs::read_dir(path)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(names)
}

pub(crate) fn has_extension(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|ext| exts.contains(&ext))
        .unwrap_or(false)
}

/// Redacts the secret from `neighbor <peer> password <secret>` lines
/// (BGP MD5 auth), mirroring the `redact_passwords` sed filter the bash
/// scripts used before this rewrite. Both vty validation output and
/// frr-reload.py's own output can echo config lines back, including a
/// peer's cleartext password - every place either gets logged goes
/// through this first.
pub fn redact_passwords(text: &str) -> String {
    text.lines()
        .map(|line| match redact_prefix_end(line) {
            Some(end) => format!("{}<REDACTED>", &line[..end]),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Byte offset right after "neighbor <peer> password " if `line` has
/// that shape (whitespace-flexible, like the sed regex it replaces), so
/// the caller keeps everything up to that point and replaces the rest.
fn redact_prefix_end(line: &str) -> Option<usize> {
    let mut idx = skip_ws(line);
    idx += expect_word(&line[idx..], "neighbor")?;
    idx += require_ws(&line[idx..])?;
    let peer_len = skip_non_ws(&line[idx..]);
    if peer_len == 0 {
        return None;
    }
    idx += peer_len;
    idx += require_ws(&line[idx..])?;
    idx += expect_word(&line[idx..], "password")?;
    idx += require_ws(&line[idx..])?;
    Some(idx)
}

fn skip_ws(s: &str) -> usize {
    s.len() - s.trim_start_matches([' ', '\t']).len()
}

fn require_ws(s: &str) -> Option<usize> {
    let n = skip_ws(s);
    (n > 0).then_some(n)
}

fn skip_non_ws(s: &str) -> usize {
    s.len() - s.trim_start_matches(|c: char| c != ' ' && c != '\t').len()
}

fn expect_word(s: &str, word: &str) -> Option<usize> {
    s.starts_with(word).then_some(word.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_password_line() {
        assert_eq!(
            redact_passwords(" neighbor 10.0.0.1 password s3cr3t"),
            " neighbor 10.0.0.1 password <REDACTED>"
        );
    }

    #[test]
    fn redacts_only_the_password_and_anything_after_it() {
        assert_eq!(
            redact_passwords("neighbor 10.0.0.1 password s3cr3t extra stuff"),
            "neighbor 10.0.0.1 password <REDACTED>"
        );
    }

    #[test]
    fn leaves_unrelated_lines_untouched() {
        let line = " neighbor 10.0.0.1 remote-as 65001";
        assert_eq!(redact_passwords(line), line);
    }

    #[test]
    fn leaves_lines_missing_password_keyword_untouched() {
        let line = "neighbor 10.0.0.1 description no-password-here";
        assert_eq!(redact_passwords(line), line);
    }

    #[test]
    fn redacts_multiple_lines_independently() {
        let input = "router bgp 65001\n neighbor 10.0.0.1 password s3cr3t\n neighbor 10.0.0.2 remote-as 65002";
        let expected = "router bgp 65001\n neighbor 10.0.0.1 password <REDACTED>\n neighbor 10.0.0.2 remote-as 65002";
        assert_eq!(redact_passwords(input), expected);
    }

    #[test]
    fn lock_blocks_a_second_acquire_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");

        let first = Lock::try_acquire(&path).unwrap();
        assert!(first.is_some());
        assert!(Lock::try_acquire(&path).unwrap().is_none());

        drop(first);
        assert!(Lock::try_acquire(&path).unwrap().is_some());
    }
}
