//! Renders one `Snapshot` as a `Vec<Line>`, drawn as a single `Paragraph`
//! over the frame with a one-line footer below it.
//!
//! Building the whole frame as styled text (instead of a Table/List per
//! section) keeps this close to the previous bash-script dashboard's
//! layout - a deliberate choice, since that layout was already read and
//! approved - while ratatui's `Terminal::draw` still gives the actual
//! payoff: it diffs the new frame against the last one and only touches
//! changed cells, so a shrinking section (an interface losing its address
//! line, a VRF's BGP peers disappearing, ...) can never leave stale
//! content on screen the way it did with the bash version's manual
//! cursor/erase-sequence bookkeeping - ratatui owns the whole screen
//! buffer, there's nothing to "leave behind".

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::net::fmt_rate;
use crate::snapshot::Snapshot;
use crate::system::fmt_bytes;
use crate::systemd::SyncHealth;

const OK: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;

fn heading(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        text.into(),
        Style::default().add_modifier(Modifier::BOLD),
    ))
}

fn rule() -> Line<'static> {
    Line::from("-".repeat(80))
}

fn status(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(color))
}

pub fn render(frame: &mut Frame, snap: &Snapshot, refresh_secs: u64) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());

    let mut lines = Vec::new();
    header(&mut lines, snap);
    system_section(&mut lines, snap);
    interfaces_section(&mut lines, snap);
    frr_section(&mut lines, snap);
    sync_section(&mut lines, snap);
    bootc_section(&mut lines, snap);

    frame.render_widget(Paragraph::new(lines), body);
    frame.render_widget(
        Line::from(format!(
            "Refreshing every {refresh_secs}s - [b] log in for a shell"
        )),
        footer,
    );
}

fn header(lines: &mut Vec<Line<'static>>, snap: &Snapshot) {
    lines.push(heading(format!(
        "frr-bootc - {} - {}",
        snap.hostname, snap.now
    )));
    lines.push(rule());
    lines.push(Line::from(format!("  Uptime: {}", snap.uptime)));
    lines.push(Line::default());
}

fn system_section(lines: &mut Vec<Line<'static>>, snap: &Snapshot) {
    lines.push(heading("System"));
    lines.push(rule());
    lines.push(Line::from(format!(
        "  Load average (1/5/15m): {}   ({} CPUs)",
        snap.load_average, snap.cpu_count
    )));
    lines.push(Line::from(match &snap.memory {
        Some(m) => format!(
            "  Memory: {} used / {} total ({} available)",
            fmt_bytes(m.used),
            fmt_bytes(m.total),
            fmt_bytes(m.available)
        ),
        None => "  Memory: unknown".to_string(),
    }));
    lines.push(Line::from(match &snap.disk {
        Some(d) => format!(
            "  Disk (/): {} used / {} total ({}% used)",
            fmt_bytes(d.used),
            fmt_bytes(d.total),
            d.percent_used
        ),
        None => "  Disk (/): unknown".to_string(),
    }));
    lines.push(Line::default());
}

fn interfaces_section(lines: &mut Vec<Line<'static>>, snap: &Snapshot) {
    lines.push(heading("Network Interfaces"));
    lines.push(rule());
    if snap.interfaces.is_empty() {
        lines.push(Line::from("  (none)"));
    }
    for iface in &snap.interfaces {
        let (state_text, color) = if iface.up { ("UP", OK) } else { ("DOWN", BAD) };
        let rate = match iface.rate {
            Some((rx, tx)) => format!("rx {} / tx {}", fmt_rate(rx), fmt_rate(tx)),
            None => "throughput: -".to_string(),
        };
        lines.push(Line::from(vec![
            Span::raw(format!("  {:<16} ", iface.name)),
            status(format!("{state_text:<9}"), color),
            Span::raw(format!(" {rate}")),
        ]));
        if !iface.addrs.is_empty() {
            lines.push(Line::from(format!(
                "  {:<16} {}",
                "",
                iface.addrs.join(" ")
            )));
        }
    }
    lines.push(Line::default());
}

fn frr_section(lines: &mut Vec<Line<'static>>, snap: &Snapshot) {
    lines.push(heading("FRR"));
    lines.push(rule());
    let (state_text, color) = if snap.frr_service_active {
        ("active", OK)
    } else {
        ("inactive", BAD)
    };
    lines.push(Line::from(vec![
        Span::raw("  frr.service: "),
        status(state_text, color),
    ]));
    if snap.frr_service_active {
        if !snap.frr.daemons.is_empty() {
            lines.push(Line::from(format!(
                "  Active daemons: {}",
                snap.frr.daemons
            )));
        }
        if let Some(routes) = &snap.frr.route_summary {
            lines.push(Line::from(format!("  IPv4 RIB: {routes}")));
        }
        if !snap.frr.bgp_vrf_peers.is_empty() {
            lines.push(Line::from("  BGP peers (per VRF):"));
            for (vrf, estab, total) in &snap.frr.bgp_vrf_peers {
                let color = if *estab == *total {
                    OK
                } else if *estab == 0 {
                    BAD
                } else {
                    WARN
                };
                lines.push(Line::from(vec![
                    Span::raw(format!("    {vrf:<20} ")),
                    status(format!("{estab}/{total} established"), color),
                ]));
            }
        }
    }
    lines.push(Line::default());
}

fn sync_section(lines: &mut Vec<Line<'static>>, snap: &Snapshot) {
    lines.push(heading("Sync Services"));
    lines.push(rule());
    for unit in &snap.sync_units {
        let line = match unit.health {
            SyncHealth::Failed => Line::from(vec![
                Span::raw(format!("  {:<24} ", unit.label)),
                status("FAILED", BAD),
                Span::raw(format!("  (see: journalctl -u {})", unit.service)),
            ]),
            SyncHealth::TimerNotActive => Line::from(vec![
                Span::raw(format!("  {:<24} ", unit.label)),
                status("timer not active", WARN),
            ]),
            SyncHealth::Ok => Line::from(vec![
                Span::raw(format!("  {:<24} ", unit.label)),
                status("ok", OK),
            ]),
        };
        lines.push(line);
    }
    lines.push(Line::default());
}

fn bootc_section(lines: &mut Vec<Line<'static>>, snap: &Snapshot) {
    if let Some(status) = &snap.bootc_status {
        lines.push(heading("System Image (bootc)"));
        lines.push(rule());
        for line in status.lines() {
            lines.push(Line::from(format!("  {line}")));
        }
        lines.push(Line::default());
    }
}
