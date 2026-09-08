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
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Gauge, Paragraph, Row, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Table, TableState, Tabs,
};
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

/// Overview is the original single-screen layout (compact, "+N more"
/// truncated) - Interfaces/Frr are full, scrollable listings of the same
/// data with nothing left out, for whenever there's more to look at than
/// "+N more" wants to say (this repo's own scaling example is "150
/// BGP-coupled tenants" - see the README's "A Trunk Instead of One NIC
/// per Tenant" section).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tab {
    #[default]
    Overview,
    Interfaces,
    Frr,
}

const TABS: [Tab; 3] = [Tab::Overview, Tab::Interfaces, Tab::Frr];

impl Tab {
    fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Interfaces => "Interfaces",
            Tab::Frr => "FRR / BGP",
        }
    }

    fn index(self) -> usize {
        TABS.iter().position(|t| *t == self).unwrap_or(0)
    }

    fn next(self) -> Tab {
        TABS[(self.index() + 1) % TABS.len()]
    }

    fn prev(self) -> Tab {
        TABS[(self.index() + TABS.len() - 1) % TABS.len()]
    }
}

/// Lives across redraws (unlike `Snapshot`, which is rebuilt from
/// scratch every refresh) - which tab is active, and how far scrolled
/// into each of the two detail tabs' own listing. A scroll position is
/// kept per-tab rather than shared so that switching tabs and back
/// doesn't lose your place in either one.
#[derive(Default)]
pub struct AppState {
    pub tab: Tab,
    interfaces_scroll: u16,
    frr_scroll: u16,
}

impl AppState {
    pub fn next_tab(&mut self) {
        self.tab = self.tab.next();
    }

    pub fn prev_tab(&mut self) {
        self.tab = self.tab.prev();
    }

    pub fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
    }

    fn scroll_mut(&mut self) -> Option<&mut u16> {
        match self.tab {
            Tab::Overview => None,
            Tab::Interfaces => Some(&mut self.interfaces_scroll),
            Tab::Frr => Some(&mut self.frr_scroll),
        }
    }

    /// A no-op on Overview, which has nothing to scroll - callers don't
    /// need to check which tab is active first.
    pub fn scroll_by(&mut self, delta: i16) {
        let Some(scroll) = self.scroll_mut() else {
            return;
        };
        *scroll = if delta >= 0 {
            scroll.saturating_add(delta as u16)
        } else {
            scroll.saturating_sub(delta.unsigned_abs())
        };
    }

    pub fn scroll_to_top(&mut self) {
        if let Some(scroll) = self.scroll_mut() {
            *scroll = 0;
        }
    }

    /// The real maximum only becomes known once the content's actual
    /// length is available at render time (see interfaces_detail/
    /// frr_detail, which clamp down to it) - u16::MAX here just means
    /// "as far as it goes", relying on that clamp rather than duplicating
    /// its arithmetic.
    pub fn scroll_to_bottom(&mut self) {
        if let Some(scroll) = self.scroll_mut() {
            *scroll = u16::MAX;
        }
    }
}

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

