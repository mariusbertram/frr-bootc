//! State-change detection and the event log. The dashboard used to show
//! only the *current* state - the answer to "since when?" and "what
//! changed while nobody was watching?" existed nowhere on this VM (no
//! alerting, no log aggregation - see main.rs's module docs), so every
//! postmortem started from zero. Comparing consecutive snapshots here
//! turns the same data the panels already show into a timestamped
//! transition log, held in a bounded ring buffer and appended to
//! `/var/log/frr-console-events.log` so history survives dashboard
//! restarts (the process exits whenever someone uses `b` to log in and
//! comes back via Restart=always).

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::health::Level;
use crate::snapshot::Snapshot;

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// The snapshot's own local-time string at the moment the change was
    /// observed (same format as the header clock) - observation time,
    /// which is what the log can honestly claim; granularity is the
    /// refresh interval.
    pub time: String,
    pub level: Level,
    pub text: String,
}

impl Event {
    fn line(&self) -> String {
        format!("{}\t{}\t{}", self.time, level_word(self.level), self.text)
    }
}

fn level_word(level: Level) -> &'static str {
    match level {
        Level::Ok => "info",
        Level::Warn => "warn",
        Level::Critical => "crit",
    }
}

fn level_from_word(word: &str) -> Option<Level> {
    match word {
        "info" => Some(Level::Ok),
        "warn" => Some(Level::Warn),
        "crit" => Some(Level::Critical),
        _ => None,
    }
}

/// In-memory ring of recent events plus the file they're appended to.
pub struct EventLog {
    events: VecDeque<Event>,
    capacity: usize,
    path: Option<PathBuf>,
}

impl EventLog {
    /// Opens (creating if needed) the persistence file and restores the
    /// tail of the previous log from it, so a dashboard that came back
    /// from a login/reboot still shows what happened before. A missing
    /// or unreadable file is fine - the log just starts empty.
    pub fn new(capacity: usize, path: Option<&Path>) -> Self {
        let events = path
            .and_then(|p| load_file(p, capacity))
            .unwrap_or_default();
        EventLog {
            events,
            capacity,
            path: path.map(Path::to_path_buf),
        }
    }

    pub fn record(&mut self, level: Level, time: &str, text: impl Into<String>) {
        let event = Event {
            time: time.to_string(),
            level,
            text: text.into(),
        };
        if let Some(path) = &self.path {
            append_line(path, &event.line());
        }
        self.events.push_back(event);
        while self.events.len() > self.capacity {
            self.events.pop_front();
        }
    }

