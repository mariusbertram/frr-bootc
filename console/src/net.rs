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

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::process::Command;
use std::time::Instant;

/// How many throughput samples are kept per interface for the Overview
/// tab's traffic graph. 72 at the default 5s refresh is a ~6 minute
/// window, and 72 bars fit the panel width on an 80-column serial
/// console (76 inner columns) without resampling.
const TRAFFIC_HISTORY: usize = 72;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Interface {
    pub name: String,
    pub up: bool,
    pub addrs: Vec<String>,
    /// (rx bytes/sec, tx bytes/sec) - `None` until a second sample has been
    /// taken for this interface, i.e. never on the very first tick.
    pub rate: Option<(u64, u64)>,
    /// (rx, tx) error+drop events per second over the same window as
    /// `rate` - `None` on the very first tick too. Cumulative counters
    /// (in `InterfaceDetail`) say "has ever dropped a packet"; this rate
    /// says "is dropping *right now*", which is the version that means
    /// congestion/overrun on a trunk that has been up for weeks.
    pub err_rate: Option<(u64, u64)>,
}

struct Sample {
    rx: u64,
    tx: u64,
    rx_err: u64,
    tx_err: u64,
    rx_drop: u64,
    tx_drop: u64,
    at: Instant,
}

/// Rolling per-interface throughput history, (rx, tx) in bytes/sec, one
/// entry per refresh tick - the data behind the Overview traffic graph.
struct IfaceHistory {
    series: VecDeque<(u64, u64)>,
}

/// The traffic graph's data: the interface it's drawn for and its
/// rolling (rx, tx) series in bytes/sec, oldest first.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Traffic {
    pub name: String,
    pub series: Vec<(u64, u64)>,
}

impl Traffic {
    /// Latest (rx, tx) bytes/sec, if any sample exists yet.
    pub fn latest(&self) -> Option<(u64, u64)> {
        self.series.last().copied()
    }

    /// Peak (rx, tx) bytes/sec within the window - the autoscaling
    /// sparkline's implicit scale, shown as a label so the graph stays
    /// readable after a burst.
    pub fn peak(&self) -> Option<(u64, u64)> {
        self.series.iter().fold(None, |acc, &(rx, tx)| match acc {
            Some((prx, ptx)) => Some((prx.max(rx), ptx.max(tx))),
            None => Some((rx, tx)),
        })
    }
}

/// (throughput, error/drop) rate pair per direction - factored out
/// purely so `sample_rate`'s signature stays readable.
type RatePair = (Option<(u64, u64)>, Option<(u64, u64)>);

/// Holds the previous byte-counter sample per interface so `list()` can
/// compute a rate on every call after the first, plus the rolling
/// per-interface history the traffic graph draws from.
#[derive(Default)]
pub struct ThroughputSampler {
    prev: HashMap<String, Sample>,
    history: HashMap<String, IfaceHistory>,
}

