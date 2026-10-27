//! Renders the TUI to image files (SVG, one per scene) instead of a real
//! terminal - a build-pipeline artifact, so the dashboard's layout can be
//! looked at (and iterated on) without a VM: `frr-console --screenshots
//! <dir>` writes one SVG per scene below into `<dir>/`. Uses the same
//! `render` entry point and `TestBackend` the unit tests draw through,
//! so the screenshots show exactly what ships - a bug that only exists
//! in this path can't exist, there is no second renderer.
//!
//! SVG rather than PNG on purpose: no image codec dependency, the file
//! stays diffable-ish text, and terminal cells map 1:1 to SVG rects
//! (background) + text (glyphs) - colors carry over, and GitHub renders
//! SVGs in PRs and artifact zips inline in the browser.

use std::fs;
use std::path::Path;

use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::style::Color;
use ratatui::Terminal;

use crate::events::EventLog;
use crate::frr::{BfdPeerDetail, BgpPeerDetail, PeerRoutes, RouteDirection};
use crate::health::Level;
use crate::net::{Interface, Traffic};
use crate::snapshot::{BootcStatus, Snapshot, SyncUnit};
use crate::system::{DiskInfo, MemInfo};
use crate::systemd::SyncHealth;
use crate::ui::{render, AppState, Tab};

const TERMINAL_DEFAULT: Color = Color::Reset;

/// One named frame to capture: a snapshot of demo data, the state the
/// UI should be in while looking at it, and the terminal size to render
/// at (80x24 = the serial-console floor this dashboard must work at).
struct Scene {
    name: &'static str,
    width: u16,
    height: u16,
    snap: Snapshot,
    state: AppState,
    log: EventLog,
}

