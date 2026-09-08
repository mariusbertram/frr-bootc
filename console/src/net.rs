//! Network interface listing and throughput sampling.
//!
//! Interface state/addresses come from `ip -brief addr show` - parsing its
//! text output is more code than talking rtnetlink directly, but far less
//! than reimplementing address/prefix decoding by hand, and it's the exact
//! command an operator would run to check the same thing, so its output
//! shape is a stable, well-known contract rather than an implementation
//! detail we're gambling on.
//!
//! Throughput is different: /sys/class/net/<if>/statistics/{rx,tx}_bytes
//! are plain counters, and a rate needs a delta between two samples. The
//! previous generation of this dashboard (a bash script looping every 5s)
//! had to fake persistent state with global associative arrays across loop
//! iterations; here it's just fields on a struct that lives as long as the
//! process does.

use std::collections::HashMap;
use std::fs;
use std::process::Command;
use std::time::Instant;

pub struct Interface {
    pub name: String,
    pub up: bool,
    pub addrs: Vec<String>,
    /// (rx bytes/sec, tx bytes/sec) - `None` until a second sample has been
    /// taken for this interface, i.e. never on the very first tick.
    pub rate: Option<(u64, u64)>,
}

struct Sample {
    rx: u64,
    tx: u64,
    at: Instant,
}

/// Holds the previous byte-counter sample per interface so `list()` can
/// compute a rate on every call after the first.
#[derive(Default)]
pub struct ThroughputSampler {
    prev: HashMap<String, Sample>,
}

impl ThroughputSampler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list(&mut self) -> Vec<Interface> {
        let mut interfaces = Vec::new();
        for line in ip_brief_addr().lines() {
            let mut fields = line.split_whitespace();
            let Some(name) = fields.next() else { continue };
            if name == "lo" {
                continue;
            }
            let up = fields.next() == Some("UP");
            let addrs: Vec<String> = fields.map(str::to_string).collect();
            let rate = self.sample_rate(name);
            interfaces.push(Interface {
                name: name.to_string(),
                up,
                addrs,
                rate,
            });
        }
        interfaces
    }

    fn sample_rate(&mut self, name: &str) -> Option<(u64, u64)> {
        let rx = read_counter(name, "rx_bytes")?;
        let tx = read_counter(name, "tx_bytes")?;
        let now = Instant::now();

        let rate = self.prev.get(name).and_then(|prev| {
            let elapsed = now.duration_since(prev.at).as_secs_f64();
            // A key mash on 'b'/any-key redraws far faster than a real 5s
            // tick; without this floor a near-zero elapsed time would
            // produce a wildly inflated (or divide-by-zero) rate.
            if elapsed < 1.0 {
                return None;
            }
            let rx_rate = (rx.saturating_sub(prev.rx) as f64 / elapsed) as u64;
            let tx_rate = (tx.saturating_sub(prev.tx) as f64 / elapsed) as u64;
            Some((rx_rate, tx_rate))
        });

        self.prev
            .insert(name.to_string(), Sample { rx, tx, at: now });
        rate
    }
}

fn read_counter(name: &str, stat: &str) -> Option<u64> {
    fs::read_to_string(format!("/sys/class/net/{name}/statistics/{stat}"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn ip_brief_addr() -> String {
    Command::new("ip")
        .args(["-brief", "addr", "show"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// bytes/sec -> human bit/s (network convention - bits, not bytes).
pub fn fmt_rate(bytes_per_sec: u64) -> String {
    let bits = bytes_per_sec as f64 * 8.0;
    if bits >= 1_000_000_000.0 {
        format!("{:.1} Gbit/s", bits / 1_000_000_000.0)
    } else if bits >= 1_000_000.0 {
        format!("{:.1} Mbit/s", bits / 1_000_000.0)
    } else if bits >= 1_000.0 {
        format!("{:.1} Kbit/s", bits / 1_000.0)
    } else {
        format!("{bits:.0} bit/s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_rate_picks_the_right_unit() {
        assert_eq!(fmt_rate(0), "0 bit/s");
        assert_eq!(fmt_rate(15), "120 bit/s");
        assert_eq!(fmt_rate(15_625), "125.0 Kbit/s");
        assert_eq!(fmt_rate(15_625_000), "125.0 Mbit/s");
        assert_eq!(fmt_rate(125_000_000), "1.0 Gbit/s");
    }
}
