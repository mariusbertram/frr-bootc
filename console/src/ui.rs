//! Renders one `Snapshot` as an actual TUI - bordered panels, a table for
//! interfaces/sync services, gauges for memory/disk - instead of one big
//! block of plain text. `ratatui::Terminal::draw` diffs each frame against
//! the last one and only touches the terminal cells that actually
//! changed, which is the real payoff regardless of layout complexity: a
//! shrinking section (an interface losing its address line, a VRF's BGP
//! peers disappearing, ...) can never leave stale content on screen the
//! way it did with the bash version's manual cursor/erase-sequence
//! bookkeeping - ratatui owns the whole screen buffer, there's nothing to
//! "leave behind".

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Gauge, Paragraph, Row, Table};
use ratatui::Frame;

use crate::net::fmt_rate;
use crate::snapshot::Snapshot;
use crate::system::fmt_bytes;
use crate::systemd::SyncHealth;

const OK: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;
const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

fn panel(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(MUTED))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

fn status(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(color))
}

fn gauge_color(percent: u16) -> Color {
    match percent {
        0..=69 => OK,
        70..=89 => WARN,
        _ => BAD,
    }
}

pub fn render(frame: &mut Frame, snap: &Snapshot, refresh_secs: u64) {
    let image_height = snap
        .bootc_status
        .as_ref()
        .map_or(0, |s| s.lines().count() as u16 + 2);

    // Network Interfaces and FRR are the two panels whose natural content
    // size varies a lot (tenant count, VRF count - this repo's own README
    // uses "150 BGP-coupled tenants" as its scaling example), so they get
    // `Fill` instead of a precomputed `Length`: whatever's actually left
    // after the fixed-size panels below, split 3:2 favoring interfaces.
    // Precomputing a "desired" Length for both here (an earlier version
    // of this did) doesn't work: when their combined desired height
    // exceeds what's actually available, ratatui's constraint solver
    // distributes the shortfall across whichever Length constraints it
    // sees fit - not necessarily the ones expected - silently dropping
    // rows with no visible sign anything's missing. Using the *real*
    // allocated Rect each panel function receives to decide how many
    // rows it can show (see interfaces_table/frr_panel below) means the
    // "+N more" indicator is always accurate to what's actually on
    // screen, because it's driven by the same number.
    let mut constraints = vec![
        Constraint::Length(3), // header
        Constraint::Length(3), // stats row
        Constraint::Fill(3),   // network interfaces
        Constraint::Fill(2),   // FRR
        Constraint::Length(6), // sync services (3 rows + header + border)
    ];
    if image_height > 0 {
        constraints.push(Constraint::Length(image_height));
    }
    constraints.push(Constraint::Length(1)); // footer

    let areas = Layout::vertical(constraints).split(frame.area());
    let mut idx = 0;
    let mut next = || {
        let area = areas[idx];
        idx += 1;
        area
    };

    header(frame, next(), snap);
    stats_row(frame, next(), snap);
    interfaces_table(frame, next(), snap);
    frr_panel(frame, next(), snap);
    sync_table(frame, next(), snap);
    if image_height > 0 {
        image_panel(frame, next(), snap);
    }
    footer(frame, next(), refresh_secs);
}

fn header(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT));
    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", snap.hostname),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("frr-bootc router console", Style::default().fg(MUTED)),
        Span::raw("   "),
        Span::raw(&snap.now),
        Span::raw("   uptime "),
        Span::raw(&snap.uptime),
    ]);
    frame.render_widget(Paragraph::new(line).block(block), area);
}

fn stats_row(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let [load_area, mem_area, disk_area] = Layout::horizontal([
        Constraint::Percentage(34),
        Constraint::Percentage(33),
        Constraint::Percentage(33),
    ])
    .areas(area);

    let load_line = Line::from(format!(
        " {}   ({} CPUs)",
        snap.load_average, snap.cpu_count
    ));
    frame.render_widget(
        Paragraph::new(load_line).block(panel("Load (1/5/15m)")),
        load_area,
    );

    render_gauge(
        frame,
        mem_area,
        "Memory",
        snap.memory.as_ref().map(|m| {
            (
                (m.used as f64 / m.total as f64 * 100.0).round() as u16,
                format!("{} / {}", fmt_bytes(m.used), fmt_bytes(m.total)),
            )
        }),
    );
    render_gauge(
        frame,
        disk_area,
        "Disk (/)",
        snap.disk.as_ref().map(|d| {
            (
                u16::from(d.percent_used),
                format!("{} / {}", fmt_bytes(d.used), fmt_bytes(d.total)),
            )
        }),
    );
}

