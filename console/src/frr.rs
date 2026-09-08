//! FRR status via `vtysh`. There's no stable library API to link against,
//! so this talks to the same CLI a human would, exactly like the bash
//! version of this dashboard did - but asking it for `json` output rather
//! than parsing the human-readable tables: those tables' exact column
//! layout and section-header wording turned out to vary between what this
//! was originally written against and what a real, live FRR instance
//! actually prints (see git history - a whole VRF's peers silently ended
//! up folded into "default" because a header line's format didn't match
//! what the text parser assumed). JSON has a stable, documented schema
//! instead of a layout that has to be reverse-engineered from output.

use std::collections::BTreeMap;
use std::process::Command;

use serde::Deserialize;

pub struct FrrStatus {
    pub daemons: String,
    pub route_summary: Option<String>,
    /// One entry per VRF running bgpd: (vrf name, established, total).
    pub bgp_vrf_peers: Vec<(String, u32, u32)>,
}

/// `frr.service`'s own active/inactive state lives on `Snapshot` directly
/// (it's read once up front, before deciding whether it's even worth
/// asking `vtysh` anything) - this only covers what's queried through
/// `vtysh`, which is skipped entirely when the service isn't up.
pub fn gather(service_active: bool) -> FrrStatus {
    if !service_active {
        return FrrStatus {
            daemons: String::new(),
            route_summary: None,
            bgp_vrf_peers: Vec::new(),
        };
    }

    // No separate "is vtysh even installed" pre-check: vtysh() already
    // returns an empty string on any failure, binary missing included, so
    // the rest of this falls through to the same empty/no-op result
    // either way - one less process spawned per refresh for the common
    // case where it's simply there.
    let daemons = vtysh("show daemons").trim().to_string();
    let route_summary = parse_route_summary(&vtysh("show ip route summary json"));
    let bgp_vrf_peers = if daemons.split_whitespace().any(|d| d == "bgpd") {
        parse_bgp_vrf_summary(&vtysh("show bgp vrf all summary json"))
    } else {
        Vec::new()
    };

    FrrStatus {
        daemons,
        route_summary,
        bgp_vrf_peers,
    }
}

fn vtysh(cmd: &str) -> String {
    Command::new("vtysh")
        .args(["-c", cmd])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct RouteSummaryJson {
    #[serde(rename = "routesTotal")]
    routes_total: u64,
    #[serde(rename = "routesTotalFib")]
    routes_total_fib: u64,
}

fn parse_route_summary(json: &str) -> Option<String> {
    let summary: RouteSummaryJson = serde_json::from_str(json).ok()?;
    Some(format!(
        "{} routes, {} FIB",
        summary.routes_total, summary.routes_total_fib
    ))
}

#[derive(Deserialize)]
struct BgpPeerJson {
    state: String,
}

#[derive(Deserialize, Default)]
struct BgpAfiJson {
    #[serde(default)]
    peers: BTreeMap<String, BgpPeerJson>,
}

/// `show bgp vrf all summary json`'s shape: an object keyed by VRF name,
/// each value an object keyed by AFI/SAFI name (`ipv4Unicast`,
/// `ipv6Unicast`, ...), each of those holding a `peers` object keyed by
/// neighbor address. `BTreeMap` rather than `HashMap` for both outer
/// levels purely for deterministic (alphabetical) iteration order - the
/// dashboard has no other basis to sort VRFs/AFIs by.
type BgpVrfSummaryJson = BTreeMap<String, BTreeMap<String, BgpAfiJson>>;

/// Established-vs-configured BGP peer count per VRF - each tenant gets its
/// own VRF and its own BGP instance (see "VRF per Tenant" in the README),
/// so a single global peer count would hide one tenant's session being
/// down behind another's being fine. A peer counts as established when
/// its `state` field is exactly `"Established"` - the same peer appears
/// under both `ipv4Unicast` and `ipv6Unicast` for a dual-stack session, so
/// counts are summed across every AFI/SAFI a VRF has, not deduplicated by
/// address.
fn parse_bgp_vrf_summary(json: &str) -> Vec<(String, u32, u32)> {
    let Ok(vrfs) = serde_json::from_str::<BgpVrfSummaryJson>(json) else {
        return Vec::new();
    };

    vrfs.into_iter()
        .map(|(vrf, afis)| {
            let mut established = 0u32;
            let mut total = 0u32;
            for afi in afis.into_values() {
                for peer in afi.peers.into_values() {
                    total += 1;
                    if peer.state == "Established" {
                        established += 1;
                    }
                }
            }
            (vrf, established, total)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_summary_reads_rib_and_fib() {
        let json = r#"{"routes":[],"routesTotal":10,"routesTotalFib":8}"#;
        assert_eq!(
            parse_route_summary(json).as_deref(),
            Some("10 routes, 8 FIB")
        );
    }

    #[test]
    fn route_summary_none_when_not_json() {
        assert_eq!(parse_route_summary("% Command incomplete.\n"), None);
    }

    // Built from real `vtysh -c "show bgp vrf all summary json"` output
    // captured on a live VM (addresses/VRF name genericized here) - two
    // VRFs (default, vrf-tenant1), each with a dual-stack session per
    // neighbor (so each neighbor appears under both ipv4Unicast and
    // ipv6Unicast), trimmed to 2 neighbors for default and 1 for
    // vrf-tenant1. This is exactly the case the old text-table parser
    // got wrong: it never found vrf-tenant1 as a separate section and
    // folded its peers into "default" instead (see this file's own git
    // history) - the JSON schema removes the whole class of "does this
    // FRR version's header line match what the parser assumed" bug that
    // came from.
    #[test]
    fn bgp_vrf_summary_sums_established_peers_per_vrf_across_afis() {
        let json = r#"{
            "default": {
                "ipv4Unicast": {
                    "peers": {
                        "192.0.2.10": {"state": "Established"},
                        "192.0.2.11": {"state": "Established"}
                    }
                },
                "ipv6Unicast": {
                    "peers": {
                        "192.0.2.10": {"state": "Established"},
                        "192.0.2.11": {"state": "Established"}
                    }
                }
            },
            "vrf-tenant1": {
                "ipv4Unicast": {
                    "peers": {
                        "198.51.100.10": {"state": "Established"}
                    }
                },
                "ipv6Unicast": {
                    "peers": {
                        "2001:db8::1": {"state": "Idle"}
                    }
                }
            }
        }"#;
        assert_eq!(
            parse_bgp_vrf_summary(json),
            vec![
                ("default".to_string(), 4, 4),
                ("vrf-tenant1".to_string(), 1, 2),
            ]
        );
    }

    #[test]
    fn bgp_vrf_summary_empty_when_no_daemons_output() {
        assert!(parse_bgp_vrf_summary("").is_empty());
    }

    #[test]
    fn bgp_vrf_summary_vrf_with_no_peers_still_listed() {
        let json = r#"{"default": {"ipv4Unicast": {"peers": {}}}}"#;
        assert_eq!(
            parse_bgp_vrf_summary(json),
            vec![("default".to_string(), 0, 0)]
        );
    }
}
