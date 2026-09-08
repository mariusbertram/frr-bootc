//! FRR status via direct connections to each daemon's vty Unix socket
//! (`frr_vty`, shared with `config-sync`) rather than shelling out to
//! `vtysh` - one less process spawned per refresh, and the exact same
//! transport `vtysh` itself uses under the hood. Status is asked for as
//! `json` rather than parsed from the human-readable tables: those
//! tables' exact column layout and section-header wording turned out to
//! vary between what this was originally written against and what a
//! real, live FRR instance actually prints (see git history - a whole
//! VRF's peers silently ended up folded into "default" because a header
//! line's format didn't match what the text parser assumed). JSON has a
//! stable, documented schema instead of a layout that has to be
//! reverse-engineered from output.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::net::UnixStream;
use std::path::Path;

use frr_vty::VtyClient;
use serde::Deserialize;

const FRR_RUN_DIR: &str = "/var/run/frr";
const ZEBRA_SOCKET: &str = "/var/run/frr/zebra.vty";
const BGPD_SOCKET: &str = "/var/run/frr/bgpd.vty";

pub struct FrrStatus {
    pub daemons: String,
    pub route_summary: Option<String>,
    /// One entry per VRF running bgpd: (vrf name, established, total).
    /// Aggregated across every AFI/SAFI a VRF has - see
    /// `bgp_vrf_peer_detail` for the per-peer, per-AFI breakdown behind
    /// each of these counts (the "drill into a VRF" detail view).
    pub bgp_vrf_peers: Vec<(String, u32, u32)>,
    /// VRF name -> its individual BGP peers, sorted by (peer address,
    /// AFI) - a dual-stack peer contributes one row per AFI/SAFI
    /// (`ipv4Unicast`, `ipv6Unicast`, ...) rather than being collapsed
    /// into one, since the two sessions can be in different states.
    pub bgp_vrf_peer_detail: BTreeMap<String, Vec<BgpPeerDetail>>,
}

pub struct BgpPeerDetail {
    pub peer: String,
    pub afi: String,
    pub state: String,
    pub remote_as: Option<u32>,
    pub uptime: Option<String>,
    /// Prefixes received (imported) and sent (exported) - kept as the
    /// raw JSON value formatted to a string rather than a number: FRR
    /// reports these as `"N/A"` (a string) for a peer that isn't
    /// Established yet, and as a number once it is, so a fixed numeric
    /// type would fail to parse the common case of a down peer.
    pub pfx_rcd: String,
    pub pfx_snt: String,
}

/// `frr.service`'s own active/inactive state lives on `Snapshot` directly
/// (it's read once up front, before deciding whether it's even worth
/// querying any daemon socket) - this only covers what's queried below,
/// which is skipped entirely when the service isn't up.
pub fn gather(service_active: bool) -> FrrStatus {
    if !service_active {
        return FrrStatus {
            daemons: String::new(),
            route_summary: None,
            bgp_vrf_peers: Vec::new(),
            bgp_vrf_peer_detail: BTreeMap::new(),
        };
    }

    let daemons = list_daemons();
    let route_summary = parse_route_summary(&query(
        Path::new(ZEBRA_SOCKET),
        "show ip route summary json",
    ));
    let (bgp_vrf_peers, bgp_vrf_peer_detail) = if daemons.split_whitespace().any(|d| d == "bgpd") {
        parse_bgp_vrf(&query(
            Path::new(BGPD_SOCKET),
            "show bgp vrf all summary json",
        ))
    } else {
        (Vec::new(), BTreeMap::new())
    };

    FrrStatus {
        daemons,
        route_summary,
        bgp_vrf_peers,
        bgp_vrf_peer_detail,
    }
}

/// Space-separated, alphabetically sorted list of daemons whose vty
/// socket is present *and* currently accepting connections - the same
/// shape `vtysh`'s own `show daemons` produces (a plain list of names,
/// used elsewhere via `.split_whitespace().any(|d| d == "bgpd")`), but
/// derived directly from socket liveness rather than a vtysh-internal
/// command with no single backend to query instead.
fn list_daemons() -> String {
    let Ok(entries) = fs::read_dir(FRR_RUN_DIR) else {
        return String::new();
    };

    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("vty") {
                return None;
            }
            UnixStream::connect(&path).ok()?;
            path.file_stem()?.to_str().map(str::to_string)
        })
        .collect();

    names.sort();
    names.join(" ")
}

