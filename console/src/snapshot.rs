//! Pulls one full dashboard's worth of data together. Called once per
//! refresh tick; nothing here is cached beyond what `net::ThroughputSampler`
//! needs to compute a rate (and the bootc status cache the caller keeps -
//! see `main`'s gather loop).

use std::process::Command;

use serde::Serialize;

use crate::frr::{self, FrrStatus};
use crate::net::{Interface, InterfaceDetail, ThroughputSampler};
use crate::system::{self, DiskInfo, MemInfo};
use crate::systemd::{self, SyncHealth, SyncKind};

#[derive(Debug, Clone, Serialize)]
pub struct SyncUnit {
    pub label: &'static str,
    pub service: &'static str,
    pub health: SyncHealth,
}

/// The `bootc` state behind the System Image panel, parsed rather than
/// shown as raw text when possible: "is a staged update waiting for the
/// operator's reboot decision" is the one fact here that changes what an
/// SRE *does* next (schedule the rolling reboot), and it deserves to be
/// a first-class signal (health verdict, event log) instead of a line
/// buried in CLI output.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BootcStatus {
    pub booted_image: Option<String>,
    pub booted_digest: Option<String>,
    pub staged_image: Option<String>,
    pub staged_digest: Option<String>,
    /// Raw `bootc status` output - the System Image panel's fallback
    /// whenever the structured parse came up empty, so the panel never
    /// has *less* information than before this struct existed.
    pub raw: Option<String>,
}

impl BootcStatus {
    /// A staged update is present but not booted - the "reboot required"
    /// signal. Keyed on the staged *digest* specifically: a staged
    /// entry without a digest isn't verifiable enough to nag about.
    pub fn reboot_pending(&self) -> bool {
        self.staged_digest.is_some()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub hostname: String,
    pub now: String,
    pub uptime: String,
    pub load_average: String,
    pub cpu_count: usize,
    pub memory: Option<MemInfo>,
    pub disk: Option<DiskInfo>,
    pub interfaces: Vec<Interface>,
    /// Extra detail (MTU, MAC, VRF/master, cumulative counters) for
    /// whichever single interface the console's "drill in" popup is
    /// currently open on - `None` when no such popup is open, or the
    /// selected interface's name doesn't match anything `net.rs` could
    /// find (e.g. it just disappeared). Gathered on-demand for this one
    /// interface only, not for all of them every tick - see
    /// `net::InterfaceDetail`'s own doc comment for why.
    pub selected_interface_detail: Option<InterfaceDetail>,
    pub frr_service_active: bool,
    pub frr: FrrStatus,
    pub sync_units: Vec<SyncUnit>,
    pub bootc: Option<BootcStatus>,
    /// The trunk's rolling throughput series for the Overview traffic
    /// graph - `None` until the sampler has two samples to diff.
    pub traffic: Option<crate::net::Traffic>,
    /// Last journal lines for the sync service whose journal popup is
    /// open on the Overview tab (`None` when none is open or journalctl
    /// failed) - gathered on demand for this one unit only, same shape
    /// as `selected_interface_detail`.
    pub selected_sync_journal: Option<String>,
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

/// What the gather loop should read *extra* data for this tick - the
/// two on-demand popups' subjects, `None` whenever the popup in
/// question is closed so no extra reads happen on ordinary ticks.
#[derive(Default, Clone)]
pub struct Selection {
    pub interface: Option<String>,
    pub sync_service: Option<String>,
}

/// `selection`: whatever popups are currently open (see `Selection`) -
/// both are only gathered when actually open, never on ordinary ticks.
pub fn gather(
    net: &mut ThroughputSampler,
    selection: &Selection,
    bootc: Option<BootcStatus>,
) -> Snapshot {
    let frr_service_active = systemd::is_active("frr.service");
    let interfaces = net.list();
    let selected_interface_detail = selection
        .interface
        .as_deref()
        .filter(|name| interfaces.iter().any(|iface| iface.name == *name))
        .map(crate::net::detail);
    let selected_sync_journal = selection
        .sync_service
        .as_deref()
        .and_then(|service| systemd::recent_journal(service, 40));

    Snapshot {
        hostname: system::hostname(),
        now: system::now_local(),
        uptime: system::uptime(),
        load_average: system::load_average(),
        cpu_count: system::cpu_count(),
        memory: system::memory(),
        disk: system::disk_usage("/"),
        interfaces,
        selected_interface_detail,
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
        // `bootc` is passed in by the caller rather than queried here:
        // it changes on the scale of minutes/days, so it's cached across
        // ticks (see main's gather loop) instead of spawning a fresh
        // `bootc status` every 5s.
        bootc,
        traffic: net.traffic(),
        selected_sync_journal,
    }
}

/// Queries bootc once: structured via `--json` when that works (which
/// fields the dashboard actually reasons about - staged vs booted -
/// come from the parse), falling back to plain `bootc status` text for
/// the panel. Both subprocess calls fully capture their output, like
/// every other one here.
pub fn bootc_status() -> Option<BootcStatus> {
    if let Some(parsed) = bootc_status_json() {
        return Some(parsed);
    }
    let out = Command::new("bootc")
        .arg("status")
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&out.stdout);
    Some(BootcStatus {
        raw: Some(text.lines().take(8).collect::<Vec<_>>().join("\n")),
        ..Default::default()
    })
}

/// `bootc status --json`'s relevant slice: top-level `booted`/`staged`
/// objects, each with an `image` object carrying the image reference and
/// its digest. Navigated through the raw `Value` rather than a strict
/// struct on purpose - extra/misnamed fields anywhere else in the
/// document then can't fail the whole parse; anything this can't find
/// yields `None` fields and the text fallback still covers the panel.
fn bootc_status_json() -> Option<BootcStatus> {
    let out = Command::new("bootc")
        .args(["status", "--json"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;

    let entry = |key: &str| -> Option<(Option<String>, Option<String>)> {
        let entry = value.get(key)?;
        if entry.is_null() {
            return None;
        }
        let image = entry.get("image")?;
        Some((
            image
                .get("image")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            image
                .get("digest")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        ))
    };

    let (booted_image, booted_digest) = entry("booted")?;
    let (staged_image, staged_digest) = entry("staged").unwrap_or((None, None));
    Some(BootcStatus {
        booted_image,
        booted_digest,
        staged_image,
        staged_digest,
        raw: None,
    })
}