/// The demo data - shaped after this repo's own deployment reality (a
/// dual-stack tenant with an established session, one with a downed
/// session, BFD, sync services, a staged update) rather than abstract
/// filler, so the screenshots answer real layout questions: do the
/// panels line up at 80 columns, does the VRF popup fit, is the
/// problem tenant's red row actually leading the list.
fn demo_snapshot() -> Snapshot {
    let interfaces = vec![
        Interface {
            name: "eth-trunk".to_string(),
            up: true,
            addrs: Vec::new(),
            rate: Some((24_500_000, 18_200_000)),
            err_rate: Some((0, 0)),
        },
        Interface {
            name: "tenant1".to_string(),
            up: true,
            addrs: vec!["198.51.100.2/30".to_string()],
            rate: Some((1_800_000, 940_000)),
            err_rate: None,
        },
        Interface {
            name: "tenant2".to_string(),
            up: true,
            addrs: vec!["203.0.113.2/30".to_string()],
            rate: Some((420_000, 310_000)),
            err_rate: Some((2, 0)),
        },
        Interface {
            name: "eth-lan".to_string(),
            up: true,
            addrs: vec!["10.0.2.1/24".to_string()],
            rate: Some((3_100_000, 2_700_000)),
            err_rate: None,
        },
    ];

    let mut bgp_vrf_peer_detail = std::collections::BTreeMap::new();
    bgp_vrf_peer_detail.insert(
        "vrf-tenant1".to_string(),
        vec![
            BgpPeerDetail {
                peer: "198.51.100.1".to_string(),
                afi: "ipv4Unicast".to_string(),
                state: "Established".to_string(),
                remote_as: Some(65000),
                uptime: Some("03:22:41".to_string()),
                pfx_rcd: "14".to_string(),
                pfx_snt: "2".to_string(),
            },
            BgpPeerDetail {
                peer: "198.51.100.1".to_string(),
                afi: "ipv6Unicast".to_string(),
                state: "Established".to_string(),
                remote_as: Some(65000),
                uptime: Some("03:22:41".to_string()),
                pfx_rcd: "3".to_string(),
                pfx_snt: "1".to_string(),
            },
        ],
    );
    bgp_vrf_peer_detail.insert(
        "vrf-tenant2".to_string(),
        vec![BgpPeerDetail {
            peer: "203.0.113.1".to_string(),
            afi: "ipv4Unicast".to_string(),
            state: "Idle".to_string(),
            remote_as: Some(65100),
            uptime: None,
            pfx_rcd: "N/A".to_string(),
            pfx_snt: "N/A".to_string(),
        }],
    );

    Snapshot {
        hostname: "frr-rz01-0".to_string(),
        now: "12:04:33".to_string(),
        uptime: "14d 3h 12m".to_string(),
        load_average: "0.42 0.51 0.48".to_string(),
        cpu_count: 16,
        memory: Some(MemInfo {
            used: 9_663_675_902,
            total: 34_359_738_368,
            available: 22_486_572_032,
        }),
        disk: Some(DiskInfo {
            used: 14_312_958_565,
            total: 128_849_018_880,
            percent_used: 12,
        }),
        interfaces,
        selected_interface_detail: None,
        frr_service_active: true,
        frr: crate::frr::FrrStatus {
            daemons: "bgpd bfdd mgmtd staticd zebra".to_string(),
            route_summary: Some("84 routes, 84 FIB".to_string()),
            bgp_vrf_peers: vec![
                ("vrf-tenant1".to_string(), 2, 2),
                ("vrf-tenant2".to_string(), 0, 1),
            ],
            bgp_vrf_peer_detail,
            vrf_routes: std::collections::BTreeMap::from([
                ("vrf-tenant1".to_string(), (22, 22)),
                ("vrf-tenant2".to_string(), (5, 3)),
            ]),
            bfd_peers: vec![
                BfdPeerDetail {
                    peer: "198.51.100.1".to_string(),
                    vrf: "vrf-tenant1".to_string(),
                    status: "up".to_string(),
                    uptime: Some("3h 22m".to_string()),
                },
                BfdPeerDetail {
                    peer: "203.0.113.1".to_string(),
                    vrf: "vrf-tenant2".to_string(),
                    status: "down".to_string(),
                    uptime: Some("47s".to_string()),
                },
            ],
            query_errors: Vec::new(),
        },
        sync_units: vec![
            SyncUnit {
                label: "FRR config sync",
                service: "frr-config-sync.service",
                health: SyncHealth::Ok { age_secs: 30 },
            },
            SyncUnit {
                label: "Network config sync",
                service: "network-config-sync.service",
                health: SyncHealth::Failed {
                    reason: "status stale (6m ago)".to_string(),
                },
            },
            SyncUnit {
                label: "bootc image sync",
                service: "bootc-image-sync.service",
                health: SyncHealth::Ok { age_secs: 300 },
            },
        ],
        bootc: Some(BootcStatus {
            booted_image: Some("ghcr.io/mariusbertram/frr-bootc:latest".to_string()),
            booted_digest: Some("sha256:1a2b3c4d5e6f7890".to_string()),
            staged_image: Some("ghcr.io/mariusbertram/frr-bootc:latest".to_string()),
            staged_digest: Some("sha256:9f8e7d6c5b4a3928".to_string()),
            raw: None,
        }),
        traffic: Some(Traffic {
            name: "eth-trunk".to_string(),
            series: (0..64)
                .map(|i| {
                    let base = 20_000_000u64 + 6_000_000 * ((i as f64 / 5.0).sin().abs()) as u64;
                    (base, base * 3 / 4)
                })
                .collect(),
        }),
        selected_sync_journal: None,
        selected_peer_routes: None,
    }
}

/// A log with a few realistic entries - the Events tab should look like
/// a log, not an empty panel.
fn demo_log() -> EventLog {
    let mut log = EventLog::new(100, None);
    log.record(
        Level::Warn,
        "12:01:58",
        "bgp vrf-tenant2 203.0.113.1 ipv4Unicast: Established -> Idle",
    );
    log.record(
        Level::Warn,
        "12:01:58",
        "bfd vrf-tenant2 203.0.113.1: up -> down",
    );
    log.record(Level::Ok, "11:47:12", "interface eth-trunk came UP");
    log.record(
        Level::Warn,
        "09:12:44",
        "Network config sync: FAILED - status stale (6m ago)",
    );
    log
}