fn render_gauge(frame: &mut Frame, area: Rect, title: &str, data: Option<(u16, String)>) {
    let block = panel(title);
    match data {
        Some((percent, label)) => {
            let percent = percent.min(100);
            let gauge = Gauge::default()
                .block(block)
                .gauge_style(Style::default().fg(gauge_color(percent)))
                .ratio(f64::from(percent) / 100.0)
                .label(format!("{percent}%  {label}"));
            frame.render_widget(gauge, area);
        }
        None => frame.render_widget(Paragraph::new(" unknown").block(block), area),
    }
}

// How many interfaces (from the front of the list) fit in `budget` data
// rows, given each takes 1 row normally or 2 if it has an address to
// show on its own row below - and, unless every one of them fits, 1 more
// row is reserved for a trailing "+N more" line so that line itself
// never has to bump something else off to make room. budget == 0 means
// there's no room even for that line, so nothing is shown at all rather
// than a "+N more" that would just get silently clipped the same way the
// interfaces it's supposed to stand in for did.
fn interfaces_visible(snap: &Snapshot, budget: usize) -> (usize, usize) {
    let total = snap.interfaces.len();
    if budget == 0 {
        return (0, 0);
    }
    let cost = |i: usize| {
        if snap.interfaces[i].addrs.is_empty() {
            1
        } else {
            2
        }
    };
    let natural: usize = (0..total).map(cost).sum();
    if natural <= budget {
        return (total, 0);
    }
    // total > 0 here (natural, a sum of per-item costs of >=1 each, can
    // only exceed budget if there's at least one item) - 0..total is
    // never empty, so this always returns from inside the loop: at
    // shown=0, used=0 < budget (budget >= 1, checked above) always holds.
    for shown in (0..total).rev() {
        let used: usize = (0..shown).map(cost).sum();
        if used < budget {
            return (shown, total - shown);
        }
    }
    (0, total)
}

fn interfaces_table(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    // area.height includes the block's top/bottom border and the
    // table's own header row - what's left is real data-row capacity.
    let budget = area.height.saturating_sub(3) as usize;
    let rows: Vec<Row> = if snap.interfaces.is_empty() {
        vec![Row::new(vec![Cell::from("(none)")])]
    } else {
        let (shown, extra) = interfaces_visible(snap, budget);
        let extra_row = (extra > 0).then(|| {
            Row::new(vec![Cell::from(Span::styled(
                format!("... +{extra} more"),
                Style::default().fg(MUTED),
            ))])
        });
        snap.interfaces[..shown]
            .iter()
            .flat_map(|iface| {
                let (state_text, color) = if iface.up { ("UP", OK) } else { ("DOWN", BAD) };
                let rate = match iface.rate {
                    Some((rx, tx)) => format!("rx {} / tx {}", fmt_rate(rx), fmt_rate(tx)),
                    None => "-".to_string(),
                };
                let main = Row::new(vec![
                    Cell::from(iface.name.clone()),
                    Cell::from(Span::styled(state_text, Style::default().fg(color))),
                    Cell::from(rate),
                ]);
                let addr = (!iface.addrs.is_empty()).then(|| {
                    Row::new(vec![
                        Cell::default(),
                        Cell::default(),
                        Cell::from(Span::styled(
                            iface.addrs.join(" "),
                            Style::default().fg(MUTED),
                        )),
                    ])
                });
                std::iter::once(main).chain(addr)
            })
            .chain(extra_row)
            .collect()
    };

    let table = Table::new(
        rows,
        [
            Constraint::Length(18),
            Constraint::Length(8),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(["Interface", "State", "Throughput / Address"])
            .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)),
    )
    .block(panel("Network Interfaces"));
    frame.render_widget(table, area);
}