impl ThroughputSampler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list(&mut self) -> Vec<Interface> {
        let vrfs = vrf_device_names();
        let mut interfaces = Vec::new();
        for line in ip_brief_addr().lines() {
            let mut fields = line.split_whitespace();
            let Some(name) = fields.next() else { continue };
            // "lo" isn't a meaningful link to show here, and a VRF
            // device (e.g. "vrf-tenant1") isn't a link at all - it's a
            // routing-table selector the kernel's VRF driver exposes as
            // a netdev for `ip`/rtnetlink's benefit, with its own
            // driver-default MTU (65535+, unrelated to any real frame
            // size) that reads as nonsensical/alarming in a table meant
            // for physical/logical links. VRFs already have their own
            // view - the FRR/BGP tab.
            if name == "lo" || vrfs.contains(name) {
                continue;
            }
            let up = fields.next() == Some("UP");
            let addrs: Vec<String> = fields.map(str::to_string).collect();
            let (rate, err_rate) = self.sample_rate(name);
            interfaces.push(Interface {
                name: name.to_string(),
                up,
                addrs,
                rate,
                err_rate,
            });
        }
        interfaces
    }

    /// The traffic graph's series: the interface carrying the most
    /// cumulative traffic (in this image's design that is always the one
    /// physical trunk - every tenant's VLAN sub-interface rides on it)
    /// and its rolling history. Keyed by what's actually been sampled
    /// rather than an assumed "eth-trunk" name: before the first
    /// successful nmstate apply the device still has its kernel name.
    pub fn traffic(&self) -> Option<Traffic> {
        let name = self
            .prev
            .iter()
            .max_by_key(|(_, s)| s.rx.saturating_add(s.tx))
            .map(|(name, _)| name.clone())?;
        let series = self.history_of(&name)?;
        Some(Traffic { name, series })
    }

    /// Copy of one interface's rolling history, oldest first - empty
    /// until at least two samples have been taken for it (no rates
    /// before that, so no history entries either).
    pub fn history_of(&self, name: &str) -> Option<Vec<(u64, u64)>> {
        let history = self.history.get(name)?;
        Some(history.series.iter().copied().collect())
    }

    /// (throughput rate, error+drop rate) over the window since the
    /// previous sample. Also the one place history entries are pushed:
    /// a rate only exists for a real ~REFRESH_SECS-apart sample pair, so
    /// the graph gets exactly one bar per refresh tick - key-mash
    /// redraws (sub-second, rate `None` below) must not insert
    /// zero-width pseudo-samples that would silently stretch the window.
    fn sample_rate(&mut self, name: &str) -> RatePair {
        let rx = read_counter(name, "rx_bytes");
        let tx = read_counter(name, "tx_bytes");
        let (Some(rx), Some(tx)) = (rx, tx) else {
            return (None, None);
        };
        let rx_err = read_counter(name, "rx_errors").unwrap_or(0);
        let tx_err = read_counter(name, "tx_errors").unwrap_or(0);
        let rx_drop = read_counter(name, "rx_dropped").unwrap_or(0);
        let tx_drop = read_counter(name, "tx_dropped").unwrap_or(0);
        let now = Instant::now();

        let outcome = self.prev.get(name).and_then(|prev| {
            let elapsed = now.duration_since(prev.at).as_secs_f64();
            // A key mash on 'b'/any-key redraws far faster than a real 5s
            // tick; without this floor a near-zero elapsed time would
            // produce a wildly inflated (or divide-by-zero) rate.
            if elapsed < 1.0 {
                return None;
            }
            let rate = (
                (rx.saturating_sub(prev.rx) as f64 / elapsed) as u64,
                (tx.saturating_sub(prev.tx) as f64 / elapsed) as u64,
            );
            let err_rate = (
                ((rx_err.saturating_sub(prev.rx_err) + rx_drop.saturating_sub(prev.rx_drop)) as f64
                    / elapsed) as u64,
                ((tx_err.saturating_sub(prev.tx_err) + tx_drop.saturating_sub(prev.tx_drop)) as f64
                    / elapsed) as u64,
            );
            Some((rate, err_rate))
        });

        self.prev.insert(
            name.to_string(),
            Sample {
                rx,
                tx,
                rx_err,
                tx_err,
                rx_drop,
                tx_drop,
                at: now,
            },
        );

        if let Some((rate, err_rate)) = outcome {
            let history = self
                .history
                .entry(name.to_string())
                .or_insert_with(|| IfaceHistory {
                    series: VecDeque::with_capacity(TRAFFIC_HISTORY),
                });
            history.series.push_back(rate);
            while history.series.len() > TRAFFIC_HISTORY {
                history.series.pop_front();
            }
            return (Some(rate), Some(err_rate));
        }
        (None, None)
    }
}

/// `ip` displays a sub-interface's name suffixed with `@<parent>` (e.g.
/// `bdbos@enp3s0` for a VLAN sub-interface, or any device whose
/// `IFLA_LINK` points at a different one) - worth keeping as the
/// *display* name (`Interface::name` already does, unchanged), but
/// that suffix is purely an `ip`-side convention, not part of the real
/// interface name: `/sys/class/net/` has no `bdbos@enp3s0` entry, only
/// `bdbos`. Every sysfs read needs the stripped form, or it silently
/// finds nothing (a missing file, not an error) - which is exactly what
/// was happening to every VLAN sub-interface's throughput sampling,
/// and to the detail popup's MTU/MAC/master/counters.
fn sysfs_name(name: &str) -> &str {
    name.split('@').next().unwrap_or(name)
}

