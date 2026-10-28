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
const BFDD_SOCKET: &str = "/var/run/frr/bfdd.vty";

#[derive(Debug, Clone, serde::Serialize)]
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
    /// VRF name -> (RIB routes, FIB routes) for VRFs with a BGP
    /// instance. A RIB/FIB divergence (RIB > FIB) means FRR knows routes
    /// the kernel hasn't installed - worth seeing per VRF, since a global
    /// count hides one tenant's blackhole behind everyone else's working
    /// routing. Only gathered for a bounded number of VRFs (see
    /// `gather_vrf_routes`) so 150 tenants can't turn one dashboard tick
    /// into 150 vty round-trips.
    pub vrf_routes: BTreeMap<String, (u64, u64)>,
    /// Every BFD session bfdd knows about, one entry per (peer, vrf).
    /// BFD is what actually detects the link failures that bounce the
    /// BGP sessions (`neighbor ... bfd` is this image's standard tenant
    /// peering setup) - a BFD session going down is the *earliest*
    /// signal of a tenant link problem, often before BGP reacts, so it
    /// gets its own row in the FRR tab instead of being inferable only
    /// from a BGP session that flapped a minute later.
    pub bfd_peers: Vec<BfdPeerDetail>,
    /// Transport- and schema-level failures from this tick's vty
    /// queries (connect refused, command rejected, unparseable response,
    /// schema drift) - the queries themselves stay best-effort (a down
    /// daemon blanks its section rather than erroring the dashboard),
    /// but "blank because nothing to report" and "blank because the
    /// socket didn't answer" are different situations an operator
    /// shouldn't have to guess between. Shown muted in the FRR panel,
    /// printed by the one-shot modes, and recorded in the event log on
    /// transition (see events.rs). Deliberately NOT part of the health
    /// verdict (see health.rs): a momentarily refusing socket while bgpd
    /// restarts costs observability, not routing.
    pub query_errors: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct BfdPeerDetail {
    pub peer: String,
    pub vrf: String,
    /// FRR's own word - "up" or "down" (compared case-insensitively by
    /// consumers; whatever bfdd prints is carried through verbatim).
    pub status: String,
    /// Session uptime, human-formatted (seconds in the JSON).
    pub uptime: Option<String>,
}

/// Which direction of a peer's prefix tables a query/popups is about.
/// `Advertised` is the Adj-RIB-Out (what this router sends the peer),
/// `Received` the Adj-RIB-In (what the peer sends this router) - the
/// pair that separates "we suppress it" from "it never arrived".
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Default)]
pub enum RouteDirection {
    #[default]
    Advertised,
    Received,
}

impl RouteDirection {
    pub fn word(self) -> &'static str {
        match self {
            RouteDirection::Advertised => "advertised",
            RouteDirection::Received => "received",
        }
    }
}

/// Which peer's which prefix table a query/popup is about - the full
/// identity a gathered `PeerRoutes` answer must match, so a stale answer
/// (one selection behind, arriving over a different tick) can never
/// render as an answer about a different peering/direction.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PeerQueryKey {
    pub direction: RouteDirection,
    pub vrf: String,
    pub peer: String,
    pub afi: String,
}

impl PeerQueryKey {
    pub fn matches(&self, routes: &PeerRoutes) -> bool {
        routes.direction == self.direction
            && routes.vrf == self.vrf
            && routes.peer == self.peer
            && routes.afi == self.afi
    }
}

/// One prefix in a peer's Adj-RIB-In/Out. `pfxRcd`/`pfxSnt` in the BGP
/// summary are only counts; this is the list behind them, which is what
/// answering "warum sieht der Upstream die BPATG-Präfixe nicht" actually
/// needs (an empty Adj-RIB-Out with an established session points at a
/// route-map/prefix-list suppressing the export; an empty Adj-RIB-In
/// points at the peer's side - it never sent the route in the first
/// place, e.g. a MetalLB pool with no service holding the prefix).
#[derive(Debug, Clone, serde::Serialize)]
pub struct BgpPeerPrefix {
    pub prefix: String,
    /// AS path as FRR prints it (`aspath.string`), e.g. "65001 65100" -
    /// an empty path (locally originated) is `None`.
    pub aspath: Option<String>,
    /// Origin attribute (`IGP`/`EGP`/`incomplete").
    pub origin: Option<String>,
    /// Whether this path is the best one for the prefix (FRR only
    /// advertises best paths, so this is expected to be true for every
    /// entry here - carried through anyway so a future filter change
    /// in FRR becomes visible rather than silently re-normalized).
    pub best: bool,
}

