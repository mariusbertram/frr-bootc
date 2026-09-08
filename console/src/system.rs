//! Host-level metrics (hostname, uptime, load, memory, disk) read directly
//! from /proc and via statvfs(2) - plain numbers with no ambiguous text
//! format to parse, so there's no reason to shell out to `hostname`/
//! `free`/`df` for these the way the bash version of this dashboard did.

use std::ffi::CStr;
use std::fs;
use std::mem::MaybeUninit;

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is a valid, appropriately-sized buffer for the duration
    // of the call.
    let ret = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if ret != 0 {
        return "unknown".to_string();
    }
    // gethostname(2) guarantees NUL-termination when it succeeds and the
    // buffer was large enough, which 256 bytes always is for a hostname.
    let cstr = CStr::from_bytes_until_nul(&buf).unwrap_or(c"unknown");
    cstr.to_string_lossy().into_owned()
}

pub fn uptime() -> String {
    let raw = fs::read_to_string("/proc/uptime").unwrap_or_default();
    let secs: u64 = raw
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| f as u64)
        .unwrap_or(0);
    format!(
        "{}d {}h {}m",
        secs / 86400,
        secs % 86400 / 3600,
        secs % 3600 / 60
    )
}

pub fn load_average() -> String {
    let raw = fs::read_to_string("/proc/loadavg").unwrap_or_default();
    raw.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}

pub fn cpu_count() -> usize {
    fs::read_to_string("/proc/cpuinfo")
        .map(|s| s.lines().filter(|l| l.starts_with("processor")).count())
        .unwrap_or(0)
        .max(1)
}

pub struct MemInfo {
    pub used: u64,
    pub total: u64,
    pub available: u64,
}

pub fn memory() -> Option<MemInfo> {
    let raw = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = None;
    let mut available = None;
    for line in raw.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("MemTotal:") => total = parts.next().and_then(|v| v.parse::<u64>().ok()),
            Some("MemAvailable:") => available = parts.next().and_then(|v| v.parse::<u64>().ok()),
            _ => {}
        }
    }
    // /proc/meminfo values are in KiB.
    let total = total? * 1024;
    let available = available? * 1024;
    Some(MemInfo {
        used: total.saturating_sub(available),
        total,
        available,
    })
}

pub struct DiskInfo {
    pub used: u64,
    pub total: u64,
    pub percent_used: u8,
}

pub fn disk_usage(path: &str) -> Option<DiskInfo> {
    let c_path = std::ffi::CString::new(path).ok()?;
    let mut stat = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: c_path is a valid, NUL-terminated string for the duration of
    // the call, and stat is a valid out-pointer of the right size/type.
    let ret = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if ret != 0 {
        return None;
    }
    // SAFETY: statvfs(2) returned success, so the struct is fully
    // initialized.
    let stat = unsafe { stat.assume_init() };
    // libc::statvfs's block-count/size fields are u64 on this target but
    // narrower on some others (e.g. 32-bit); the cast is a genuine
    // widening conversion there, so it stays rather than being narrowed
    // to whatever happens to already match here.
    #[allow(clippy::unnecessary_cast)]
    let frsize = stat.f_frsize as u64;
    #[allow(clippy::unnecessary_cast)]
    let total = stat.f_blocks as u64 * frsize;
    #[allow(clippy::unnecessary_cast)]
    let free = stat.f_bfree as u64 * frsize;
    #[allow(clippy::unnecessary_cast)]
    let avail = stat.f_bavail as u64 * frsize;
    let used = total.saturating_sub(free);
    let percent_used = if total == 0 {
        0
    } else {
        (used * 100 / (used + avail).max(1)) as u8
    };
    Some(DiskInfo {
        used,
        total,
        percent_used,
    })
}

/// Human-readable binary size (Ki/Mi/Gi), matching `free -h`'s units.
pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["", "Ki", "Mi", "Gi", "Ti"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

pub fn now_local() -> String {
    std::process::Command::new("date")
        .arg("+%Y-%m-%d %H:%M:%S %Z")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_bytes_picks_the_right_unit() {
        assert_eq!(fmt_bytes(0), "0B");
        assert_eq!(fmt_bytes(512), "512B");
        assert_eq!(fmt_bytes(1536), "1.5Ki");
        assert_eq!(fmt_bytes(15 * 1024 * 1024), "15.0Mi");
        assert_eq!(fmt_bytes(2 * 1024 * 1024 * 1024), "2.0Gi");
    }
}
