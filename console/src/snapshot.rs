//! Pulls one full dashboard's worth of data together. Called once per
//! refresh tick; nothing here is cached beyond what `net::ThroughputSampler`
//! needs to compute a rate.

use std::process::Command;

use crate::frr::{self, FrrStatus};
use crate::net::{Interface, ThroughputSampler};
use crate::system::{self, DiskInfo, MemInfo};
use crate::systemd::{self, SyncHealth, SyncKind};

pub struct SyncUnit {
    pub label: &'static str,
    pub service: &'static str,
    pub health: SyncHealth,
}

pub struct Snapshot {
    pub hostname: String,
    pub now: String,
    pub uptime: String,
    pub load_average: String,
    pub cpu_count: usize,
    pub memory: Option<MemInfo>,
    pub disk: Option<DiskInfo>,
    pub interfaces: Vec<Interface>,
    pub frr_service_active: bool,
    pub frr: FrrStatus,
    pub sync_units: Vec<SyncUnit>,
    pub bootc_status: Option<String>,
}

/// frr-config-sync/network-config-sync are persistent daemons now (no
/// `.timer` - see the Containerfile's top comment), health comes from
/// whether the process is up plus the status file it writes after every
/// attempt. bootc-image-sync is still the older oneshot+`.timer` shape.
const SYNC_UNITS: &[(&str, &str, SyncKind)] = &[
    (
        "FRR config sync",
        "frr-config-sync.service",
        SyncKind::Daemon {
            status_file: "/run/frr-config-sync.status",
        },
    ),
    (
        "Network config sync",
        "network-config-sync.service",
        SyncKind::Daemon {
            status_file: "/run/network-config-sync.status",
        },
    ),
    (
        "bootc image sync",
        "bootc-image-sync.service",
        SyncKind::Timer {
            timer: "bootc-image-sync.timer",
        },
    ),
];

pub fn gather(net: &mut ThroughputSampler) -> Snapshot {
    let frr_service_active = systemd::is_active("frr.service");

    Snapshot {
        hostname: system::hostname(),
        now: system::now_local(),
        uptime: system::uptime(),
        load_average: system::load_average(),
        cpu_count: system::cpu_count(),
        memory: system::memory(),
        disk: system::disk_usage("/"),
        interfaces: net.list(),
        frr_service_active,
        frr: frr::gather(frr_service_active),
        sync_units: SYNC_UNITS
            .iter()
            .map(|(label, service, kind)| SyncUnit {
                label,
                service,
                health: systemd::sync_health(service, kind),
            })
            .collect(),
        bootc_status: bootc_status(),
    }
}

fn bootc_status() -> Option<String> {
    let out = Command::new("bootc").arg("status").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(text.lines().take(8).collect::<Vec<_>>().join("\n"))
}