/// Runs one command against a daemon's vty socket, returning its output
/// on success or an empty string on any failure (connection refused,
/// timeout, non-zero status) - same best-effort fallback the old `vtysh`
/// subprocess helper had, so a daemon being down just blanks that part
/// of the dashboard instead of erroring.
fn query(socket_path: &Path, cmd: &str) -> String {
    let Ok(mut client) = VtyClient::connect(socket_path) else {
        return String::new();
    };
    match client.execute(cmd) {
        Ok(resp) if resp.success() => resp.output,
        _ => String::new(),
    }
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
    #[serde(default, rename = "remoteAs")]
    remote_as: Option<u32>,
    #[serde(default, rename = "peerUptime")]
    uptime: Option<String>,
    // Not a fixed type on purpose - see BgpPeerDetail::pfx_rcd's doc
    // comment on why FRR's own JSON mixes a number and the string "N/A"
    // for these fields depending on peer state.
    #[serde(default, rename = "pfxRcd")]
    pfx_rcd: Option<serde_json::Value>,
    #[serde(default, rename = "pfxSnt")]
    pfx_snt: Option<serde_json::Value>,
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

fn fmt_pfx(v: &Option<serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => "-".to_string(),
    }
}

/// Builds both the Overview/summary-panel shape (established/total per
/// VRF - each tenant gets its own VRF and its own BGP instance, see "VRF
/// per Tenant" in the README, so a single global peer count would hide
/// one tenant's session being down behind another's being fine) and the
/// per-peer detail behind it (the "drill into a VRF" view), from a
/// single parse of the same JSON rather than walking it twice. A peer
/// counts as established when its `state` field is exactly
/// `"Established"`. The same neighbor address appears under both
/// `ipv4Unicast` and `ipv6Unicast` for a dual-stack session - summed
/// into one count for the summary shape, kept as two separate rows
/// (one per AFI) in the detail shape, since the two sessions can be in
/// different states.
type BgpVrfParsed = (
    Vec<(String, u32, u32)>,
    BTreeMap<String, Vec<BgpPeerDetail>>,
);

fn parse_bgp_vrf(json: &str) -> BgpVrfParsed {
    let Ok(vrfs) = serde_json::from_str::<BgpVrfSummaryJson>(json) else {
        return (Vec::new(), BTreeMap::new());
    };

    let mut summary = Vec::new();
    let mut detail = BTreeMap::new();

    for (vrf, afis) in vrfs {
        let mut established = 0u32;
        let mut total = 0u32;
        let mut peers = Vec::new();
        for (afi, afi_data) in afis {
            for (peer, p) in afi_data.peers {
                total += 1;
                if p.state == "Established" {
                    established += 1;
                }
                peers.push(BgpPeerDetail {
                    peer,
                    afi: afi.clone(),
                    state: p.state,
                    remote_as: p.remote_as,
                    uptime: p.uptime,
                    pfx_rcd: fmt_pfx(&p.pfx_rcd),
                    pfx_snt: fmt_pfx(&p.pfx_snt),
                });
            }
        }
        peers.sort_by(|a, b| (&a.peer, &a.afi).cmp(&(&b.peer, &b.afi)));
        summary.push((vrf.clone(), established, total));
        detail.insert(vrf, peers);
    }

    (summary, detail)
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
        let (summary, _) = parse_bgp_vrf(json);
        assert_eq!(
            summary,
            vec![
                ("default".to_string(), 4, 4),
                ("vrf-tenant1".to_string(), 1, 2),
            ]
        );
    }

    #[test]
    fn bgp_vrf_summary_empty_when_no_daemons_output() {
        assert!(parse_bgp_vrf("").0.is_empty());
    }

    #[test]
    fn bgp_vrf_summary_vrf_with_no_peers_still_listed() {
        let json = r#"{"default": {"ipv4Unicast": {"peers": {}}}}"#;
        assert_eq!(parse_bgp_vrf(json).0, vec![("default".to_string(), 0, 0)]);
    }

    #[test]
    fn bgp_vrf_detail_has_one_row_per_peer_per_afi_sorted() {
        let json = r#"{
            "vrf-tenant1": {
                "ipv4Unicast": {
                    "peers": {
                        "198.51.100.10": {
                            "state": "Established",
                            "remoteAs": 65000,
                            "peerUptime": "01:23:45",
                            "pfxRcd": 12,
                            "pfxSnt": 3
                        }
                    }
                },
                "ipv6Unicast": {
                    "peers": {
                        "198.51.100.10": {"state": "Idle", "pfxRcd": "N/A", "pfxSnt": "N/A"}
                    }
                }
            }
        }"#;
        let (_, detail) = parse_bgp_vrf(json);
        let peers = &detail["vrf-tenant1"];
        assert_eq!(peers.len(), 2);

        let ipv4 = peers.iter().find(|p| p.afi == "ipv4Unicast").unwrap();
        assert_eq!(ipv4.peer, "198.51.100.10");
        assert_eq!(ipv4.state, "Established");
        assert_eq!(ipv4.remote_as, Some(65000));
        assert_eq!(ipv4.uptime.as_deref(), Some("01:23:45"));
        assert_eq!(ipv4.pfx_rcd, "12");
        assert_eq!(ipv4.pfx_snt, "3");

        let ipv6 = peers.iter().find(|p| p.afi == "ipv6Unicast").unwrap();
        assert_eq!(ipv6.state, "Idle");
        assert_eq!(ipv6.remote_as, None);
        assert_eq!(ipv6.pfx_rcd, "N/A");
        assert_eq!(ipv6.pfx_snt, "N/A");
    }
}
