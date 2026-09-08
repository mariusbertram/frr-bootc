//! FRR status via `vtysh`. There's no stable library API to link against,
//! so this talks to the same CLI a human would, exactly like the bash
//! version of this dashboard did.

use std::process::Command;

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
    let route_summary = parse_route_summary(&vtysh("show ip route summary"));
    let bgp_vrf_peers = if daemons.split_whitespace().any(|d| d == "bgpd") {
        parse_bgp_vrf_summary(&vtysh("show bgp vrf all summary"))
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

// "show ip route summary" ends with a "Totals   <rib>   <fib>" line.
fn parse_route_summary(out: &str) -> Option<String> {
    for line in out.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() == Some("Totals") {
            let rib = fields.next()?;
            let fib = fields.last().unwrap_or(rib);
            return Some(format!("{rib} routes, {fib} FIB"));
        }
    }
    None
}

/// Established-vs-configured BGP peer count per VRF - each tenant gets its
/// own VRF and its own BGP instance (see "VRF per Tenant" in the README),
/// so a single global peer count would hide one tenant's session being
/// down behind another's being fine.
///
/// Neighbor rows in `show bgp vrf all summary` are identified by column 2
/// (the BGP version, always "4"), stable across FRR releases. Which column
/// holds "State/PfxRcd" is NOT stable, though - newer FRR appends a
/// trailing PfxSnt (and on some versions a Desc) column after it, which
/// would make the last column always numeric and every peer look
/// established - so the State/PfxRcd column index is read from each VRF
/// section's own "Neighbor ..." header line instead of assumed. A peer
/// counts as established when that column is numeric (a received-prefix
/// count) rather than a state word like Idle/Active/Connect/OpenSent.
fn parse_bgp_vrf_summary(out: &str) -> Vec<(String, u32, u32)> {
    let mut order: Vec<String> = Vec::new();
    let mut counts: std::collections::HashMap<String, (u32, u32)> =
        std::collections::HashMap::new();

    let mut vrf = "default".to_string();
    let mut state_col: Option<usize> = None;

    for line in out.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.is_empty() {
            continue;
        }

        // A VRF section boundary. This has to scan for a "VRF" token
        // anywhere in the line rather than assuming it's fields[0]: real
        // FRR (unlike the older "VRF <name> (VRF id N):" header this
        // used to assume) reports it mid-line instead, e.g. "BGP router
        // identifier ..., local AS number ... VRF vrf-bdbos vrf-id 3" -
        // every VRF including the default one, so there's no separate
        // "Default"-prefixed line to special-case either. Assuming
        // fields[0] here silently left every VRF's peers folded into
        // "default" (never finding a section boundary at all) - the
        // exact bug this scan fixes.
        if let Some(pos) = fields.iter().position(|f| *f == "VRF") {
            vrf = fields
                .get(pos + 1)
                .map(|s| s.split('(').next().unwrap_or(s).to_string())
                .unwrap_or_else(|| "default".to_string());
            state_col = None;
            continue;
        }
        if fields[0] == "Neighbor" {
            state_col = fields.iter().position(|f| *f == "State/PfxRcd");
            continue;
        }
        // A neighbor row: column index 1 (0-based) is the BGP version,
        // always "4".
        if fields.get(1) != Some(&"4") {
            continue;
        }

        let entry = counts.entry(vrf.clone()).or_insert_with(|| {
            order.push(vrf.clone());
            (0, 0)
        });
        entry.1 += 1;
        let col = state_col.unwrap_or(fields.len() - 1);
        if fields
            .get(col)
            .is_some_and(|f| f.chars().all(|c| c.is_ascii_digit()))
        {
            entry.0 += 1;
        }
    }

    order
        .into_iter()
        .map(|v| {
            let (estab, total) = counts[&v];
            (v, estab, total)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_summary_reads_rib_and_fib() {
        let out = "AFI/SAFI ipv4-unicast\nType  Id  ...\nTotals               10          8\n";
        assert_eq!(
            parse_route_summary(out).as_deref(),
            Some("10 routes, 8 FIB")
        );
    }

    #[test]
    fn route_summary_none_when_missing() {
        assert_eq!(parse_route_summary("nothing here\n"), None);
    }

    // Modern FRR appends a PfxSnt column after State/PfxRcd, which is
    // always numeric - if the parser assumed the last column instead of
    // locating "State/PfxRcd" by name, an Idle peer would be miscounted
    // as established here (this is the exact bug the column lookup
    // guards against - see the doc comment above parse_bgp_vrf_summary).
    #[test]
    fn bgp_vrf_summary_uses_named_state_column_not_last_column() {
        let out = "\
VRF vrf-tenant1 (VRF id 1):
BGP router identifier 10.0.0.1, local AS number 65001 vrf-id 1
Neighbor        V         AS   MsgRcvd   MsgSent   TblVer  InQ OutQ  Up/Down State/PfxRcd   PfxSnt
10.100.1.2      4      65101       120       118        0    0    0 01:23:45            5        3
10.100.1.3      4      65102         0         0        0    0    0    never      Active        0

Total number of neighbors 2

VRF vrf-tenant2 (VRF id 2):
Neighbor        V         AS   MsgRcvd   MsgSent   TblVer  InQ OutQ  Up/Down State/PfxRcd   PfxSnt
10.100.2.2      4      65201        50        49        0    0    0 00:12:03            2        1

Total number of neighbors 1

VRF default (VRF id 0):
Neighbor        V         AS   MsgRcvd   MsgSent   TblVer  InQ OutQ  Up/Down State/PfxRcd   PfxSnt
10.0.0.2        4      65001        90        88        0    0    0 00:45:10            1        1
";
        assert_eq!(
            parse_bgp_vrf_summary(out),
            vec![
                ("vrf-tenant1".to_string(), 1, 2),
                ("vrf-tenant2".to_string(), 1, 1),
                ("default".to_string(), 1, 1),
            ]
        );
    }

    // Older FRR without the trailing PfxSnt column - State/PfxRcd is the
    // last column here, so the fallback-to-last-column path (when the
    // header lookup somehow fails) needs to agree with the named lookup.
    #[test]
    fn bgp_vrf_summary_without_pfxsnt_column() {
        let out = "\
VRF vrf-tenant1 (VRF id 1):
Neighbor        V         AS   MsgRcvd   MsgSent   TblVer  InQ OutQ  Up/Down State/PfxRcd
10.100.1.2      4      65101       120       118        0    0    0 01:23:45            5
10.100.1.3      4      65102         0         0        0    0    0    never      Active

Total number of neighbors 2
";
        assert_eq!(
            parse_bgp_vrf_summary(out),
            vec![("vrf-tenant1".to_string(), 1, 2)]
        );
    }

    #[test]
    fn bgp_vrf_summary_empty_when_no_daemons_output() {
        assert!(parse_bgp_vrf_summary("").is_empty());
    }

    // Real FRR (confirmed live, not the fabricated "VRF <name> (VRF id
    // N):" header format the other tests use) never puts "VRF" as
    // fields[0] at all - it's embedded mid-line in the "BGP router
    // identifier" line, for the default VRF too, and each VRF appears
    // once per AFI (IPv4 and IPv6 each get their own header+neighbor
    // table). Assuming fields[0] == "VRF" never found a section
    // boundary here, so every peer - across both VRFs and both AFIs -
    // silently landed under "default" instead of vrf-bdbos getting its
    // own entry.
    #[test]
    fn bgp_vrf_summary_finds_vrf_name_embedded_mid_line() {
        let out = "\
IPv4 Unicast Summary:
BGP router identifier 25.120.26.244, local AS number 65060 VRF default vrf-id 0
BGP table version 1
Peers 8, using 240 KiB of memory

Neighbor        V         AS   MsgRcvd   MsgSent   TblVer  InQ OutQ  Up/Down State/PfxRcd   PfxSnt Desc
*25.120.26.142  4 4200113170        10        10        1    0    0 00:06:32            0        0 N/A
*25.120.26.161  4 4200113170        10        10        1    0    0 00:06:32            0        0 N/A

Total number of neighbors 2
* - dynamic neighbor
2 dynamic neighbor(s), limit 100

IPv6 Unicast Summary:
BGP router identifier 25.120.26.244, local AS number 65060 VRF default vrf-id 0
BGP table version 0
Peers 8, using 240 KiB of memory

Neighbor        V         AS   MsgRcvd   MsgSent   TblVer  InQ OutQ  Up/Down State/PfxRcd   PfxSnt Desc
*25.120.26.142  4 4200113170        10        10        0    0    0 00:06:32            0        0 N/A
*25.120.26.161  4 4200113170        10        10        0    0    0 00:06:32            0        0 N/A

Total number of neighbors 2

IPv4 Unicast Summary:
BGP router identifier 28.172.192.183, local AS number 4200113182 VRF vrf-bdbos vrf-id 3
BGP table version 20
Peers 2, using 60 KiB of memory

Neighbor        V         AS   MsgRcvd   MsgSent   TblVer  InQ OutQ  Up/Down State/PfxRcd   PfxSnt Desc
28.172.192.177  4      65002        27        22       20    0    0 00:00:23            1        0 N/A

Total number of neighbors 1

IPv6 Unicast Summary:
BGP router identifier 28.172.192.183, local AS number 4200113182 VRF vrf-bdbos vrf-id 3
BGP table version 5
Peers 2, using 60 KiB of memory

Neighbor               V         AS   MsgRcvd   MsgSent   TblVer  InQ OutQ  Up/Down State/PfxRcd   PfxSnt Desc
2a02:110d:9020:1055::1 4      65002        18        13        5    0    0 00:00:23            1        0 N/A

Total number of neighbors 1
";
        assert_eq!(
            parse_bgp_vrf_summary(out),
            vec![
                ("default".to_string(), 4, 4),
                ("vrf-bdbos".to_string(), 2, 2),
            ]
        );
    }
}