/// All scenes, in the order they should be read: the two main tabs
/// first, then the drill-in popups (the layout-sensitive ones), then
/// the narrow-terminal cases that most need eyeballing.
fn scenes() -> Vec<Scene> {
    let snap = demo_snapshot();
    let log = demo_log();

    let mut overview_state = AppState::default();
    overview_state.set_tab(Tab::Overview);
    let overview = Scene {
        name: "01-overview",
        width: 100,
        height: 34,
        snap: snap.clone(),
        state: overview_state,
        log: EventLog::new(100, None),
    };

    let mut frr_state = AppState::default();
    frr_state.set_tab(Tab::Frr);
    frr_state.toggle_detail();
    let frr_popup = Scene {
        name: "02-frr-vrf-popup",
        width: 100,
        height: 34,
        snap: snap.clone(),
        state: frr_state,
        log: EventLog::new(100, None),
    };

    // The advertised-prefixes popup - the BPATG-question view. The
    // routes data must match the highlighted peer (vrf-tenant1's first
    // peer after sorting) or the popup renders as "collecting".
    let mut adv_state = AppState::default();
    adv_state.set_tab(Tab::Frr);
    adv_state.toggle_detail();
    adv_state.open_routes(RouteDirection::Advertised);
    let mut adv_snap = snap.clone();
    // The popup's subject is whatever peer sorts first (the not-
    // Established one leads) - the routes data must match it or the
    // popup renders as "collecting".
    adv_snap.selected_peer_routes = Some(PeerRoutes {
        direction: RouteDirection::Advertised,
        vrf: "vrf-tenant2".to_string(),
        peer: "203.0.113.1".to_string(),
        afi: "ipv4Unicast".to_string(),
        prefixes: vec![
            crate::frr::BgpPeerPrefix {
                prefix: "10.0.0.0/24".to_string(),
                aspath: None,
                origin: Some("IGP".to_string()),
                best: true,
            },
            crate::frr::BgpPeerPrefix {
                prefix: "10.128.0.0/16".to_string(),
                aspath: Some("65001 65100".to_string()),
                origin: Some("IGP".to_string()),
                best: true,
            },
            crate::frr::BgpPeerPrefix {
                prefix: "192.168.23.0/24".to_string(),
                aspath: Some("65001".to_string()),
                origin: Some("EGP".to_string()),
                best: false,
            },
        ],
        error: None,
    });
    let adv_popup = Scene {
        name: "03-frr-advertised-popup",
        width: 100,
        height: 34,
        snap: adv_snap,
        state: adv_state,
        log: EventLog::new(100, None),
    };

    let mut events_state = AppState::default();
    events_state.set_tab(Tab::Events);
    let events = Scene {
        name: "04-events",
        width: 100,
        height: 34,
        snap: snap.clone(),
        state: events_state,
        log,
    };

    // The serial-console floor: 80x24, the size that most often breaks
    // layouts (footer hint tiers, panel truncation) - captured for both
    // the Overview and the FRR popup.
    let mut narrow_state = AppState::default();
    narrow_state.set_tab(Tab::Overview);
    let narrow_overview = Scene {
        name: "05-overview-80x24",
        width: 80,
        height: 24,
        snap: snap.clone(),
        state: narrow_state,
        log: EventLog::new(100, None),
    };

    let mut narrow_frr_state = AppState::default();
    narrow_frr_state.set_tab(Tab::Frr);
    narrow_frr_state.toggle_detail();
    let narrow_frr = Scene {
        name: "06-frr-vrf-popup-80x24",
        width: 80,
        height: 24,
        snap: snap.clone(),
        state: narrow_frr_state,
        log: EventLog::new(100, None),
    };

    vec![
        overview,
        frr_popup,
        adv_popup,
        events,
        narrow_overview,
        narrow_frr,
    ]
}