    /// Emits an event for every state transition between `old` and
    /// `new`, timestamped with `new`'s own clock line.
    pub fn diff(&mut self, old: Option<&Snapshot>, new: &Snapshot) {
        let Some(old) = old else { return };
        let time = &new.now;

        for iface in &new.interfaces {
            let prev = old.interfaces.iter().find(|i| i.name == iface.name);
            match prev {
                None => {
                    self.record(
                        Level::Ok,
                        time,
                        format!("interface {} appeared", iface.name),
                    );
                }
                Some(prev) => {
                    if prev.up != iface.up {
                        let (level, verb) = if iface.up {
                            (Level::Ok, "came UP")
                        } else {
                            (Level::Warn, "went DOWN")
                        };
                        self.record(level, time, format!("interface {} {verb}", iface.name));
                    }
                    if prev.addrs != iface.addrs {
                        let addrs = if iface.addrs.is_empty() {
                            "(none)".to_string()
                        } else {
                            iface.addrs.join(" ")
                        };
                        self.record(
                            Level::Ok,
                            time,
                            format!("interface {} addresses now: {addrs}", iface.name),
                        );
                    }
                }
            }
        }
        for iface in &old.interfaces {
            if !new.interfaces.iter().any(|i| i.name == iface.name) {
                self.record(
                    Level::Warn,
                    time,
                    format!("interface {} disappeared", iface.name),
                );
            }
        }

        if old.frr_service_active != new.frr_service_active {
            let (level, verb) = if new.frr_service_active {
                (Level::Ok, "started")
            } else {
                (Level::Critical, "STOPPED - this VM is not routing")
            };
            self.record(level, time, format!("frr.service {verb}"));
        }

        // BGP/BFD transitions only compare while FRR is up on both
        // sides: when the service is down the detail maps are empty by
        // design (frr::gather short-circuits), and diffing against that
        // emptiness would emit one spurious "went down" event per peer
        // on service stop - frr.service's own STOPPED event above is
        // the honest report for that case.
        if old.frr_service_active && new.frr_service_active {
            for (vrf, peers) in &new.frr.bgp_vrf_peer_detail {
                let old_peers = old.frr.bgp_vrf_peer_detail.get(vrf);
                for peer in peers {
                    let prev_state = old_peers
                        .and_then(|ps| ps.iter().find(|p| p.peer == peer.peer && p.afi == peer.afi))
                        .map(|p| p.state.as_str());
                    match prev_state {
                        None => {
                            self.record(
                                Level::Ok,
                                time,
                                format!(
                                    "BGP {vrf}: new peer {} ({}) is {}",
                                    peer.peer, peer.afi, peer.state
                                ),
                            );
                        }
                        Some(prev) if prev != peer.state => {
                            let (level, verb) = if peer.state == "Established" {
                                (Level::Ok, "established")
                            } else {
                                (Level::Critical, "went DOWN")
                            };
                            self.record(
                                level,
                                time,
                                format!(
                                    "BGP {vrf}: peer {} ({}) {verb} (was {prev})",
                                    peer.peer, peer.afi
                                ),
                            );
                        }
                        _ => {}
                    }
                }
            }

            for bfd in &new.frr.bfd_peers {
                let prev = old
                    .frr
                    .bfd_peers
                    .iter()
                    .find(|b| b.peer == bfd.peer && b.vrf == bfd.vrf);
                let prev_status = prev.map(|b| b.status.as_str());
                let down = |s: &str| s.eq_ignore_ascii_case("down");
                match (prev_status.map(down), down(&bfd.status)) {
                    (Some(false), true) => self.record(
                        Level::Warn,
                        time,
                        format!("BFD {}: went DOWN ({})", bfd.peer, bfd.vrf),
                    ),
                    (Some(true), false) => self.record(
                        Level::Ok,
                        time,
                        format!("BFD {}: back UP ({})", bfd.peer, bfd.vrf),
                    ),
                    _ => {}
                }
            }
        }

        for unit in &new.sync_units {
            let Some(prev) = old.sync_units.iter().find(|u| u.label == unit.label) else {
                continue;
            };
            if prev.health != unit.health {
                match &unit.health {
                    crate::systemd::SyncHealth::Failed { reason } => self.record(
                        Level::Critical,
                        time,
                        format!("{} FAILED: {reason}", unit.label),
                    ),
                    crate::systemd::SyncHealth::TimerNotActive => self.record(
                        Level::Warn,
                        time,
                        format!("{}: timer not active", unit.label),
                    ),
                    crate::systemd::SyncHealth::Ok { .. } => {
                        self.record(Level::Ok, time, format!("{} recovered", unit.label))
                    }
                }
            }
        }

        // vty transport/schema failures: recorded when an error first
        // appears and when the last one clears - "since when can't the
        // dashboard see bgpd?" is exactly the kind of fact this log
        // exists for. Deliberately not in the health verdict (see
        // health.rs): a bgpd restart blinds the dashboard for seconds,
        // it doesn't break routing.
        for error in &new.frr.query_errors {
            if !old.frr.query_errors.contains(error) {
                self.record(Level::Warn, time, format!("vty query failing: {error}"));
            }
        }
        if !old.frr.query_errors.is_empty() && new.frr.query_errors.is_empty() {
            self.record(Level::Ok, time, "vty queries recovered");
        }

        // Only comparable when both sides parsed; a half-parsed pair
        // (e.g. bootc briefly failing to answer) must not read as
        // "update staged then unstaged".
        if let (Some(old_b), Some(new_b)) = (&old.bootc, &new.bootc) {
            if new_b.staged_digest.is_some() && old_b.staged_digest != new_b.staged_digest {
                self.record(
                    Level::Warn,
                    time,
                    format!(
                        "bootc update staged: {} (reboot required)",
                        new_b.staged_digest.as_deref().unwrap_or("?")
                    ),
                );
            }
            if new_b.booted_digest.is_some() && old_b.booted_digest != new_b.booted_digest {
                self.record(
                    Level::Ok,
                    time,
                    format!(
                        "booted image is now {}",
                        new_b.booted_digest.as_deref().unwrap_or("?")
                    ),
                );
            }
        }
    }