/// The answer to one advertised-/received-routes query, gathered on
/// demand for the single peer being looked at (see `Selection::routes`
/// and `snapshot::gather` - never for every peer on every tick, for the
/// same 150-tenant round-trip-budget reasoning as `gather_vrf_routes`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PeerRoutes {
    pub direction: RouteDirection,
    pub vrf: String,
    pub peer: String,
    pub afi: String,
    /// True when `received-routes` was rejected (the neighbor has no
    /// `soft-reconfiguration inbound`, so the pre-policy Adj-RIB-In
    /// isn't kept) and the query fell back to the post-policy `routes`
    /// form - the popup notes which of the two views it is showing.
    pub post_policy_fallback: bool,
    pub prefixes: Vec<BgpPeerPrefix>,
    /// Transport/parse failure for this query, if any - "no routes"
    /// (empty list, no error) and "couldn't ask" are different
    /// findings, same distinction `FrrStatus::query_errors` makes for
    /// the periodic queries.
    pub error: Option<String>,
}

/// The AFI/SAFI names `show bgp vrf all summary json` reports (the JSON
/// spelling, e.g. `ipv4Unicast`) translated into the two-token vty
/// command form `show bgp <afi> neighbors ...` expects. A fixed mapping
/// rather than string surgery: anything unrecognized returns `None`
/// and the caller surfaces it as a query error instead of sending a
/// malformed command to bgpd.
fn afi_to_cli(afi: &str) -> Option<&'static str> {
    match afi {
        "ipv4Unicast" => Some("ipv4 unicast"),
        "ipv6Unicast" => Some("ipv6 unicast"),
        "ipv4LabeledUnicast" => Some("ipv4 labeled-unicast"),
        "ipv6LabeledUnicast" => Some("ipv6 labeled-unicast"),
        "l2vpnEvpn" => Some("l2vpn evpn"),
        "ipv4Vpn" => Some("ipv4 vpn"),
        "ipv6Vpn" => Some("ipv6 vpn"),
        "ipv4Flowspec" => Some("ipv4 flowspec"),
        "ipv6Flowspec" => Some("ipv6 flowspec"),
        _ => None,
    }
}

/// Same reasoning as `is_safe_vrf_name`, for the neighbor address that
/// gets interpolated into `show bgp ... neighbors <peer>
/// advertised-routes json` - IPv6 addresses need `:` in their charset.
fn is_safe_peer_addr(peer: &str) -> bool {
    !peer.is_empty()
        && peer.len() <= 45
        && peer
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.'))
}

/// Asks bgpd for one peer's Adj-RIB-Out or Adj-RIB-In (`show bgp
/// [vrf <vrf>] <afi> neighbors <peer> advertised-routes json` or the
/// `received-routes` form) - the per-peer, per-prefix breakdown behind
/// the summary's `pfxSnt`/`pfxRcd` counts. The default VRF uses the
/// command form without a `vrf` clause (the form every FRR version this
/// image runs accepts); named VRFs use `vrf <name>`. Failures are
/// carried in `error` rather than treated as "nothing": an established
/// session with an empty result list is itself the finding (for
/// advertised: a route-map/prefix-list is suppressing the export; for
/// received: the peer never sent the route), and it must not be
/// conflated with a query that never got answered.
pub fn peer_prefixes(direction: RouteDirection, vrf: &str, peer: &str, afi: &str) -> PeerRoutes {
    let mut result = PeerRoutes {
        direction,
        vrf: vrf.to_string(),
        peer: peer.to_string(),
        afi: afi.to_string(),
        post_policy_fallback: false,
        prefixes: Vec::new(),
        error: None,
    };
    let what = direction.word();
    let Some(cli_afi) = afi_to_cli(afi) else {
        result.error = Some(format!("{what}-routes: unsupported AFI \"{afi}\""));
        return result;
    };
    if !is_safe_vrf_name(vrf) {
        result.error = Some(format!(
            "{what}-routes: skipping VRF with unexpected name ({vrf})"
        ));
        return result;
    }
    if !is_safe_peer_addr(peer) {
        result.error = Some(format!(
            "{what}-routes: skipping peer with unexpected address ({peer})"
        ));
        return result;
    }

    let cmd = if vrf == "default" {
        format!("show bgp {cli_afi} neighbors {peer} {what}-routes json")
    } else {
        format!("show bgp vrf {vrf} {cli_afi} neighbors {peer} {what}-routes json")
    };
    let query_result = query(Path::new(BGPD_SOCKET), &cmd);
    let (prefixes, parse_error) = parse_peer_routes(&query_result.output);
    result.prefixes = prefixes;
    result.error = query_result.error.or(parse_error);

    // `received-routes` (pre-policy Adj-RIB-In) requires the neighbor to
    // be configured with `soft-reconfiguration inbound` - bgpd rejects
    // the command otherwise ("% Inbound soft reconfig for X not
    // possible as it has neither refresh capability, nor inbound soft
    // reconfig"), which is exactly the "Command rejected" the live VM
    // showed. Fall back to the plain `routes` form: what the peer sent
    // *after* our import filters, which for the question this view
    // answers ("did the route we expect ever arrive from the peer") is
    // the operative table anyway - and note the substitution so the
    // popup says which view it is showing, not silently the other one.
    if result.error.is_some() && direction == RouteDirection::Received {
        let fallback_cmd = if vrf == "default" {
            format!("show bgp {cli_afi} neighbors {peer} routes json")
        } else {
            format!("show bgp vrf {vrf} {cli_afi} neighbors {peer} routes json")
        };
        let fallback = query(Path::new(BGPD_SOCKET), &fallback_cmd);
        let (prefixes, parse_error) = parse_peer_routes(&fallback.output);
        if fallback.error.is_none() && parse_error.is_none() {
            result.prefixes = prefixes;
            // Not an error anymore - but carry the note so the UI can
            // render which view it actually shows.
            result.error = None;
            result.post_policy_fallback = true;
        }
    }
    result
}