/// Renders every scene and writes one SVG per scene into `dir`.
/// Returns the number of files written, for a one-line stdout summary.
pub fn render_all(dir: &Path) -> usize {
    let _ = fs::create_dir_all(dir);
    let mut written = 0;
    for scene in scenes() {
        let backend = TestBackend::new(scene.width, scene.height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = scene.state;
        let snap = &scene.snap;
        let log = &scene.log;
        terminal
            .draw(|frame| render(frame, Some(snap), log, &mut state, 5, Some(2)))
            .unwrap();
        let svg = buffer_to_svg(terminal.backend().buffer(), scene.name);
        let path = dir.join(format!("{}.svg", scene.name));
        if fs::write(&path, svg).is_ok() {
            written += 1;
        }
    }
    written
}

/// Cell width + height in SVG units. 8x16 keeps roughly the classic
/// terminal aspect ratio; glyph baselines are placed at 12 units.
const CELL_W: f64 = 8.0;
const CELL_H: f64 = 16.0;

/// Converts a rendered `Buffer` into a standalone SVG document - one
/// `<text>` element per cell that has a symbol, plus one background
/// rect per cell whose background differs from the terminal default
/// (a rect for literally-every cell would bloat the file without a
/// visual difference; popups' `Clear`ed areas look right because the
/// popup panel draws its own background via its border/title spans).
fn buffer_to_svg(buf: &Buffer, name: &str) -> String {
    let area = buf.area;
    let mut out = String::new();
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" \
         viewBox=\"0 0 {w} {h}\" font-family=\"monospace\" font-size=\"13\">\n",
        w = (area.width as f64 * CELL_W) as u32,
        h = (area.height as f64 * CELL_H) as u32,
    ));
    out.push_str(&format!(
        "  <title>frr-console - {name}</title>\n  <rect width=\"100%\" height=\"100%\" fill=\"{bg}\"/>\n",
        bg = svg_color(TERMINAL_DEFAULT, false)
    ));

    for y in 0..area.height {
        for x in 0..area.width {
            let cell: &Cell = &buf[(x, y)];
            let bg = cell.bg;
            let rect_x = x as f64 * CELL_W;
            let rect_y = y as f64 * CELL_H;
            if bg != TERMINAL_DEFAULT {
                out.push_str(&format!(
                    "  <rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{}\"/>\n",
                    rect_x,
                    rect_y,
                    CELL_W,
                    CELL_H,
                    svg_color(bg, false)
                ));
            }
            let symbol = cell.symbol();
            if symbol != " " && !symbol.is_empty() {
                out.push_str(&format!(
                    "  <text x=\"{}\" y=\"{}\" fill=\"{}\">{}</text>\n",
                    rect_x,
                    rect_y + 12.0,
                    svg_color(cell.fg, true),
                    xml_escape(symbol)
                ));
            }
        }
    }
    out.push_str("</svg>\n");
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A ratatui `Color` to an SVG/CSS hex color - the 16 ANSI colors get
/// their classic terminal palette values so the screenshots read like
/// the real console (green/red/yellow for states), the 256-indexed and
/// RGB colors their literal values.
fn svg_color(color: Color, _fg: bool) -> String {
    match color {
        Color::Reset => "#1e1e2e".to_string(),
        Color::Black => "#45475a".to_string(),
        Color::Red => "#f38ba8".to_string(),
        Color::Green => "#a6e3a1".to_string(),
        Color::Yellow => "#f9e2af".to_string(),
        Color::Blue => "#89b4fa".to_string(),
        Color::Magenta => "#f5c2e7".to_string(),
        Color::Cyan => "#89dceb".to_string(),
        Color::Gray => "#9399b2".to_string(),
        Color::DarkGray => "#6c7086".to_string(),
        Color::LightRed => "#f38ba8".to_string(),
        Color::LightGreen => "#a6e3a1".to_string(),
        Color::LightBlue => "#89b4fa".to_string(),
        Color::LightYellow => "#f9e2af".to_string(),
        Color::LightMagenta => "#f5c2e7".to_string(),
        Color::LightCyan => "#89dceb".to_string(),
        Color::White => "#bac2de".to_string(),
        Color::Indexed(i) => format!("#{:06x}", 0x1000000 + i as u32 * 0x010101),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}