fn read_counter(name: &str, stat: &str) -> Option<u64> {
    let name = sysfs_name(name);
    fs::read_to_string(format!("/sys/class/net/{name}/statistics/{stat}"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Extra per-interface detail beyond what the always-on `Interface`
/// listing carries - gathered only for whichever single interface the
/// console dashboard's "drill in" detail view is currently showing
/// (see ui.rs), not for every interface on every refresh: cheap per
/// interface, but with this repo's own scaling example being "150
/// BGP-coupled tenants" (see the README's "A Trunk Instead of One NIC
/// per Tenant" section), each with its own VLAN sub-interface, gathering
/// all of this for every one of them on every 5s tick would add up for
/// no benefit - nothing shows this except the one open detail view.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InterfaceDetail {
    pub mtu: Option<u32>,
    pub mac: Option<String>,
    /// The device this interface is enslaved to (e.g. its VRF, for a
    /// tenant's VLAN sub-interface - see "VRF per Tenant" in the
    /// README), read from the `master` symlink `ip link` itself follows
    /// to show the same relationship.
    pub master: Option<String>,
    pub rx_bytes: Option<u64>,
    pub tx_bytes: Option<u64>,
    pub rx_packets: Option<u64>,
    pub tx_packets: Option<u64>,
    pub rx_errors: Option<u64>,
    pub tx_errors: Option<u64>,
    pub rx_dropped: Option<u64>,
    pub tx_dropped: Option<u64>,
}

pub fn detail(name: &str) -> InterfaceDetail {
    InterfaceDetail {
        mtu: read_sys_value(name, "mtu"),
        mac: fs::read_to_string(format!("/sys/class/net/{}/address", sysfs_name(name)))
            .ok()
            .map(|s| s.trim().to_string()),
        master: read_master(name),
        rx_bytes: read_counter(name, "rx_bytes"),
        tx_bytes: read_counter(name, "tx_bytes"),
        rx_packets: read_counter(name, "rx_packets"),
        tx_packets: read_counter(name, "tx_packets"),
        rx_errors: read_counter(name, "rx_errors"),
        tx_errors: read_counter(name, "tx_errors"),
        rx_dropped: read_counter(name, "rx_dropped"),
        tx_dropped: read_counter(name, "tx_dropped"),
    }
}

fn read_sys_value<T: std::str::FromStr>(name: &str, file: &str) -> Option<T> {
    let name = sysfs_name(name);
    fs::read_to_string(format!("/sys/class/net/{name}/{file}"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// `/sys/class/net/<name>/master` is a symlink to the enslaving device
/// (e.g. `/sys/class/net/<name>/master -> ../vrf-tenant1`) when one
/// exists, absent otherwise - the same relationship `ip -d link show`
/// reports as `master vrf-tenant1`, read directly rather than shelling
/// out for it.
fn read_master(name: &str) -> Option<String> {
    let name = sysfs_name(name);
    let link = fs::read_link(format!("/sys/class/net/{name}/master")).ok()?;
    link.file_name()?.to_str().map(str::to_string)
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

/// The kernel's own authoritative list of VRF devices (`ip link show
/// type vrf` filters by netdev kind, not by naming convention - a
/// tenant could in principle name theirs anything). Same reasoning as
/// `ip_brief_addr`'s own doc comment: the exact command an operator
/// would run, not a heuristic layered on top of a different one's
/// output.
fn vrf_device_names() -> HashSet<String> {
    let output = Command::new("ip")
        .args(["-brief", "link", "show", "type", "vrf"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect()
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

    #[test]
    fn sysfs_name_strips_the_ip_display_suffix() {
        assert_eq!(sysfs_name("bdbos@enp3s0"), "bdbos");
        assert_eq!(sysfs_name("enp3s0"), "enp3s0");
        assert_eq!(sysfs_name("vrf-tenant1"), "vrf-tenant1");
    }

    #[test]
    fn traffic_peak_and_latest_read_the_series_ends() {
        let traffic = Traffic {
            name: "eth-trunk".to_string(),
            series: vec![(10, 5), (30, 100), (20, 50)],
        };
        assert_eq!(traffic.latest(), Some((20, 50)));
        assert_eq!(traffic.peak(), Some((30, 100)));
        let empty = Traffic {
            name: "eth-trunk".to_string(),
            series: Vec::new(),
        };
        assert_eq!(empty.latest(), None);
        assert_eq!(empty.peak(), None);
    }
}