    pub fn events(&self) -> &VecDeque<Event> {
        &self.events
    }

    /// The last `n` events, oldest first - what the plain-text and
    /// `--json` one-shot modes attach to their output.
    pub fn tail(&self, n: usize) -> Vec<&Event> {
        self.events.iter().rev().take(n).rev().collect()
    }
}

fn append_line(path: &Path, line: &str) {
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}

/// Reads back at most `capacity` events from the log file - parse errors
/// on individual lines skip just those lines, not the whole file.
fn load_file(path: &Path, capacity: usize) -> Option<VecDeque<Event>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut events = VecDeque::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut parts = line.splitn(3, '\t');
        let (Some(time), Some(level), Some(text)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Some(level) = level_from_word(level) else {
            continue;
        };
        events.push_back(Event {
            time: time.to_string(),
            level,
            text: text.to_string(),
        });
    }
    while events.len() > capacity {
        events.pop_front();
    }
    Some(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frr::{BgpPeerDetail, FrrStatus};
    use crate::net::Interface;
    use crate::snapshot::{BootcStatus, SyncUnit};
    use crate::systemd::SyncHealth;
    use std::collections::BTreeMap;

    fn snapshot_with(f: impl FnOnce(&mut Snapshot)) -> Snapshot {
        let mut snap = Snapshot {
            hostname: "test".to_string(),
            now: "2026-09-22 12:00:00 UTC".to_string(),
            uptime: "1d".to_string(),
            load_average: "0".to_string(),
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
        };
        f(&mut snap);
        snap
    }

    fn peer(state: &str) -> BgpPeerDetail {
        BgpPeerDetail {
            peer: "198.51.100.10".to_string(),
            afi: "ipv4Unicast".to_string(),
            state: state.to_string(),
            remote_as: None,
            uptime: None,
            pfx_rcd: "-".to_string(),
            pfx_snt: "-".to_string(),
        }
    }

    #[test]
    fn interface_transitions_are_recorded() {
        let mut log = EventLog::new(100, None);
        let old = snapshot_with(|s| {
            s.interfaces = vec![Interface {
                name: "eth-trunk".to_string(),
                up: true,
                addrs: vec!["10.0.0.1/30".to_string()],
                rate: None,
                err_rate: None,
            }];
        });
        let new = snapshot_with(|s| {
            s.interfaces = vec![Interface {
                name: "eth-trunk".to_string(),
                up: false,
                addrs: Vec::new(),
                rate: None,
                err_rate: None,
            }];
        });

        log.diff(Some(&old), &new);

        let texts: Vec<_> = log.events().iter().map(|e| e.text.clone()).collect();
        assert!(texts.iter().any(|t| t.contains("eth-trunk went DOWN")));
        // The address change is its own event - one diff, two findings.
        assert!(texts.iter().any(|t| t.contains("addresses now")));
    }

    #[test]
    fn bgp_flaps_are_critical_recoveries_are_info_and_service_down_diffs_nothing() {
        let mut log = EventLog::new(100, None);
        let with_established = |s: &mut Snapshot| {
            s.frr
                .bgp_vrf_peer_detail
                .insert("vrf-tenant1".to_string(), vec![peer("Established")]);
            s.frr.bgp_vrf_peers = vec![("vrf-tenant1".to_string(), 1, 1)];
        };
        let up = snapshot_with(with_established);
        let down = snapshot_with(|s| {
            s.frr
                .bgp_vrf_peer_detail
                .insert("vrf-tenant1".to_string(), vec![peer("Idle")]);
            s.frr.bgp_vrf_peers = vec![("vrf-tenant1".to_string(), 0, 1)];
        });

        log.diff(Some(&up), &down);
        assert_eq!(log.events().len(), 1);
        assert_eq!(log.events()[0].level, Level::Critical);
        assert!(log.events()[0].text.contains("went DOWN"));

        log.diff(Some(&down), &up);
        let last = log.events().back().unwrap();
        assert_eq!(last.level, Level::Ok);
        assert!(last.text.contains("established"));

        // Service down on either side: peer-level diffing suppressed.
        let mut log = EventLog::new(100, None);
        let stopped = snapshot_with(|s| s.frr_service_active = false);
        log.diff(Some(&up), &stopped);
        assert!(log.events().iter().all(|e| !e.text.starts_with("BGP")));
        assert!(log
            .events()
            .iter()
            .any(|e| e.text.contains("frr.service STOPPED")));
    }

    #[test]
    fn sync_recovery_and_failure_are_recorded() {
        let mut log = EventLog::new(100, None);
        let ok = |s: &mut Snapshot| {
            s.sync_units = vec![SyncUnit {
                label: "FRR config sync",
                service: "frr-config-sync.service",
                health: SyncHealth::Ok { age_secs: 30 },
            }];
        };
        let failed = |s: &mut Snapshot| {
            s.sync_units = vec![SyncUnit {
                label: "FRR config sync",
                service: "frr-config-sync.service",
                health: SyncHealth::Failed {
                    reason: "status stale (6m ago)".to_string(),
                },
            }];
        };

        let old = snapshot_with(ok);
        let new = snapshot_with(failed);
        log.diff(Some(&old), &new);
        assert_eq!(log.events().back().unwrap().level, Level::Critical);
        assert!(log.events().back().unwrap().text.contains("status stale"));

        let new = snapshot_with(ok);
        log.diff(Some(&snapshot_with(failed)), &new);
        let last = log.events().back().unwrap();
        assert_eq!(last.level, Level::Ok);
        assert!(last.text.contains("recovered"));
    }

    #[test]
    fn staged_update_and_reboot_are_recorded_once_each() {
        let mut log = EventLog::new(100, None);
        let bare = snapshot_with(|s| {
            s.bootc = Some(BootcStatus {
                booted_digest: Some("sha256:aaa".to_string()),
                ..Default::default()
            });
        });
        let staged = snapshot_with(|s| {
            s.bootc = Some(BootcStatus {
                booted_digest: Some("sha256:aaa".to_string()),
                staged_digest: Some("sha256:bbb".to_string()),
                ..Default::default()
            });
        });
        let rebooted = snapshot_with(|s| {
            s.bootc = Some(BootcStatus {
                booted_digest: Some("sha256:bbb".to_string()),
                ..Default::default()
            });
        });

        log.diff(Some(&bare), &staged);
        log.diff(Some(&staged), &staged); // unchanged tick: no repeat
        let texts: Vec<_> = log.events().iter().map(|e| e.text.clone()).collect();
        assert_eq!(
            texts.iter().filter(|t| t.contains("staged")).count(),
            1,
            "staging must be recorded exactly once"
        );

        log.diff(Some(&staged), &rebooted);
        assert!(log
            .events()
            .back()
            .unwrap()
            .text
            .contains("booted image is now"));
    }

    #[test]
    fn ring_buffer_and_file_roundtrip() {
        // A unique *file* directly in temp_dir - a subdirectory would
        // need creating first, and OpenOptions on a missing directory
        // silently fails every append below.
        let path = std::env::temp_dir().join(format!(
            "frr-console-events-test-{}-{:?}.log",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        let mut log = EventLog::new(3, Some(&path));
        log.record(Level::Ok, "t1", "one");
        log.record(Level::Warn, "t2", "two");
        log.record(Level::Critical, "t3", "three");
        log.record(Level::Ok, "t4", "four");
        assert_eq!(log.events().len(), 3, "capacity must cap the ring");
        assert_eq!(log.events()[0].text, "two");

        let reloaded = EventLog::new(100, Some(&path));
        assert_eq!(reloaded.events().len(), 4);
        assert_eq!(reloaded.events()[0].text, "one");
        assert_eq!(reloaded.events()[2].level, Level::Critical);

        let _ = std::fs::remove_file(&path);
    }
}