/// Parses the advertised-/received-routes JSON into a prefix-sorted
/// list.
/// Expected shape is `{"routes": {"<prefix>": {"paths": [...]}}}`
/// (plus whatever top-level metadata this FRR version adds, ignored);
/// a bare routes object without the `routes` wrapper is tolerated the
/// same way `parse_bfd_peers` tolerates field-spelling drift. Empty is
/// a valid answer - `"routes": {}` means the peer is genuinely being
/// advertised nothing, which is exactly the situation this view exists
/// to make visible, so it parses to `(empty, no error)` rather than to
/// a schema-drift note. Anything non-JSON (a `% ...` vty error line for
/// a peer with no session in this AFI, an unknown-command rejection on
/// an older FRR build) surfaces as an error carrying bgpd's own first
/// line - that text is the diagnosis.
type PeerRoutesParsed = (Vec<BgpPeerPrefix>, Option<String>);

fn parse_peer_routes(json: &str) -> PeerRoutesParsed {
    if json.trim().is_empty() {
        return (Vec::new(), None);
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return (
            Vec::new(),
            Some(format!("peer-routes: {}", first_line(json))),
        );
    };
    // FRR wraps these per-direction: `show bgp ... advertised-routes
    // json` nests the prefix map under "advertisedRoutes",
    // received-routes under "receivedRoutes", the plain `routes` form
    // under "routes". All three accepted; also a direct prefix-keyed
    // object (older builds / alternate shapes) as before.
    let routes = match value
        .get("routes")
        .or_else(|| value.get("advertisedRoutes"))
        .or_else(|| value.get("receivedRoutes"))
    {
        Some(serde_json::Value::Null) => return (Vec::new(), None),
        Some(v) if v.is_object() => v.as_object().unwrap(),
        Some(_) => {
            return (
                Vec::new(),
                Some("peer-routes: routes is not an object - schema drift?".to_string()),
            )
        }
        // No wrapper at all: accept the object itself if it looks like
        // a prefix-keyed map of path entries; anything else is drift.
        None => match value
            .as_object()
            .filter(|m| m.values().any(|e| e.get("paths").is_some()))
        {
            Some(map) => map,
            None => {
                return (
                    Vec::new(),
                    Some("peer-routes: response has no routes object - schema drift?".to_string()),
                )
            }
        },
    };

    let mut out = Vec::new();
    for (prefix, entry) in routes {
        let path = entry
            .get("paths")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .or_else(|| entry.get("path"));
        let aspath = path
            .and_then(|p| p.get("aspath"))
            .and_then(|a| a.get("string"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let origin = path
            .and_then(|p| p.get("origin"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let best = path
            .and_then(|p| p.get("bestpath"))
            .and_then(|b| b.get("overall"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        out.push(BgpPeerPrefix {
            prefix: prefix.clone(),
            aspath,
            origin,
            best,
        });
    }
    // Numeric-aware order ("10.32.0.0" after "10.128.0.0", not between
    // "10.1" and "10.2" the way a plain string sort would put it), so
    // scanning for one prefix in a long list stays intuitive.
    out.sort_by(|a, b| cmp_prefix(&a.prefix, &b.prefix));
    (out, None)
}

/// Compares prefix strings by their numeric runs rather than
/// byte-by-byte - dotted quads and IPv6 hextets compare as numbers,
/// separators as text. A parse failure inside a run can't happen (only
/// ascii digits accumulate), the `unwrap_or` keeps it total anyway.
fn cmp_prefix(a: &str, b: &str) -> std::cmp::Ordering {
    let key = |s: &str| -> Vec<(u64, String)> {
        let mut parts = Vec::new();
        let mut num = String::new();
        let mut other = String::new();
        for c in s.chars() {
            if c.is_ascii_digit() {
                if !other.is_empty() {
                    parts.push((u64::MAX, other.clone()));
                    other.clear();
                }
                num.push(c);
            } else {
                if !num.is_empty() {
                    parts.push((num.parse().unwrap_or(u64::MAX), String::new()));
                    num.clear();
                }
                other.push(c);
            }
        }
        if !num.is_empty() {
            parts.push((num.parse().unwrap_or(u64::MAX), String::new()));
        }
        if !other.is_empty() {
            parts.push((u64::MAX, other));
        }
        parts
    };
    key(a).cmp(&key(b))
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
            vrf_routes: BTreeMap::new(),
            bfd_peers: Vec::new(),
            query_errors: Vec::new(),
        };
    }

    let daemons = list_daemons();
    let mut query_errors = Vec::new();

    let route_summary = {
        let result = query(Path::new(ZEBRA_SOCKET), "show ip route summary json");
        add_query_error(&mut query_errors, result.error);
        parse_route_summary(&result.output)
    };
    let has_daemon = |name: &str| daemons.split_whitespace().any(|d| d == name);

    let (bgp_vrf_peers, bgp_vrf_peer_detail) = if has_daemon("bgpd") {
        let result = query(Path::new(BGPD_SOCKET), "show bgp vrf all summary json");
        add_query_error(&mut query_errors, result.error);
        parse_bgp_vrf(&result.output)
    } else {
        (Vec::new(), BTreeMap::new())
    };
    let bfd_peers = if has_daemon("bfdd") {
        let result = query(Path::new(BFDD_SOCKET), "show bfd peers json");
        add_query_error(&mut query_errors, result.error);
        let (peers, drift) = parse_bfd_peers(&result.output);
        add_query_error(&mut query_errors, drift);
        peers
    } else {
        Vec::new()
    };
    let vrf_routes = gather_vrf_routes(has_daemon("zebra"), &bgp_vrf_peers, &mut query_errors);

    FrrStatus {
        daemons,
        route_summary,
        bgp_vrf_peers,
        bgp_vrf_peer_detail,
        vrf_routes,
        bfd_peers,
        query_errors,
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

/// One vty query's outcome: the output on success, or a description of
/// what went wrong (plus empty output) on any failure. The dashboard
/// still treats a failure as "this section is blank" - same best-effort
/// fallback the old `vtysh` subprocess helper had - but the failure is
/// no longer indistinguishable from "nothing to report": the caller
/// carries `error` into `FrrStatus::query_errors`.
struct QueryResult {
    output: String,
    error: Option<String>,
}

/// Runs one command against a daemon's vty socket.
fn query(socket_path: &Path, cmd: &str) -> QueryResult {
    // The daemon's socket file stem ("bgpd", "zebra", ...) - enough to
    // attribute an error in the panel/event log without dragging full
    // paths into a one-line UI.
    let name = socket_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("vty");
    let mut client = match VtyClient::connect(socket_path) {
        Ok(client) => client,
        Err(e) => {
            return QueryResult {
                output: String::new(),
                error: Some(format!("{name}: connect failed ({e})")),
            }
        }
    };
    match client.execute(cmd) {
        Ok(resp) if resp.success() => QueryResult {
            output: resp.output,
            error: None,
        },
        Ok(resp) => QueryResult {
            output: String::new(),
            error: Some(format!(
                "{name}: command rejected: {}",
                first_line(&resp.output)
            )),
        },
        Err(e) => QueryResult {
            output: String::new(),
            error: Some(format!("{name}: query failed ({e})")),
        },
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().map(str::trim).unwrap_or("")
}

/// Adds a query failure to the panel's error list - deduplicated (the
/// same failure repeats every tick) and capped (a pathological tick
/// must not grow this list without bound; 8 entries say everything a
/// muted UI line needs).
fn add_query_error(errors: &mut Vec<String>, error: Option<String>) {
    if let Some(error) = error {
        if !errors.contains(&error) && errors.len() < 8 {
            errors.push(error);
        }
    }
}

/// Only names that are unambiguous as a single vty command token are
/// ever interpolated into `show ip route vrf <name> summary`. The name
/// comes from FRR's own BGP summary JSON - ultimately from the
/// `frr.conf` ConfigMap, i.e. someone who already controls the router's
/// whole configuration - so this is defense in depth against
/// *accidental* breakage (a name carrying a newline or `;` would corrupt
/// the constructed query), not a trust boundary: anything outside the
/// charset real VRF names use is skipped, with a note in
/// `query_errors` instead of a silently wrong query.
fn is_safe_vrf_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Per-VRF route counts for the VRFs that have a BGP instance. Two
/// strategies, tried in order:
///
/// 1. `show ip route vrf all summary json` - one round-trip for every
///    VRF at once, *if* this FRR build supports the `vrf all` form
///    (parsed leniently; anything that isn't a VRF-keyed map of
///    routesTotal/routesTotalFib counts as "not supported").
/// 2. One `show ip route vrf <name> summary json` per VRF - but only
///    while there are few enough VRFs that a dashboard tick can't turn
///    into a vty query storm (this repo's own scaling example is 150
///    tenants; past the cap the panel simply shows "-" and the global
///    RIB count stays the authoritative number).
fn gather_vrf_routes(
    zebra_up: bool,
    bgp_vrf_peers: &[(String, u32, u32)],
    errors: &mut Vec<String>,
) -> BTreeMap<String, (u64, u64)> {
    let mut routes = BTreeMap::new();
    if !zebra_up || bgp_vrf_peers.is_empty() {
        return routes;
    }

    let all = query(
        Path::new(ZEBRA_SOCKET),
        "show ip route vrf all summary json",
    );
    add_query_error(errors, all.error);
    routes.extend(parse_vrf_all_route_summary(&all.output));
    if !routes.is_empty() {
        return routes;
    }

    let cap = 30;
    if bgp_vrf_peers.len() > cap {
        return routes;
    }
    for (vrf, _, _) in bgp_vrf_peers {
        if !is_safe_vrf_name(vrf) {
            add_query_error(
                errors,
                Some(format!("routes: skipping VRF with unexpected name ({vrf})")),
            );
            continue;
        }
        let json = query(
            Path::new(ZEBRA_SOCKET),
            &format!("show ip route vrf {vrf} summary json"),
        );
        add_query_error(errors, json.error);
        if let Some(summary) = parse_route_summary_json(&json.output) {
            routes.insert(vrf.clone(), summary);
        }
    }
    routes
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

/// One VRF's summary JSON -> (routesTotal, routesTotalFib), or None.
fn parse_route_summary_json(json: &str) -> Option<(u64, u64)> {
    let summary: RouteSummaryJson = serde_json::from_str(json).ok()?;
    Some((summary.routes_total, summary.routes_total_fib))
}

/// Lenient parse of `show ip route vrf all summary json` into
/// VRF -> (routesTotal, routesTotalFib). Whatever this returns on a
/// build where the command doesn't exist (an error line, a plain
/// summary object) is structurally incapable of being a VRF-keyed map
/// of summary objects, so the caller's "empty means fall back" check is
/// sound.
fn parse_vrf_all_route_summary(json: &str) -> BTreeMap<String, (u64, u64)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return BTreeMap::new();
    };
    let Some(map) = value.as_object() else {
        return BTreeMap::new();
    };
    let mut routes = BTreeMap::new();
    for (vrf, entry) in map {
        let (Some(total), Some(fib)) = (
            entry.get("routesTotal").and_then(|v| v.as_u64()),
            entry.get("routesTotalFib").and_then(|v| v.as_u64()),
        ) else {
            continue;
        };
        routes.insert(vrf.clone(), (total, fib));
    }
    routes
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

/// `show bfd peers json`'s shape (FRR's bfdd): `{"peers": [ {...}, ... ]}`
/// with one object per session. The fields this reads are matched
/// leniently through the raw `Value` rather than a strict struct - peer
/// address has been spelled `remote`, `peer` or `address` across FRR
/// versions, and uptime has appeared as either a number (seconds) or a
/// string - so an unexpected spelling degrades to "-" fields instead of
/// blanking the whole BFD section. Sessions whose address can't be
/// identified at all are skipped: an unattributable "something is down"
/// row would be worse than none.
/// (peers, schema-drift note) - the note is `Some` whenever the response
/// didn't match any shape this parser knows, so a future FRR renaming
/// its BFD JSON fields surfaces as a visible `query_errors` entry
/// instead of a silently empty BFD section.
type BfdPeersParsed = (Vec<BfdPeerDetail>, Option<String>);

fn parse_bfd_peers(json: &str) -> BfdPeersParsed {
    // An empty response means bfdd answered with nothing (no sessions,
    // or the daemon restarting) - normal, not drift.
    if json.trim().is_empty() {
        return (Vec::new(), None);
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return (
            Vec::new(),
            Some("BFD: response is not JSON - schema drift?".to_string()),
        );
    };
    // Either `{"peers": [...]}` or, defensively, a bare array.
    let Some(peers) = value
        .get("peers")
        .and_then(|v| v.as_array())
        .or_else(|| value.as_array())
    else {
        return (
            Vec::new(),
            Some("BFD: response has no peers array - schema drift?".to_string()),
        );
    };

    let mut out = Vec::new();
    let mut unrecognized = 0usize;
    for peer in peers {
        let addr = ["remote", "peer", "address"]
            .iter()
            .find_map(|k| peer.get(*k).and_then(|v| v.as_str()))
            .map(str::to_string);
        // An entry whose address can't be identified is skipped (an
        // unattributable "something is down" row would be worse than
        // none) - but counted, so wholesale field renames don't just
        // look like "no BFD configured".
        let Some(peer_addr) = addr else {
            unrecognized += 1;
            continue;
        };
        let vrf = peer
            .get("vrf")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        let status = ["status", "state"]
            .iter()
            .find_map(|k| peer.get(*k).and_then(|v| v.as_str()))
            .unwrap_or("unknown")
            .to_string();
        let uptime = peer.get("uptime").and_then(|v| match v {
            serde_json::Value::Number(n) => n.as_u64().map(fmt_uptime),
            serde_json::Value::String(s) => Some(s.clone()),
            _ => None,
        });
        out.push(BfdPeerDetail {
            peer: peer_addr,
            vrf,
            status,
            uptime,
        });
    }
    out.sort_by(|a, b| (&a.vrf, &a.peer).cmp(&(&b.vrf, &b.peer)));
    let drift = (unrecognized > 0).then(|| {
        format!(
            "BFD: {unrecognized}/{} peer entries had unrecognized fields",
            peers.len()
        )
    });
    (out, drift)
}

/// Seconds -> "3d 2h"/"5m 10s"-style compact uptime, for BFD sessions
/// (which report numeric seconds) so they read like BGP's own
/// `peerUptime` strings elsewhere in the dashboard.
fn fmt_uptime(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        3600..=86399 => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
        _ => format!("{}d {}h", secs / 86400, secs % 86400 / 3600),
    }
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

    #[test]
    fn bfd_peers_parse_from_the_peers_array() {
        let json = r#"{"peers": [
            {"remote": "198.51.100.1", "vrf": "vrf-tenant1", "status": "up",
             "uptime": 3661, "diagnostic": "ok"},
            {"remote": "198.51.100.10", "vrf": "vrf-tenant1", "status": "down",
             "uptime": 5}
        ]}"#;
        let (peers, drift) = parse_bfd_peers(json);
        assert!(drift.is_none());
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].peer, "198.51.100.1");
        assert_eq!(peers[0].vrf, "vrf-tenant1");
        assert_eq!(peers[0].status, "up");
        assert_eq!(peers[0].uptime.as_deref(), Some("1h 1m"));
        assert_eq!(peers[1].status, "down");
        assert_eq!(peers[1].uptime.as_deref(), Some("5s"));
    }

    #[test]
    fn bfd_peers_tolerate_alternate_field_spellings_and_bare_arrays() {
        // An FRR build that spells the address `peer` and the state
        // `state`, inside a bare array - must still parse rather than
        // blanking the whole section.
        let json = r#"[{"peer": "192.0.2.9", "state": "Up", "uptime": "00:05:00"}]"#;
        let (peers, drift) = parse_bfd_peers(json);
        assert!(drift.is_none());
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].peer, "192.0.2.9");
        assert_eq!(peers[0].vrf, "default");
        assert_eq!(peers[0].status, "Up");
        assert_eq!(peers[0].uptime.as_deref(), Some("00:05:00"));
    }

    // The leniency above is one-way tolerable, not one-way silent: when
    // an FRR upgrade renames fields wholesale, the section must not just
    // quietly read as "no BFD configured" - the drift note turns into a
    // query_errors entry and from there into the event log.
    #[test]
    fn bfd_schema_drift_is_reported_not_swallowed() {
        let (peers, drift) = parse_bfd_peers(r#"{"peers": [{"status": "down"}, {"newfield": 1}]}"#);
        assert!(peers.is_empty());
        assert_eq!(
            drift.as_deref(),
            Some("BFD: 2/2 peer entries had unrecognized fields")
        );

        let (_, drift) = parse_bfd_peers("not json at all");
        assert!(drift.is_some());

        let (_, drift) = parse_bfd_peers(r#"{"unexpected": {}}"#);
        assert_eq!(
            drift.as_deref(),
            Some("BFD: response has no peers array - schema drift?")
        );

        // A genuinely empty response is normal (no sessions / daemon
        // restarting), not drift.
        let (peers, drift) = parse_bfd_peers("");
        assert!(peers.is_empty() && drift.is_none());
    }

    #[test]
    fn vrf_names_are_validated_before_interpolation_into_queries() {
        assert!(is_safe_vrf_name("default"));
        assert!(is_safe_vrf_name("vrf-tenant1"));
        assert!(is_safe_vrf_name("VRF_10.5"));

        // Anything that could read as vty command syntax or whitespace
        // is rejected - the name ends up inside
        // `show ip route vrf <name> summary json`.
        assert!(!is_safe_vrf_name(""));
        assert!(!is_safe_vrf_name("default; shutdown"));
        assert!(!is_safe_vrf_name("default\nshutdown"));
        assert!(!is_safe_vrf_name("vrf tenant"));
        assert!(!is_safe_vrf_name(&"x".repeat(65)));
    }

    #[test]
    fn query_errors_dedupe_and_cap() {
        let mut errors = Vec::new();
        add_query_error(&mut errors, Some("bgpd: connect failed".to_string()));
        add_query_error(&mut errors, Some("bgpd: connect failed".to_string()));
        assert_eq!(errors.len(), 1, "identical failures repeat every tick");

        for i in 0..20 {
            add_query_error(&mut errors, Some(format!("error {i}")));
        }
        assert_eq!(errors.len(), 8, "capped so a bad tick can't grow it");
        assert!(add_then_none_keeps_list(&mut errors));
    }

    fn add_then_none_keeps_list(errors: &mut Vec<String>) -> bool {
        add_query_error(errors, None);
        !errors.is_empty()
    }

    #[test]
    fn vrf_all_route_summary_reads_vrf_keyed_maps() {
        let json = r#"{
            "default": {"routesTotal": 30, "routesTotalFib": 28},
            "vrf-tenant1": {"routesTotal": 12, "routesTotalFib": 12}
        }"#;
        let routes = parse_vrf_all_route_summary(json);
        assert_eq!(routes.get("default"), Some(&(30, 28)));
        assert_eq!(routes.get("vrf-tenant1"), Some(&(12, 12)));
    }

    #[test]
    fn vrf_all_route_summary_empty_for_non_vrf_keyed_shapes() {
        // The plain (non-vrf-all) summary shape and a vty error line
        // both parse to "not supported" - the caller falls back to
        // per-VRF queries on exactly this.
        assert!(
            parse_vrf_all_route_summary(r#"{"routesTotal": 30, "routesTotalFib": 28}"#).is_empty()
        );
        assert!(parse_vrf_all_route_summary("% Unknown command").is_empty());
    }

    #[test]
    fn peer_routes_parse_paths_aspath_origin_and_bestpath() {
        let json = r#"{
            "neighborAddress": "198.51.100.10",
            "routes": {
                "10.0.0.0/24": {"destinations": 1, "paths": [
                    {"aspath": {"string": "65001"}, "origin": "IGP",
                     "bestpath": {"overall": true}}
                ]},
                "10.128.0.0/16": {"paths": [
                    {"aspath": {"string": "65001 65100"}, "origin": "IGP"}
                ]}
            }
        }"#;
        let (prefixes, error) = parse_peer_routes(json);
        assert!(error.is_none());
        assert_eq!(prefixes.len(), 2);
        // Numeric run comparison: 10.128 sorts after 10.0, not before.
        assert_eq!(prefixes[0].prefix, "10.0.0.0/24");
        assert_eq!(prefixes[1].prefix, "10.128.0.0/16");
        assert!(prefixes[0].best);
        assert_eq!(prefixes[0].aspath.as_deref(), Some("65001"));
        assert_eq!(prefixes[1].aspath.as_deref(), Some("65001 65100"));
        assert!(!prefixes[1].best);
        assert_eq!(prefixes[0].origin.as_deref(), Some("IGP"));
    }

    #[test]
    fn peer_routes_empty_is_a_valid_answer() {
        // The case this whole view exists for: session up, nothing being
        // advertised - an empty Adj-RIB-Out, not an error.
        let (prefixes, error) = parse_peer_routes(r#"{"routes": {}}"#);
        assert!(prefixes.is_empty() && error.is_none());

        let (prefixes, error) = parse_peer_routes(r#"{"routes": null}"#);
        assert!(prefixes.is_empty() && error.is_none());

        let (prefixes, error) = parse_peer_routes("");
        assert!(prefixes.is_empty() && error.is_none());
    }

    #[test]
    fn peer_routes_vty_errors_surface_as_errors() {
        // A peer with no session in this AFI (or an older FRR without
        // the command) answers with a `% ...` line, not JSON - bgpd's
        // own words become the diagnosis.
        let (prefixes, error) =
            parse_peer_routes("% Neighbor 198.51.100.10 has no session in this AFI\n");
        assert!(prefixes.is_empty());
        assert!(error.is_some());
        assert!(error.unwrap().contains("no session"));
    }

    #[test]
    fn peer_routes_schema_drift_is_reported() {
        let (prefixes, error) = parse_peer_routes(r#"{"unexpected": 1}"#);
        assert!(prefixes.is_empty());
        assert_eq!(
            error.as_deref(),
            Some("peer-routes: response has no routes object - schema drift?")
        );

        let (prefixes, error) = parse_peer_routes(r#"{"routes": [1, 2]}"#);
        assert!(prefixes.is_empty());
        assert!(error.is_some());
    }

    #[test]
    fn peer_routes_tolerate_a_bare_routes_object() {
        let json = r#"{"10.0.0.0/24": {"paths": [{"aspath": {"string": "65001"}}]}}"#;
        let (prefixes, error) = parse_peer_routes(json);
        assert!(error.is_none());
        assert_eq!(prefixes.len(), 1);
        assert_eq!(prefixes[0].prefix, "10.0.0.0/24");
        assert_eq!(prefixes[0].aspath.as_deref(), Some("65001"));
        // No origin/bestpath fields at all: degrades to None/false.
        assert_eq!(prefixes[0].origin, None);
        assert!(!prefixes[0].best);
    }

    #[test]
    fn prefix_sorting_is_numeric_not_lexicographic() {
        let mut v = vec![
            "10.32.0.0/16",
            "10.128.0.0/16",
            "10.0.0.0/24",
            "192.168.0.0/16",
        ];
        v.sort_by(|a, b| cmp_prefix(a, b));
        assert_eq!(
            v,
            vec![
                "10.0.0.0/24",
                "10.32.0.0/16",
                "10.128.0.0/16",
                "192.168.0.0/16"
            ]
        );
    }

    #[test]
    fn afi_names_map_to_vty_command_tokens() {
        assert_eq!(afi_to_cli("ipv4Unicast"), Some("ipv4 unicast"));
        assert_eq!(afi_to_cli("ipv6Unicast"), Some("ipv6 unicast"));
        assert_eq!(afi_to_cli("l2vpnEvpn"), Some("l2vpn evpn"));
        assert_eq!(afi_to_cli("nonsense"), None);
    }

    #[test]
    fn peer_addresses_are_validated_before_interpolation_into_queries() {
        assert!(is_safe_peer_addr("198.51.100.10"));
        assert!(is_safe_peer_addr("2001:db8:100::1"));

        assert!(!is_safe_peer_addr(""));
        assert!(!is_safe_peer_addr(&"x".repeat(46)));
        assert!(!is_safe_peer_addr("198.51.100.10 password x"));
        assert!(!is_safe_peer_addr("10.0.0.1;shutdown"));
        assert!(!is_safe_peer_addr("10.0.0.1\nshutdown"));
    }

    // FRR wraps the per-direction JSON under "advertisedRoutes"/
    // "receivedRoutes" (not "routes") - the live VM's "response has no
    // routes object - schema drift?" came from exactly this.
    #[test]
    fn peer_routes_parse_the_frr_direction_wrapper_keys() {
        let adv = r#"{"advertisedRoutes": {"10.0.0.0/24": {"paths": [
            {"aspath": {"string": "65001"}, "origin": "IGP",
             "bestpath": {"overall": true}}]}}}"#;
        let (prefixes, error) = parse_peer_routes(adv);
        assert!(error.is_none());
        assert_eq!(prefixes.len(), 1);
        assert_eq!(prefixes[0].prefix, "10.0.0.0/24");
        assert_eq!(prefixes[0].aspath.as_deref(), Some("65001"));

        let rcvd = r#"{"receivedRoutes": {"2001:db8::/64": {"paths": [
            {"aspath": {"string": "4200113170 65002"}}]}}}"#;
        let (prefixes, error) = parse_peer_routes(rcvd);
        assert!(error.is_none());
        assert_eq!(prefixes.len(), 1);
        assert_eq!(prefixes[0].prefix, "2001:db8::/64");
        assert_eq!(prefixes[0].aspath.as_deref(), Some("4200113170 65002"));
    }

    #[test]
    fn fmt_uptime_buckets() {
        assert_eq!(fmt_uptime(45), "45s");
        assert_eq!(fmt_uptime(125), "2m 5s");
        assert_eq!(fmt_uptime(7260), "2h 1m");
        assert_eq!(fmt_uptime(90000), "1d 1h");
    }
}
