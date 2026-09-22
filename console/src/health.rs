//! The one-glance "is this router actually okay" verdict, aggregated
//! from everything the dashboard already gathers. Every panel answers
//! "what is the state of X"; this answers the question an on-call
//! engineer actually opens the console with - and it's the same
//! aggregation the plain-text/`--json` modes report and base their exit
//! code on, so a scripted check and the on-screen badge can never
//! disagree about how healthy the VM is.

use serde::Serialize;

use crate::snapshot::Snapshot;
use crate::systemd::SyncHealth;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Level {
    Ok,
    Warn,
    Critical,
}

impl Level {
    /// ratatui color for this level (kept here rather than in ui.rs so
    /// both the badge and any per-problem coloring agree).
    pub fn color(self) -> ratatui::style::Color {
        match self {
            Level::Ok => ratatui::style::Color::Green,
            Level::Warn => ratatui::style::Color::Yellow,
            Level::Critical => ratatui::style::Color::Red,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Problem {
    pub level: Level,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    pub level: Level,
    pub problems: Vec<Problem>,
}

impl Verdict {
    /// Compact header-badge text: "healthy", or the level plus how many
    /// findings at that level - the details stay in their panels, the
    /// badge only has to say whether looking is necessary.
    pub fn badge(&self) -> String {
        if self.level == Level::Ok {
            return "healthy".to_string();
        }
        let n = self
            .problems
            .iter()
            .filter(|p| p.level == self.level)
            .count();
        let word = match self.level {
            Level::Ok => unreachable!("handled above"),
            Level::Warn => "WARN",
            Level::Critical => "CRIT",
        };
        format!("{word} ({n})")
    }
}

/// Aggregates the snapshot into a verdict. Deliberately conservative
/// about what counts: only things that are unambiguously a fault (a down
/// FRR, a failed sync, BGP/BFD sessions down, saturation, a pending
/// reboot) escalate the level - an interface being DOWN, a 0/0 VRF or a
/// RIB/FIB wobble stay panel-level information, so the badge can be
/// trusted to mean "go look" rather than "something is cosmetically
/// off".
pub fn assess(snap: &Snapshot) -> Verdict {
    let mut problems = Vec::new();

    if !snap.frr_service_active {
        problems.push(Problem {
            level: Level::Critical,
            text: "FRR service inactive - this VM is not routing".to_string(),
        });
    }

    for unit in &snap.sync_units {
        match &unit.health {
            SyncHealth::Failed { reason } => problems.push(Problem {
                level: Level::Critical,
                text: format!("{}: {reason}", unit.label),
            }),
            SyncHealth::TimerNotActive => problems.push(Problem {
                level: Level::Warn,
                text: format!("{}: timer not active", unit.label),
            }),
            SyncHealth::Ok { .. } => {}
        }
    }

    // Sessions down, aggregated across VRFs: enumerated per VRF up to a
    // few, then rolled up - a fleet-scale incident (150 tenants flapping)
    // must produce one readable line, not 150.
    let down_vrfs: Vec<String> = snap
        .frr
        .bgp_vrf_peers
        .iter()
        .filter(|&&(_, estab, total)| total > 0 && estab < total)
        .map(|&(ref vrf, estab, total)| format!("{vrf} {estab}/{total}"))
        .collect();
    if !down_vrfs.is_empty() {
        let down_total: u32 = snap
            .frr
            .bgp_vrf_peers
            .iter()
            .filter(|&&(_, _, total)| total > 0)
            .map(|&(_, estab, total)| total - estab)
            .sum();
        let named = down_vrfs.iter().take(3).cloned().collect::<Vec<_>>();
        let rest = down_vrfs.len() - named.len();
        let mut text = format!("BGP: {down_total} session(s) down: {}", named.join(", "));
        if rest > 0 {
            text.push_str(&format!(" (+{rest} more VRFs)"));
        }
        problems.push(Problem {
            level: Level::Critical,
            text,
        });
    }

    let bfd_down = snap
        .frr
        .bfd_peers
        .iter()
        .filter(|p| p.status.eq_ignore_ascii_case("down"))
        .count();
    if bfd_down > 0 {
        problems.push(Problem {
            level: Level::Warn,
            text: format!("BFD: {bfd_down} session(s) down - affected links are failing"),
        });
    }

    // Same thresholds the memory/disk gauges color by (see ui.rs's
    // gauge_color), so the badge and the gauges can't disagree.
    if let Some(mem) = &snap.memory {
        let percent = (mem.used as f64 / mem.total as f64 * 100.0).round() as u16;
        if let Some(problem) = saturation_problem("Memory", percent) {
            problems.push(problem);
        }
    }
    if let Some(disk) = &snap.disk {
        if let Some(problem) = saturation_problem("Disk", u16::from(disk.percent_used)) {
            problems.push(problem);
        }
    }

    if snap.bootc.as_ref().is_some_and(|b| b.reboot_pending()) {
        problems.push(Problem {
            level: Level::Warn,
            text: "reboot pending - a staged bootc update is not booted yet".to_string(),
        });
    }

    let level = problems.iter().map(|p| p.level).max().unwrap_or(Level::Ok);
    Verdict { level, problems }
}

fn saturation_problem(what: &str, percent: u16) -> Option<Problem> {
    let (level, verb) = match percent {
        0..=79 => return None,
        80..=89 => (Level::Warn, "high"),
        _ => (Level::Critical, "critically"),
    };
    Some(Problem {
        level,
        text: format!("{what} {percent}% used - {verb} saturated"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frr::{BfdPeerDetail, BgpPeerDetail, FrrStatus};
    use crate::net::Interface;
    use crate::snapshot::{BootcStatus, SyncUnit};
    use crate::systemd::SyncHealth;
    use std::collections::BTreeMap;

    fn base_snapshot() -> Snapshot {
        Snapshot {
            hostname: "test".to_string(),
            now: "now".to_string(),
            uptime: "0d 0h 0m".to_string(),
            load_average: "0.00 0.00 0.00".to_string(),
            cpu_count: 1,
            memory: None,
            disk: None,
            interfaces: Vec::new(),
            selected_interface_detail: None,
            frr_service_active: true,
            frr: FrrStatus {
                daemons: String::new(),
                route_summary: None,
                bgp_vrf_peers: Vec::new(),
                bgp_vrf_peer_detail: BTreeMap::new(),
                vrf_routes: BTreeMap::new(),
                bfd_peers: Vec::new(),
                query_errors: Vec::new(),
            },
            sync_units: Vec::new(),
            bootc: None,
            traffic: None,
            selected_sync_journal: None,
        }
    }

    #[test]
    fn all_green_is_ok() {
        let snap = base_snapshot();
        let verdict = assess(&snap);
        assert_eq!(verdict.level, Level::Ok);
        assert_eq!(verdict.badge(), "healthy");
    }

    #[test]
    fn inactive_frr_is_critical() {
        let mut snap = base_snapshot();
        snap.frr_service_active = false;
        assert_eq!(assess(&snap).level, Level::Critical);
    }

    #[test]
    fn down_bgp_sessions_are_critical_and_roll_up() {
        let mut snap = base_snapshot();
        snap.frr.bgp_vrf_peers = vec![
            ("vrf-tenant1".to_string(), 1, 2),
            ("vrf-tenant2".to_string(), 0, 0), // no configured peers - not a fault
            ("vrf-tenant3".to_string(), 4, 4),
        ];
        let verdict = assess(&snap);
        assert_eq!(verdict.level, Level::Critical);
        let bgp = verdict
            .problems
            .iter()
            .find(|p| p.text.starts_with("BGP:"))
            .unwrap();
        assert!(bgp.text.contains("1 session(s) down"));
        assert!(bgp.text.contains("vrf-tenant1 1/2"));
        assert!(!bgp.text.contains("vrf-tenant2"));
    }

    #[test]
    fn down_bgp_enumeration_caps_at_three_vrfs() {
        let mut snap = base_snapshot();
        snap.frr.bgp_vrf_peers = (0..5).map(|i| (format!("vrf-tenant{i}"), 0, 1)).collect();
        let verdict = assess(&snap);
        let bgp = verdict
            .problems
            .iter()
            .find(|p| p.text.starts_with("BGP:"))
            .unwrap();
        assert!(bgp.text.contains("(+2 more VRFs)"), "{}", bgp.text);
    }

    #[test]
    fn failed_sync_is_critical_timer_warning_is_not() {
        let mut snap = base_snapshot();
        snap.sync_units = vec![
            SyncUnit {
                label: "FRR config sync",
                service: "frr-config-sync.service",
                health: SyncHealth::Failed {
                    reason: "status stale (12m ago)".to_string(),
                },
            },
            SyncUnit {
                label: "bootc image sync",
                service: "bootc-image-sync.service",
                health: SyncHealth::TimerNotActive,
            },
        ];
        let verdict = assess(&snap);
        assert_eq!(verdict.level, Level::Critical);
        assert_eq!(verdict.badge(), "CRIT (1)");
        assert!(verdict
            .problems
            .iter()
            .any(|p| p.level == Level::Warn && p.text.contains("timer not active")));
    }

    #[test]
    fn saturation_escalates_at_the_gauge_thresholds() {
        let mut snap = base_snapshot();
        snap.disk = Some(crate::system::DiskInfo {
            used: 85,
            total: 100,
            percent_used: 85,
        });
        assert_eq!(assess(&snap).level, Level::Warn);
        snap.disk.as_mut().unwrap().percent_used = 95;
        assert_eq!(assess(&snap).level, Level::Critical);
    }

    #[test]
    fn staged_update_and_down_bfd_are_warnings() {
        let mut snap = base_snapshot();
        snap.bootc = Some(BootcStatus {
            booted_digest: Some("sha256:aaa".to_string()),
            staged_digest: Some("sha256:bbb".to_string()),
            ..Default::default()
        });
        snap.frr.bfd_peers = vec![BfdPeerDetail {
            peer: "198.51.100.1".to_string(),
            vrf: "vrf-tenant1".to_string(),
            status: "down".to_string(),
            uptime: None,
        }];
        let verdict = assess(&snap);
        assert_eq!(verdict.level, Level::Warn);
        assert_eq!(verdict.badge(), "WARN (2)");
    }

    // Guards the "conservative about what counts" contract: an admin-down
    // interface, an idle BGP VRF and a healthy-but-old sync must never
    // turn the badge.
    #[test]
    fn panel_level_states_do_not_escalate() {
        let mut snap = base_snapshot();
        snap.interfaces = vec![Interface {
            name: "tenant9".to_string(),
            up: false,
            addrs: vec![],
            rate: None,
            err_rate: None,
        }];
        snap.frr.bgp_vrf_peers = vec![("vrf-idle".to_string(), 0, 0)];
        snap.frr.bgp_vrf_peer_detail = BTreeMap::from([(
            "vrf-idle".to_string(),
            vec![BgpPeerDetail {
                peer: "203.0.113.1".to_string(),
                afi: "ipv4Unicast".to_string(),
                state: "Idle".to_string(),
                remote_as: None,
                uptime: None,
                pfx_rcd: "N/A".to_string(),
                pfx_snt: "N/A".to_string(),
            }],
        )]);
        snap.sync_units = vec![SyncUnit {
            label: "FRR config sync",
            service: "frr-config-sync.service",
            health: SyncHealth::Ok { age_secs: 600 },
        }];
        snap.frr.bfd_peers = vec![BfdPeerDetail {
            peer: "198.51.100.1".to_string(),
            vrf: "vrf-idle".to_string(),
            status: "up".to_string(),
            uptime: None,
        }];
        assert_eq!(assess(&snap).level, Level::Ok);
    }
}