pub fn render(frame: &mut Frame, snap: &Snapshot, state: &mut AppState, refresh_secs: u64) {
    let image_height = snap
        .bootc_status
        .as_ref()
        .map_or(0, |s| s.lines().count() as u16 + 2);

    // Network Interfaces and FRR are the two panels whose natural content
    // size varies a lot (tenant count, VRF count - this repo's own README
    // uses "150 BGP-coupled tenants" as its scaling example), so on the
    // Overview tab they get `Fill` instead of a precomputed `Length`:
    // whatever's actually left after the fixed-size panels below, split
    // 3:2 favoring interfaces. Precomputing a "desired" Length for both
    // here (an earlier version of this did) doesn't work: when their
    // combined desired height exceeds what's actually available,
    // ratatui's constraint solver distributes the shortfall across
    // whichever Length constraints it sees fit - not necessarily the
    // ones expected - silently dropping rows with no visible sign
    // anything's missing. Using the *real* allocated Rect each panel
    // function receives to decide how many rows it can show (see
    // interfaces_table/frr_panel below) means the "+N more" indicator is
    // always accurate to what's actually on screen, because it's driven
    // by the same number. The Interfaces/Frr tabs sidestep the whole
    // question by not truncating at all - they scroll instead (see
    // interfaces_detail/frr_detail).
    let mut constraints = vec![
        Constraint::Length(3), // header
        Constraint::Length(1), // tab bar
    ];
    match state.tab {
        Tab::Overview => {
            constraints.push(Constraint::Length(3)); // stats row
            constraints.push(Constraint::Fill(3)); // network interfaces
            constraints.push(Constraint::Fill(2)); // FRR
            constraints.push(Constraint::Length(6)); // sync services
            if image_height > 0 {
                constraints.push(Constraint::Length(image_height));
            }
        }
        Tab::Interfaces | Tab::Frr => constraints.push(Constraint::Fill(1)),
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
    tab_bar(frame, next(), state.tab);

    match state.tab {
        Tab::Overview => {
            stats_row(frame, next(), snap);
            interfaces_table(frame, next(), snap);
            frr_panel(frame, next(), snap);
            sync_table(frame, next(), snap);
            if image_height > 0 {
                image_panel(frame, next(), snap);
            }
        }
        Tab::Interfaces => interfaces_detail(frame, next(), snap, &mut state.interfaces_scroll),
        Tab::Frr => frr_detail(frame, next(), snap, &mut state.frr_scroll),
    }

    footer(frame, next(), state.tab, refresh_secs);
}

fn tab_bar(frame: &mut Frame, area: Rect, active: Tab) {
    let tabs = Tabs::new(TABS.iter().map(|t| t.title()))
        .select(active.index())
        .style(Style::default().fg(MUTED))
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .divider(" ");
    frame.render_widget(tabs, area);
}

/// A visual scroll indicator on the right edge of `area` - omitted
/// entirely (rather than drawn at 0%/100%) when everything already fits,
/// since a scrollbar implying there's more to see when there isn't would
/// be actively misleading.
fn render_scrollbar(frame: &mut Frame, area: Rect, total_rows: u16, visible_rows: u16, pos: u16) {
    if total_rows <= visible_rows {
        return;
    }
    let mut state = ScrollbarState::new(total_rows as usize)
        .position(pos as usize)
        .viewport_content_length(visible_rows as usize);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None);
    frame.render_stateful_widget(scrollbar, area, &mut state);
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
                    // A VRF can have a BGP instance but zero configured
                    // neighbors - shown (unlike the old text-table
                    // parser, which never produced an entry for it at
                    // all) rather than hidden, but "0/0" isn't a healthy
                    // "OK" so much as "nothing to report" - MUTED reads
                    // that way, where green would misleadingly imply
                    // every configured peer (there are none) is up.
                    let color = if *total == 0 {
                        MUTED
                    } else if *estab == *total {
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

/// Every interface, no truncation - the Overview tab's compact,
/// "+N more" table is `interfaces_table` above; this is its scrollable
/// counterpart for when that's not enough.
fn interfaces_detail(frame: &mut Frame, area: Rect, snap: &Snapshot, scroll: &mut u16) {
    let rows: Vec<Row> = if snap.interfaces.is_empty() {
        vec![Row::new(vec![Cell::from("(none)")])]
    } else {
        snap.interfaces
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
            .collect()
    };

    // area.height includes the block's top/bottom border and the
    // table's own header row - what's left is real data-row capacity,
    // same accounting as interfaces_table above.
    let visible_rows = area.height.saturating_sub(3);
    let total_rows = rows.len() as u16;
    *scroll = (*scroll).min(total_rows.saturating_sub(visible_rows));

    let title = format!("Network Interfaces ({} total)", snap.interfaces.len());
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
    .block(panel(&title));

    let mut table_state = TableState::default().with_offset(*scroll as usize);
    frame.render_stateful_widget(table, area, &mut table_state);
    render_scrollbar(frame, area, total_rows, visible_rows, *scroll);
}

/// Every VRF's BGP peer count, no truncation - `frr_panel` above is the
/// Overview tab's compact, "+N more" version.
fn frr_detail(frame: &mut Frame, area: Rect, snap: &Snapshot, scroll: &mut u16) {
    let block = panel("FRR / BGP (all VRFs)");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [summary_area, table_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).areas(inner);

    let (state_text, color) = if snap.frr_service_active {
        ("active", OK)
    } else {
        ("inactive", BAD)
    };
    let mut summary = vec![Line::from(vec![
        Span::raw("frr.service: "),
        status(state_text, color),
    ])];
    if snap.frr_service_active {
        if !snap.frr.daemons.is_empty() {
            summary.push(Line::from(format!("daemons: {}", snap.frr.daemons)));
        }
        if let Some(routes) = &snap.frr.route_summary {
            summary.push(Line::from(format!("IPv4 RIB: {routes}")));
        }
    }
    frame.render_widget(Paragraph::new(summary), summary_area);

    let rows: Vec<Row> = if !snap.frr_service_active {
        Vec::new()
    } else if snap.frr.bgp_vrf_peers.is_empty() {
        vec![Row::new(vec![Cell::from("(no BGP peers)")])]
    } else {
        snap.frr
            .bgp_vrf_peers
            .iter()
            .map(|(vrf, estab, total)| {
                // See frr_panel's own comment on why 0/0 is MUTED, not
                // the OK "every configured peer is up" green.
                let color = if *total == 0 {
                    MUTED
                } else if *estab == *total {
                    OK
                } else if *estab == 0 {
                    BAD
                } else {
                    WARN
                };
                Row::new(vec![
                    Cell::from(vrf.clone()),
                    Cell::from(Span::styled(
                        format!("{estab}/{total} established"),
                        Style::default().fg(color),
                    )),
                ])
            })
            .collect()
    };

    // Just the table's own header row here, no block border of its own -
    // frr_detail's outer block (above) already drew one around the whole
    // tab, summary section included.
    let visible_rows = table_area.height.saturating_sub(1);
    let total_rows = rows.len() as u16;
    *scroll = (*scroll).min(total_rows.saturating_sub(visible_rows));

    let table = Table::new(rows, [Constraint::Length(24), Constraint::Min(20)]).header(
        Row::new(["VRF", "BGP Peers"])
            .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)),
    );
    let mut table_state = TableState::default().with_offset(*scroll as usize);
    frame.render_stateful_widget(table, table_area, &mut table_state);
    render_scrollbar(frame, table_area, total_rows, visible_rows, *scroll);
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

/// htop/btop-style hint bar: each key rendered as a highlighted block
/// glued directly to its action label (no "[key] description" prose) -
/// the same visual language as htop's permanently-visible
/// `F1Help F2Setup ...` row, with number keys picking a tab the way
/// btop's own box switcher uses its number row.
fn footer(frame: &mut Frame, area: Rect, tab: Tab, refresh_secs: u64) {
    let key_style = Style::default().fg(Color::Black).bg(ACCENT);
    let label_style = Style::default().fg(MUTED);
    let hint = |k: &'static str, label: &'static str| {
        [
            Span::styled(k, key_style),
            Span::styled(label, label_style),
            Span::raw(" "),
        ]
    };

    let mut spans = Vec::new();
    spans.extend(hint("1", "Overview"));
    spans.extend(hint("2", "Interfaces"));
    spans.extend(hint("3", "FRR/BGP"));
    if tab != Tab::Overview {
        spans.extend(hint("\u{2191}\u{2193}", "Scroll"));
        spans.extend(hint("PgUp/PgDn", "Page"));
    }
    spans.extend(hint("b", "Shell"));
    spans.push(Span::raw(format!("  (every {refresh_secs}s)")));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use crate::frr::FrrStatus;
    use crate::net::Interface;

    fn fake_snapshot(n_interfaces: usize) -> Snapshot {
        Snapshot {
            hostname: "test".to_string(),
            now: "now".to_string(),
            uptime: "0d 0h 0m".to_string(),
            load_average: "0.00 0.00 0.00".to_string(),
            cpu_count: 1,
            memory: None,
            disk: None,
            interfaces: (0..n_interfaces)
                .map(|i| Interface {
                    name: format!("eth{i}"),
                    up: true,
                    addrs: Vec::new(),
                    rate: None,
                })
                .collect(),
            frr_service_active: false,
            frr: FrrStatus {
                daemons: String::new(),
                route_summary: None,
                bgp_vrf_peers: Vec::new(),
            },
            sync_units: Vec::new(),
            bootc_status: None,
        }
    }

    /// Every visible cell's symbol, concatenated - good enough to assert
    /// "this text is/isn't on screen anywhere" without hand-deriving
    /// which exact row a given piece of content lands on (border/header
    /// row accounting that's already covered, and re-asserted
    /// separately, by interfaces_visible/bgp_visible_rows's own tests).
    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    // Real regression coverage for the scrolling this test file's own
    // change added: a `TableState` offset that's silently ignored (or a
    // clamp that's off by one) wouldn't show up as a panic or a build
    // failure - only as "the list just doesn't move" the next time
    // someone's actually looking at a real deployment's long interface
    // list, exactly the class of bug this dashboard's whole console
    // history (see git log) keeps turning out to only be caught live.
    #[test]
    fn interfaces_detail_scrolls_and_clamps() {
        let snap = fake_snapshot(30);
        let backend = TestBackend::new(60, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState {
            tab: Tab::Interfaces,
            ..Default::default()
        };

        terminal
            .draw(|frame| render(frame, &snap, &mut state, 5))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("eth0"), "eth0 should be visible unscrolled");

        state.scroll_by(5);
        terminal
            .draw(|frame| render(frame, &snap, &mut state, 5))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            !text.contains("eth0"),
            "eth0 should have scrolled off after scroll_by(5)"
        );
        assert!(
            text.contains("eth5"),
            "eth5 should be the first visible interface after scroll_by(5)"
        );

        // Scrolling far past the end must clamp (see AppState::
        // scroll_to_bottom's own comment on why u16::MAX is the sentinel
        // for that), not panic on an out-of-range TableState offset.
        state.scroll_to_bottom();
        terminal
            .draw(|frame| render(frame, &snap, &mut state, 5))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("eth29"),
            "the last interface should be visible once scrolled to the bottom"
        );
    }

    #[test]
    fn frr_detail_renders_with_no_vrfs_without_panicking() {
        let snap = fake_snapshot(0);
        let backend = TestBackend::new(60, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState {
            tab: Tab::Frr,
            ..Default::default()
        };
        terminal
            .draw(|frame| render(frame, &snap, &mut state, 5))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("inactive"));
    }

    #[test]
    fn tab_switching_changes_the_rendered_view() {
        let snap = fake_snapshot(3);
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::default();

        terminal
            .draw(|frame| render(frame, &snap, &mut state, 5))
            .unwrap();
        assert!(buffer_text(terminal.backend().buffer()).contains("Sync Services"));

        state.next_tab();
        assert_eq!(state.tab, Tab::Interfaces);
        terminal
            .draw(|frame| render(frame, &snap, &mut state, 5))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Network Interfaces (3 total)"));
        assert!(!text.contains("Sync Services"));
    }
}