// Real deployments can have a lot of VRFs (this repo's own README uses
// "150 BGP-coupled tenants" as its scaling example) - showing them all
// unconditionally would run this panel off the bottom of a normal-sized
// screen. `budget` is however many peer rows are actually available -
// driven by the real allocated Rect (see frr_panel), the same reasoning
// as interfaces_visible above. budget == 0 means not even one more row
// fits (frr_panel doesn't call this unless there's room for at least the
// "BGP peers" heading plus one line under it, so this only returns
// (0, 0) in that case rather than ever needing to reserve space for a
// "+N more" line that wouldn't fit either).
fn bgp_visible_rows(total: usize, budget: usize) -> (usize, usize) {
    if budget == 0 {
        (0, 0)
    } else if total <= budget {
        (total, 0)
    } else {
        (budget - 1, total - (budget - 1))
    }
}

fn frr_panel(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let mut lines = Vec::new();
    let (state_text, color) = if snap.frr_service_active {
        ("active", OK)
    } else {
        ("inactive", BAD)
    };
    lines.push(Line::from(vec![
        Span::raw("frr.service: "),
        status(state_text, color),
    ]));
    if snap.frr_service_active {
        if !snap.frr.daemons.is_empty() {
            lines.push(Line::from(format!("daemons: {}", snap.frr.daemons)));
        }
        if let Some(routes) = &snap.frr.route_summary {
            lines.push(Line::from(format!("IPv4 RIB: {routes}")));
        }
        if !snap.frr.bgp_vrf_peers.is_empty() {
            // Whole "BGP peers" section (heading + rows) has to fit in
            // whatever's left after frr.service/daemons/routes above and
            // the block's own top/bottom border - computed before the
            // heading itself is pushed, so a too-tight fit can collapse
            // to one combined line (or nothing) instead of drawing a
            // heading over rows that then get silently clipped.
            let section_budget = (area.height as usize)
                .saturating_sub(2)
                .saturating_sub(lines.len());
            let total_vrfs = snap.frr.bgp_vrf_peers.len();
            if section_budget >= 2 {
                lines.push(Line::from(Span::styled(
                    "BGP peers (per VRF):",
                    Style::default().fg(MUTED),
                )));
                let (shown, extra) = bgp_visible_rows(total_vrfs, section_budget - 1);
                for (vrf, estab, total) in snap.frr.bgp_vrf_peers.iter().take(shown) {
                    let color = if *estab == *total {
                        OK
                    } else if *estab == 0 {
                        BAD
                    } else {
                        WARN
                    };
                    lines.push(Line::from(vec![
                        Span::raw(format!("  {vrf:<20} ")),
                        status(format!("{estab}/{total} established"), color),
                    ]));
                }
                if extra > 0 {
                    lines.push(Line::styled(
                        format!("  ... +{extra} more"),
                        Style::default().fg(MUTED),
                    ));
                }
            } else if section_budget == 1 {
                lines.push(Line::styled(
                    format!("BGP peers: {total_vrfs} VRF(s), no room to list"),
                    Style::default().fg(MUTED),
                ));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines).block(panel("FRR")), area);
}

fn sync_table(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let rows: Vec<Row> = snap
        .sync_units
        .iter()
        .map(|unit| match &unit.health {
            SyncHealth::Failed => Row::new(vec![
                Cell::from(unit.label),
                Cell::from(Span::styled("FAILED", Style::default().fg(BAD))),
                Cell::from(Span::styled(
                    format!("journalctl -u {}", unit.service),
                    Style::default().fg(MUTED),
                )),
            ]),
            SyncHealth::TimerNotActive => Row::new(vec![
                Cell::from(unit.label),
                Cell::from(Span::styled("timer not active", Style::default().fg(WARN))),
                Cell::default(),
            ]),
            SyncHealth::Ok => Row::new(vec![
                Cell::from(unit.label),
                Cell::from(Span::styled("ok", Style::default().fg(OK))),
                Cell::default(),
            ]),
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(22),
            Constraint::Length(18),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(["Sync Service", "Status", "Detail"])
            .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)),
    )
    .block(panel("Sync Services"));
    frame.render_widget(table, area);
}

fn image_panel(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let Some(status) = &snap.bootc_status else {
        return;
    };
    let lines: Vec<Line> = status.lines().map(Line::from).collect();
    frame.render_widget(
        Paragraph::new(lines).block(panel("System Image (bootc)")),
        area,
    );
}

fn footer(frame: &mut Frame, area: Rect, refresh_secs: u64) {
    let line = Line::from(vec![
        Span::raw(format!("Refreshing every {refresh_secs}s   ")),
        Span::styled(
            "[b]",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" log in for a shell"),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}
