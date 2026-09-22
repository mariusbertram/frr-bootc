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
    Block, BorderType, Borders, Cell, Clear, Gauge, Paragraph, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Sparkline, Table, TableState, Tabs,
};
use ratatui::Frame;

use crate::events::EventLog;
use crate::frr::BgpPeerDetail;
use crate::health;
use crate::net::{fmt_rate, InterfaceDetail, Traffic};
use crate::snapshot::{BootcStatus, Snapshot};
use crate::system::fmt_bytes;
use crate::systemd::SyncHealth;

const OK: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;
const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

/// Overview is the original single-screen layout (compact, "+N more"
/// truncated) - Interfaces/Frr are full, scrollable listings of the same
/// data with nothing left out, and Events is the state-change log
/// (see events.rs), for whenever there's more to look at than
/// "+N more" wants to say (this repo's own scaling example is "150
/// BGP-coupled tenants" - see the README's "A Trunk Instead of One NIC
/// per Tenant" section).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tab {
    #[default]
    Overview,
    Interfaces,
    Frr,
    Events,
}

const TABS: [Tab; 4] = [Tab::Overview, Tab::Interfaces, Tab::Frr, Tab::Events];

impl Tab {
    fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Interfaces => "Interfaces",
            Tab::Frr => "FRR / BGP",
            Tab::Events => "Events",
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
/// scratch every refresh) - which tab is active, which item is selected
/// (highlighted) in each listing, how far each is scrolled, and whether
/// a detail popup is open (plus its own scroll). Kept per-tab rather
/// than shared so that switching tabs and back doesn't lose your place.
#[derive(Default)]
pub struct AppState {
    pub tab: Tab,
    interfaces_scroll: u16,
    interfaces_selected: usize,
    frr_scroll: u16,
    frr_selected: usize,
    sync_selected: usize,
    detail_open: bool,
    detail_scroll: u16,
    events_scroll: u16,
    events_follow: bool,
}

impl AppState {
    pub fn next_tab(&mut self) {
        self.tab = self.tab.next();
        self.close_detail();
    }

    pub fn prev_tab(&mut self) {
        self.tab = self.tab.prev();
        self.close_detail();
    }

    pub fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.close_detail();
    }

    /// Which list the selection keys drive on the active tab - Overview
    /// selects sync services (Enter opens their journal), Interfaces/
    /// Frr select their listings, Events has no selection (it scrolls).
    fn selected_mut(&mut self) -> Option<&mut usize> {
        match self.tab {
            Tab::Overview => Some(&mut self.sync_selected),
            Tab::Interfaces => Some(&mut self.interfaces_selected),
            Tab::Frr => Some(&mut self.frr_selected),
            Tab::Events => None,
        }
    }

    pub fn interfaces_selected(&self) -> usize {
        self.interfaces_selected
    }

    pub fn sync_selected(&self) -> usize {
        self.sync_selected
    }

    pub fn detail_open(&self) -> bool {
        self.detail_open
    }

    /// A no-op on Events, which has nothing to select - callers don't
    /// need to check which tab is active first. The real maximum only
    /// becomes known once the list's actual length is available at
    /// render time (see the render functions, which clamp down to it),
    /// same reasoning as the old scroll_to_bottom's u16::MAX.
    pub fn move_selection(&mut self, delta: i32) {
        let Some(selected) = self.selected_mut() else {
            return;
        };
        *selected = if delta >= 0 {
            selected.saturating_add(delta as usize)
        } else {
            selected.saturating_sub(delta.unsigned_abs() as usize)
        };
    }

    pub fn select_first(&mut self) {
        if let Some(selected) = self.selected_mut() {
            *selected = 0;
        }
    }

    pub fn select_last(&mut self) {
        if let Some(selected) = self.selected_mut() {
            *selected = usize::MAX;
        }
    }

    /// Opens the detail popup for whichever item is currently selected -
    /// a no-op on Events (nothing to select there in the first place)
    /// and on an Overview with no sync services to drill into.
    pub fn toggle_detail(&mut self) {
        match self.tab {
            Tab::Events => {}
            Tab::Overview if self.sync_selected == usize::MAX => {}
            _ => {
                self.detail_open = !self.detail_open;
                // Every (re)open starts at the top of the popup's
                // content - inheriting the previous popup's scroll
                // offset would land the reader mid-list of a different
                // item.
                if self.detail_open {
                    self.detail_scroll = 0;
                }
            }
        }
    }

    pub fn close_detail(&mut self) {
        self.detail_open = false;
        self.detail_scroll = 0;
    }

    /// Popup scrolling - `u16::MAX` is the "jump to the very bottom"
    /// sentinel (the render pass clamps it to the real content height),
    /// the same trick `select_last` uses for list selections.
    pub fn detail_scroll_by(&mut self, delta: i32) {
        self.detail_scroll = if delta >= 0 {
            self.detail_scroll.saturating_add(delta as u16)
        } else {
            self.detail_scroll
                .saturating_sub(delta.unsigned_abs() as u16)
        };
    }

    pub fn detail_scroll_top(&mut self) {
        self.detail_scroll = 0;
    }

    pub fn detail_scroll_bottom(&mut self) {
        self.detail_scroll = u16::MAX;
    }

    /// Events-tab scrolling. Scrolling up deliberately suspends
    /// auto-follow (a new event arriving mid-read must not yank the
    /// view back to the bottom); reaching the bottom again re-enables
    /// it (see events_panel).
    pub fn events_scroll_by(&mut self, delta: i32) {
        self.events_follow = false;
        self.events_scroll = if delta >= 0 {
            self.events_scroll.saturating_add(delta as u16)
        } else {
            self.events_scroll
                .saturating_sub(delta.unsigned_abs() as u16)
        };
    }

    pub fn events_to_top(&mut self) {
        self.events_follow = false;
        self.events_scroll = 0;
    }

    pub fn events_to_bottom(&mut self) {
        self.events_follow = true;
    }
}

/// `Block<'static>` on purpose: the title text is copied into an owned
/// `Span`, so callers can build titles from temporaries (e.g. a
/// `format!`ed traffic panel title) without the block borrowing them.
fn panel(title: &str) -> Block<'static> {
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

pub fn render(
    frame: &mut Frame,
    snap: Option<&Snapshot>,
    log: &EventLog,
    state: &mut AppState,
    refresh_secs: u64,
    data_age_secs: Option<u64>,
) {
    let Some(snap) = snap else {
        // First moments after startup, before the gather thread's first
        // snapshot has arrived - say so instead of drawing empty panels
        // that could be mistaken for "nothing is running".
        let text = Paragraph::new(Span::styled(
            " collecting data...",
            Style::default().fg(MUTED),
        ))
        .block(panel("Status"));
        frame.render_widget(text, frame.area());
        return;
    };

    let image_height = snap
        .bootc
        .as_ref()
        .map_or(0, |b| bootc_panel_lines(b).len() as u16 + 2);

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
    let total_height = frame.area().height;
    let traffic_height = traffic_panel_height(total_height);
    let mut constraints = vec![
        Constraint::Length(3), // header
        Constraint::Length(1), // tab bar
    ];
    match state.tab {
        Tab::Overview => {
            constraints.push(Constraint::Length(3)); // stats row
            if let Some(height) = traffic_height {
                constraints.push(Constraint::Length(height)); // traffic graph
            }
            constraints.push(Constraint::Fill(3)); // network interfaces
            constraints.push(Constraint::Fill(2)); // FRR
            constraints.push(Constraint::Length(6)); // sync services
            if image_height > 0 {
                constraints.push(Constraint::Length(image_height));
            }
        }
        Tab::Interfaces | Tab::Frr | Tab::Events => constraints.push(Constraint::Fill(1)),
    }
    constraints.push(Constraint::Length(1)); // footer

    let areas = Layout::vertical(constraints).split(frame.area());
    let mut idx = 0;
    let mut next = || {
        let area = areas[idx];
        idx += 1;
        area
    };

    header(frame, next(), snap, data_age_secs, refresh_secs);
    tab_bar(frame, next(), state.tab);

    match state.tab {
        Tab::Overview => {
            stats_row(frame, next(), snap);
            if traffic_height.is_some() {
                let area = next();
                match &snap.traffic {
                    Some(traffic) => traffic_panel(frame, area, traffic),
                    None => frame.render_widget(
                        Paragraph::new(Span::styled(
                            " (warming up - no throughput samples yet)",
                            Style::default().fg(MUTED),
                        ))
                        .block(panel("Traffic")),
                        area,
                    ),
                }
            }
            interfaces_table(frame, next(), snap);
            frr_panel(frame, next(), snap);
            sync_table(frame, next(), snap, state);
            if image_height > 0 {
                image_panel(frame, next(), snap);
            }
            if state.detail_open {
                if let Some(unit) = snap.sync_units.get(state.sync_selected) {
                    let lines: Vec<Line<'static>> = match &snap.selected_sync_journal {
                        Some(journal) => {
                            journal.lines().map(|l| Line::from(l.to_string())).collect()
                        }
                        None => vec![Line::styled(
                            "(no journal output)",
                            Style::default().fg(MUTED),
                        )],
                    };
                    // Anchored over the whole frame - the Overview has
                    // no single panel a journal belongs to.
                    render_popup(
                        frame,
                        frame.area(),
                        unit.label,
                        lines,
                        &mut state.detail_scroll,
                    );
                }
            }
        }
        Tab::Interfaces => interfaces_detail(frame, next(), snap, state),
        Tab::Frr => frr_detail(frame, next(), snap, state),
        Tab::Events => events_panel(frame, next(), log, state),
    }

    footer(frame, next(), state.tab, state.detail_open, refresh_secs);
}

/// The traffic panel's height for a given terminal height - 2-row
/// sparklines when there's room to spare, 1-row ones on a short
/// terminal, nothing at all below 22 rows (an 80x24 serial console with
/// every other panel up has no business fitting a graph too; the
/// detail tabs keep the numbers readable there instead).
fn traffic_panel_height(total_height: u16) -> Option<u16> {
    match total_height {
        0..=21 => None,
        22..=29 => Some(6),
        _ => Some(8),
    }
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

fn header(
    frame: &mut Frame,
    area: Rect,
    snap: &Snapshot,
    data_age_secs: Option<u64>,
    refresh_secs: u64,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT));
    let verdict = health::assess(snap);

    let mut line = Line::from(vec![
        Span::styled(
            format!(" {} ", snap.hostname),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        status(verdict.badge(), verdict.level.color()),
    ]);
    // Ordered by how much it matters on a narrow terminal: the verdict
    // and how stale the data is survive clipping; the static label is
    // first to go, the clock last.
    if let Some(age) = data_age_secs {
        let stale = age > refresh_secs.saturating_mul(2);
        line.spans.push(Span::raw("  data "));
        line.spans.push(status(
            format!("{age}s ago"),
            if stale { BAD } else { MUTED },
        ));
    }
    line.spans.push(Span::raw("   "));
    line.spans.push(Span::raw(&snap.now));
    line.spans.push(Span::raw("   uptime "));
    line.spans.push(Span::raw(&snap.uptime));
    line.spans.push(Span::styled(
        "   frr-bootc router console",
        Style::default().fg(MUTED),
    ));
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

/// The Overview tab's traffic graph: rolling rx/tx for the interface
/// carrying the most cumulative traffic (the one physical trunk in this
/// image's design), one autoscaling sparkline per direction. Two
/// directions, never one combined curve - on a router the rx/tx
/// asymmetry *is* the signal.
fn traffic_panel(frame: &mut Frame, area: Rect, traffic: &Traffic) {
    // How tall the two spark areas are is decided entirely by the
    // constraint `traffic_panel_height` picked for the panel - the Fill
    // splits below turn that into 1 or 2 sparkline rows each, no
    // separate bookkeeping here.
    let block = panel(&format!("Traffic ({}, ~6 min, 5s/bar)", traffic.name));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [rx_label, rx_spark, tx_label, tx_spark] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);

    let rx: Vec<u64> = traffic
        .series
        .iter()
        .map(|&(rx, _)| rx.saturating_mul(8))
        .collect();
    let tx: Vec<u64> = traffic
        .series
        .iter()
        .map(|&(_, tx)| tx.saturating_mul(8))
        .collect();

    for (label_area, spark_area, series, name, color) in [
        (&rx_label, &rx_spark, &rx, "rx", ACCENT),
        (&tx_label, &tx_spark, &tx, "tx", OK),
    ] {
        let latest = series.last().copied();
        let peak = series.iter().max().copied();
        let fmt = |bits: u64| fmt_rate(bits / 8);
        let mut spans = vec![Span::styled(
            format!("{name} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )];
        match (latest, peak) {
            (Some(latest), Some(peak)) => {
                spans.push(Span::raw(fmt(latest)));
                spans.push(Span::styled(
                    format!("   peak {}", fmt(peak)),
                    Style::default().fg(MUTED),
                ));
            }
            _ => spans.push(Span::styled("-", Style::default().fg(MUTED))),
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), *label_area);
        let spark = Sparkline::default()
            .data(series)
            .style(Style::default().fg(color));
        frame.render_widget(spark, *spark_area);
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

fn interface_rows(snap: &Snapshot, shown: usize, selected: Option<usize>) -> Vec<Row<'static>> {
    snap.interfaces[..shown]
        .iter()
        .enumerate()
        .flat_map(|(i, iface)| {
            let (state_text, color) = if iface.up { ("UP", OK) } else { ("DOWN", BAD) };
            let rate = match iface.rate {
                Some((rx, tx)) => format!("rx {} / tx {}", fmt_rate(rx), fmt_rate(tx)),
                None => "-".to_string(),
            };
            let row_style = if Some(i) == selected {
                Style::default().add_modifier(SELECTED)
            } else {
                Style::default()
            };
            let main = Row::new(vec![
                Cell::from(iface.name.clone()),
                Cell::from(Span::styled(state_text, Style::default().fg(color))),
                Cell::from(rate),
            ])
            .style(row_style);
            let addr = (!iface.addrs.is_empty()).then(|| {
                Row::new(vec![
                    Cell::default(),
                    Cell::default(),
                    Cell::from(Span::styled(
                        iface.addrs.join(" "),
                        Style::default().fg(MUTED),
                    )),
                ])
                .style(row_style)
            });
            std::iter::once(main).chain(addr)
        })
        .collect()
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
        // No selection on the Overview tab's copy - it's a read-only
        // summary; selection/drill-down lives on the Interfaces tab.
        interface_rows(snap, shown, None)
            .into_iter()
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
        lines.extend(bfd_summary_lines(snap));
        if let Some(line) = vty_error_line(snap) {
            lines.push(line);
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
                for (vrf, estab, total) in sorted_vrfs(snap).into_iter().take(shown) {
                    // A VRF can have a BGP instance but zero configured
                    // neighbors - shown (unlike the old text-table
                    // parser, which never produced an entry for it at
                    // all) rather than hidden, but "0/0" isn't a healthy
                    // "OK" so much as "nothing to report" - MUTED reads
                    // that way, where green would misleadingly imply
                    // every configured peer (there are none) is up.
                    let color = peer_count_color(estab, total);
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

/// The Overview FRR panel sorts problems to the top, same as the FRR
/// tab - with 150 VRFs, the one red row must not sit behind 149 green
/// ones below the fold.
/// Borrows the VRF list rather than cloning it - this runs on every
/// redraw (key presses included), not just every refresh tick, and the
/// snapshot already owns the data.
fn sorted_vrfs(snap: &Snapshot) -> Vec<(&str, u32, u32)> {
    let mut vrfs: Vec<(&str, u32, u32)> = snap
        .frr
        .bgp_vrf_peers
        .iter()
        .map(|(vrf, estab, total)| (vrf.as_str(), *estab, *total))
        .collect();
    vrfs.sort_by(|a, b| (rank_vrf(a.1, a.2), a.0).cmp(&(rank_vrf(b.1, b.2), b.0)));
    vrfs
}

/// The muted `vty: ...` line for both FRR panels - the first couple of
/// this tick's query failures, with a "+N more" rollup. Kept out of the
/// health verdict on purpose (see `FrrStatus::query_errors`): observability
/// loss is reported where you're looking, it doesn't claim the router is
/// broken.
fn vty_error_line(snap: &Snapshot) -> Option<Line<'static>> {
    let errors = &snap.frr.query_errors;
    if errors.is_empty() {
        return None;
    }
    let shown: Vec<&str> = errors.iter().take(2).map(String::as_str).collect();
    let mut text = format!("vty: {}", shown.join("; "));
    let rest = errors.len() - shown.len();
    if rest > 0 {
        text.push_str(&format!(" (+{rest} more)"));
    }
    Some(Line::styled(text, Style::default().fg(MUTED)))
}

/// Sort key: VRFs with downed sessions first (rank 0), fully
/// established next, "0/0 nothing configured" last - the interesting
/// rows lead, the "nothing to report" rows trail.
fn rank_vrf(estab: u32, total: u32) -> u8 {
    if total == 0 {
        2
    } else if estab == total {
        1
    } else {
        0
    }
}

fn peer_count_color(estab: u32, total: u32) -> Color {
    if total == 0 {
        MUTED
    } else if estab == total {
        OK
    } else if estab == 0 {
        BAD
    } else {
        WARN
    }
}

/// The `BFD: N/N up` line for the Overview panel and the FRR tab's
/// summary - present only when bfdd reported sessions at all, so a VM
/// without BFD configured doesn't grow a permanently-zero line.
fn bfd_summary_lines(snap: &Snapshot) -> Vec<Line<'static>> {
    if snap.frr.bfd_peers.is_empty() {
        return Vec::new();
    }
    let up = snap
        .frr
        .bfd_peers
        .iter()
        .filter(|p| !p.status.eq_ignore_ascii_case("down"))
        .count();
    let total = snap.frr.bfd_peers.len();
    let color = if up == total { OK } else { BAD };
    vec![Line::from(vec![
        Span::raw("BFD: "),
        status(format!("{up}/{total} up"), color),
    ])]
}

fn sync_table(frame: &mut Frame, area: Rect, snap: &Snapshot, state: &mut AppState) {
    if !snap.sync_units.is_empty() {
        state.sync_selected = state.sync_selected.min(snap.sync_units.len() - 1);
    }

    let rows: Vec<Row> = snap
        .sync_units
        .iter()
        .enumerate()
        .map(|(i, unit)| {
            let (word, color) = match &unit.health {
                SyncHealth::Failed { .. } => ("FAILED", BAD),
                SyncHealth::TimerNotActive => ("timer not active", WARN),
                SyncHealth::Ok { .. } => ("ok", OK),
            };
            let row_style = if i == state.sync_selected {
                Style::default().add_modifier(SELECTED)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(unit.label),
                Cell::from(Span::styled(word, Style::default().fg(color))),
                Cell::from(Span::styled(
                    unit.health.detail(),
                    Style::default().fg(MUTED),
                )),
            ])
            .style(row_style)
        })
        .collect();

    let title = if state.sync_selected == usize::MAX {
        "Sync Services".to_string()
    } else {
        "Sync Services (Enter: journal)".to_string()
    };
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
    .block(panel(&title));
    frame.render_widget(table, area);
}

/// Adjusts `*scroll` (if needed) so that row `row` falls within the
/// visible window - scrolls up if it's above, down if it's below,
/// leaves it alone if it's already visible. Shared by the detail tabs'
/// listings to keep the selected row on screen as it moves.
fn ensure_visible(scroll: &mut u16, row: u16, visible_rows: u16) {
    if row < *scroll {
        *scroll = row;
    } else if visible_rows > 0 && row >= scroll.saturating_add(visible_rows) {
        *scroll = row + 1 - visible_rows;
    }
}

const SELECTED: Modifier = Modifier::REVERSED;

/// Every interface, no truncation - the Overview tab's compact,
/// "+N more" table is `interfaces_table` above; this is its scrollable,
/// selectable counterpart, with Enter opening a detail popup for
/// whichever interface is currently highlighted (see
/// interface_detail_popup).
fn interfaces_detail(frame: &mut Frame, area: Rect, snap: &Snapshot, state: &mut AppState) {
    let selected = &mut state.interfaces_selected;
    if !snap.interfaces.is_empty() {
        *selected = (*selected).min(snap.interfaces.len() - 1);
    } else {
        *selected = 0;
    }

    let mut selected_row = 0u16;
    let rows: Vec<Row> = if snap.interfaces.is_empty() {
        vec![Row::new(vec![Cell::from("(none)")])]
    } else {
        for (i, _) in snap.interfaces.iter().enumerate() {
            if i == *selected {
                selected_row = row_count_so_far(&snap.interfaces[..i]);
            }
        }
        interface_rows(snap, snap.interfaces.len(), Some(*selected))
    };

    // area.height includes the block's top/bottom border and the
    // table's own header row - what's left is real data-row capacity,
    // same accounting as interfaces_table above.
    let visible_rows = area.height.saturating_sub(3);
    let total_rows = rows.len() as u16;
    ensure_visible(&mut state.interfaces_scroll, selected_row, visible_rows);
    state.interfaces_scroll = state
        .interfaces_scroll
        .min(total_rows.saturating_sub(visible_rows));

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

    let mut table_state = TableState::default().with_offset(state.interfaces_scroll as usize);
    frame.render_stateful_widget(table, area, &mut table_state);
    render_scrollbar(
        frame,
        area,
        total_rows,
        visible_rows,
        state.interfaces_scroll,
    );

    if state.detail_open {
        if let Some(iface) = snap.interfaces.get(state.interfaces_selected) {
            interface_detail_popup(
                frame,
                area,
                iface,
                snap.selected_interface_detail.as_ref(),
                &mut state.detail_scroll,
            );
        }
    }
}

/// How many table rows `interfaces[..i]` (every interface *before* index
/// `i`) occupies - each contributes 1 row normally, 2 if it has an
/// address line of its own, same cost accounting `interfaces_visible`
/// (the Overview tab's truncation) uses.
fn row_count_so_far(interfaces: &[crate::net::Interface]) -> u16 {
    interfaces
        .iter()
        .map(|iface| if iface.addrs.is_empty() { 1 } else { 2 })
        .sum()
}

/// A floating box roughly centered over `area`, sized to its content
/// (up to `area`'s own bounds) - the popup pattern ratatui itself
/// documents: `Clear` first so whatever was drawn underneath doesn't
/// show through around/behind the box's own background.
fn popup_area(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

/// A scrollable popup: content longer than fits is reachable with the
/// scroll keys (see AppState::detail_scroll_by) instead of being
/// silently clipped - a tenant VRF with dozens of BGP peers used to
/// simply lose its tail beyond the fold. `scroll` is clamped here
/// against the real content height, so the `u16::MAX` "bottom" sentinel
/// works and an over-long offset from a previously-shown popup can't
/// overshoot.
fn render_popup(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    lines: Vec<Line<'static>>,
    scroll: &mut u16,
) {
    let content_height = lines.len() as u16;
    let height = (content_height + 2).clamp(3, area.height.saturating_sub(2));
    let inner_height = height.saturating_sub(2);
    let width = lines
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(0)
        .clamp(30, area.width.saturating_sub(4) as usize) as u16
        + 4;
    let popup = popup_area(area, width, height);
    frame.render_widget(Clear, popup);

    let max_scroll = content_height.saturating_sub(inner_height);
    *scroll = (*scroll).min(max_scroll);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(title))
            .scroll((*scroll, 0)),
        popup,
    );
    render_scrollbar(frame, popup, content_height, inner_height, *scroll);
}

/// Detail for one interface (Enter, on the Interfaces tab): MTU, MAC,
/// which VRF/device it's enslaved to (relevant for a tenant's VLAN
/// sub-interface - see "VRF per Tenant" in the README), current
/// error/drop rates, and cumulative (not just current-rate) rx/tx
/// counters. `detail` is `None` only very briefly - it's populated on
/// the same refresh cycle that opens the popup (see snapshot::gather) -
/// or if every read under /sys/class/net/<iface> failed outright.
fn interface_detail_popup(
    frame: &mut Frame,
    area: Rect,
    iface: &crate::net::Interface,
    detail: Option<&InterfaceDetail>,
    scroll: &mut u16,
) {
    let (state_text, color) = if iface.up { ("UP", OK) } else { ("DOWN", BAD) };
    let mut lines = vec![Line::from(vec![
        Span::raw("State: "),
        status(state_text, color),
    ])];
    lines.push(if iface.addrs.is_empty() {
        Line::styled("Addresses: (none)", Style::default().fg(MUTED))
    } else {
        Line::from(format!("Addresses: {}", iface.addrs.join(", ")))
    });
    if let Some((rx, tx)) = iface.rate {
        lines.push(Line::from(format!(
            "Throughput: rx {} / tx {}",
            fmt_rate(rx),
            fmt_rate(tx)
        )));
    }
    if let Some((rx, tx)) = iface.err_rate {
        // Rates, not counters: "is it dropping right now". Zero rates
        // stay visible rather than hidden - on a busy trunk, "0/s" is
        // itself the finding worth glancing at.
        lines.push(Line::from(format!(
            "Error/drop rate: rx {rx}/s / tx {tx}/s"
        )));
    }

    match detail {
        Some(d) => {
            lines.push(Line::from(""));
            if let Some(mtu) = d.mtu {
                lines.push(Line::from(format!("MTU: {mtu}")));
            }
            if let Some(mac) = &d.mac {
                lines.push(Line::from(format!("MAC: {mac}")));
            }
            lines.push(match &d.master {
                Some(m) => Line::from(format!("VRF/master: {m}")),
                None => Line::styled("VRF/master: (none)", Style::default().fg(MUTED)),
            });
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "Cumulative counters:",
                Style::default().fg(MUTED),
            ));
            let n = |v: Option<u64>| v.map_or_else(|| "-".to_string(), |v| v.to_string());
            lines.push(Line::from(format!(
                "  RX: {} bytes, {} packets, {} errors, {} dropped",
                n(d.rx_bytes),
                n(d.rx_packets),
                n(d.rx_errors),
                n(d.rx_dropped)
            )));
            lines.push(Line::from(format!(
                "  TX: {} bytes, {} packets, {} errors, {} dropped",
                n(d.tx_bytes),
                n(d.tx_packets),
                n(d.tx_errors),
                n(d.tx_dropped)
            )));
        }
        None => lines.push(Line::styled(
            "(detail unavailable)",
            Style::default().fg(MUTED),
        )),
    }

    render_popup(frame, area, &format!(" {} ", iface.name), lines, scroll);
}

/// Every VRF's BGP peer count, no truncation - `frr_panel` above is
/// the Overview tab's compact, "+N more" version. Enter opens a popup
/// with that VRF's individual peers (see vrf_detail_popup) - the
/// aggregate established/total counts shown here are exactly the sums
/// of what's in that popup. VRFs with downed sessions sort to the top
/// (see sorted_vrfs), so the one problem tenant is the first row, not
/// row 137 of 150.
fn frr_detail(frame: &mut Frame, area: Rect, snap: &Snapshot, state: &mut AppState) {
    let block = panel("FRR / BGP (all VRFs)");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let bfd_lines = bfd_summary_lines(snap);
    let vty_error = vty_error_line(snap);
    // Exact line count of the summary block below (frr.service line,
    // plus - only while the service is up - the BFD line and whatever
    // daemons/RIB lines will actually render, plus the vty error line
    // whenever there are failures). A Length that's shorter than the
    // content would silently clip its last line, e.g. the RIB count on
    // a VM with BFD.
    let summary_height = 1
        + u16::from(vty_error.is_some())
        + if snap.frr_service_active {
            bfd_lines.len() as u16
                + u16::from(!snap.frr.daemons.is_empty())
                + u16::from(snap.frr.route_summary.is_some())
        } else {
            0
        };
    let [summary_area, table_area] =
        Layout::vertical([Constraint::Length(summary_height), Constraint::Fill(1)]).areas(inner);

    let (state_text, color) = if snap.frr_service_active {
        ("active", OK)
    } else {
        ("inactive", BAD)
    };
    let mut summary = vec![Line::from(vec![
        Span::raw("frr.service: "),
        status(state_text, color),
    ])];
    summary.extend(bfd_lines);
    if let Some(line) = vty_error {
        summary.push(line);
    }
    if snap.frr_service_active {
        if !snap.frr.daemons.is_empty() {
            summary.push(Line::from(format!("daemons: {}", snap.frr.daemons)));
        }
        if let Some(routes) = &snap.frr.route_summary {
            summary.push(Line::from(format!("IPv4 RIB: {routes}")));
        }
    }
    frame.render_widget(Paragraph::new(summary), summary_area);

    let vrfs = sorted_vrfs(snap);
    if !vrfs.is_empty() {
        state.frr_selected = state.frr_selected.min(vrfs.len() - 1);
    } else {
        state.frr_selected = 0;
    }

    let rows: Vec<Row> = if !snap.frr_service_active {
        Vec::new()
    } else if vrfs.is_empty() {
        vec![Row::new(vec![Cell::from("(no BGP peers)")])]
    } else {
        vrfs.iter()
            .enumerate()
            .map(|(i, (vrf, estab, total))| {
                // See frr_panel's own comment on why 0/0 is MUTED, not
                // the OK "every configured peer is up" green.
                let color = peer_count_color(*estab, *total);
                let route_cell = match snap.frr.vrf_routes.get(*vrf) {
                    Some(&(rib, fib)) => {
                        let style = if fib < rib {
                            Style::default().fg(WARN)
                        } else {
                            Style::default().fg(OK)
                        };
                        Cell::from(Span::styled(format!("{rib}/{fib}"), style))
                    }
                    None => Cell::from(Span::styled("-", Style::default().fg(MUTED))),
                };
                let row_style = if i == state.frr_selected {
                    Style::default().add_modifier(SELECTED)
                } else {
                    Style::default()
                };
                Row::new(vec![
                    Cell::from(*vrf),
                    Cell::from(Span::styled(
                        format!("{estab}/{total} established"),
                        Style::default().fg(color),
                    )),
                    route_cell,
                ])
                .style(row_style)
            })
            .collect()
    };

    // Just the table's own header row here, no block border of its own -
    // frr_detail's outer block (above) already drew one around the whole
    // tab, summary section included.
    let visible_rows = table_area.height.saturating_sub(1);
    let total_rows = rows.len() as u16;
    ensure_visible(
        &mut state.frr_scroll,
        state.frr_selected as u16,
        visible_rows,
    );
    state.frr_scroll = state
        .frr_scroll
        .min(total_rows.saturating_sub(visible_rows));

    let table = Table::new(
        rows,
        [
            Constraint::Length(24),
            Constraint::Length(18),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(["VRF", "BGP Peers", "Routes (RIB/FIB)"])
            .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)),
    );
    let mut table_state = TableState::default().with_offset(state.frr_scroll as usize);
    frame.render_stateful_widget(table, table_area, &mut table_state);
    render_scrollbar(
        frame,
        table_area,
        total_rows,
        visible_rows,
        state.frr_scroll,
    );

    if state.detail_open && snap.frr_service_active {
        if let Some(&(vrf, _, _)) = vrfs.get(state.frr_selected) {
            let empty = Vec::new();
            let peers = snap.frr.bgp_vrf_peer_detail.get(vrf).unwrap_or(&empty);
            vrf_detail_popup(
                frame,
                area,
                vrf,
                peers,
                &snap.frr.bfd_peers,
                &mut state.detail_scroll,
            );
        }
    }
}

/// A VRF's individual BGP peers - each peer contributes one row per
/// AFI/SAFI it runs (a dual-stack session shows as two rows, since the
/// IPv4 and IPv6 sessions can be in different states), with the prefix
/// counts that are this repo's stand-in for "routes imported/exported":
/// `pfxRcd`/`pfxSnt` are exactly how many prefixes this peer has sent
/// this router (imported into this VRF's table) and been sent by it
/// (exported/advertised), respectively - FRR's own BGP RIB numbers, not
/// a separate route dump. Sessions that aren't Established sort to the
/// top so the problem is the first row of the popup, and the VRF's BFD
/// sessions (if any) follow - a BFD-down line explains a BGP session
/// that's flapping or about to.
fn vrf_detail_popup(
    frame: &mut Frame,
    area: Rect,
    vrf: &str,
    peers: &[BgpPeerDetail],
    bfd_peers: &[crate::frr::BfdPeerDetail],
    scroll: &mut u16,
) {
    let mut sorted: Vec<&BgpPeerDetail> = peers.iter().collect();
    sorted.sort_by(|a, b| {
        (a.state != "Established", &a.peer, &a.afi).cmp(&(
            b.state != "Established",
            &b.peer,
            &b.afi,
        ))
    });

    let mut lines: Vec<Line<'static>> = if sorted.is_empty() {
        vec![Line::styled("(no BGP peers)", Style::default().fg(MUTED))]
    } else {
        sorted
            .iter()
            .flat_map(|p| {
                let color = if p.state == "Established" { OK } else { BAD };
                let heading = Line::from(vec![
                    Span::raw(format!("{:<20} {:<12} ", p.peer, p.afi)),
                    status(p.state.clone(), color),
                ]);
                let as_text = p
                    .remote_as
                    .map_or_else(|| "-".to_string(), |a| a.to_string());
                let uptime = p.uptime.as_deref().unwrap_or("-");
                let detail = Line::styled(
                    format!(
                        "  AS {as_text}   up {uptime}   prefixes in/out {}/{}",
                        p.pfx_rcd, p.pfx_snt
                    ),
                    Style::default().fg(MUTED),
                );
                [heading, detail]
            })
            .collect()
    };

    let vrf_bfd: Vec<&crate::frr::BfdPeerDetail> =
        bfd_peers.iter().filter(|b| b.vrf == vrf).collect();
    if !vrf_bfd.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::styled("BFD sessions:", Style::default().fg(MUTED)));
        for b in vrf_bfd {
            let down = b.status.eq_ignore_ascii_case("down");
            let uptime = b.uptime.as_deref().unwrap_or("-");
            lines.push(Line::from(vec![
                Span::raw(format!("  {:<20} ", b.peer)),
                status(
                    if down { "down" } else { "up" },
                    if down { BAD } else { OK },
                ),
                Span::styled(format!("   up {uptime}"), Style::default().fg(MUTED)),
            ]));
        }
    }

    render_popup(frame, area, &format!(" {vrf} "), lines, scroll);
}

/// The state-change log (see events.rs) - newest at the bottom like any
/// log, auto-following the tail until the reader scrolls up to study
/// history (and re-following once they scroll back down to the bottom,
/// see AppState::events_scroll_by).
fn events_panel(frame: &mut Frame, area: Rect, log: &EventLog, state: &mut AppState) {
    let block = panel("Event Log (state changes since boot)");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines: Vec<Line<'static>> = if log.events().is_empty() {
        vec![Line::styled("(no events yet)", Style::default().fg(MUTED))]
    } else {
        log.events()
            .iter()
            .map(|e| {
                Line::from(vec![
                    Span::styled(format!("[{}] ", e.time), Style::default().fg(MUTED)),
                    Span::styled(e.text.clone(), Style::default().fg(e.level.color())),
                ])
            })
            .collect()
    };

    let total = lines.len() as u16;
    let visible = inner.height;
    if state.events_follow {
        state.events_scroll = total.saturating_sub(visible);
    } else {
        state.events_scroll = state.events_scroll.min(total.saturating_sub(visible));
    }
    // Scrolled back down to the tail: resume auto-follow.
    if state.events_scroll >= total.saturating_sub(visible) {
        state.events_follow = true;
    }

    frame.render_widget(
        Paragraph::new(lines).scroll((state.events_scroll, 0)),
        inner,
    );
    render_scrollbar(frame, inner, total, visible, state.events_scroll);
}

fn bootc_panel_lines(bootc: &BootcStatus) -> Vec<Line<'static>> {
    let short_image = |image: &Option<String>| {
        image
            .as_deref()
            .and_then(|i| i.rsplit('/').next())
            .unwrap_or("?")
            .to_string()
    };
    let short_digest = |digest: &Option<String>| {
        digest
            .as_deref()
            .map(|d| d.trim_start_matches("sha256:").chars().take(12).collect())
            .unwrap_or_else(|| "?".to_string())
    };

    let mut lines = Vec::new();
    if bootc.booted_digest.is_some() || bootc.booted_image.is_some() {
        lines.push(Line::from(format!(
            "booted: {} ({})",
            short_image(&bootc.booted_image),
            short_digest(&bootc.booted_digest),
        )));
        if bootc.reboot_pending() {
            lines.push(Line::from(vec![
                Span::raw(format!(
                    "staged: {} ({}) - ",
                    short_image(&bootc.staged_image),
                    short_digest(&bootc.staged_digest),
                )),
                status("reboot required", WARN),
            ]));
        }
    }
    // Raw text fallback - whatever the structured parse couldn't answer,
    // `bootc status`'s own output still shows, as before.
    if let Some(raw) = &bootc.raw {
        for line in raw.lines() {
            lines.push(Line::from(line.to_string()));
        }
    }
    lines
}

fn image_panel(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let Some(bootc) = &snap.bootc else {
        return;
    };
    let lines = bootc_panel_lines(bootc);
    if lines.is_empty() {
        return;
    }
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
///
/// Hints carry a priority *tier*: when the combined line doesn't fit the
/// terminal width (80 columns on a serial console is the floor this must
/// work at), the lowest-priority tier is dropped whole, then the next -
/// a truncated hint that reads as present-but-clipped would be worse
/// than a missing one. Tier 0 is load-bearing (tabs, shell), 1 is
/// navigation detail, 2 is the refresh annotation.
const fn hint_tier(
    key: &'static str,
    label: &'static str,
    tier: u8,
) -> (&'static str, &'static str, u8) {
    (key, label, tier)
}

fn footer(frame: &mut Frame, area: Rect, tab: Tab, detail_open: bool, refresh_secs: u64) {
    let key_style = Style::default().fg(Color::Black).bg(ACCENT);
    let label_style = Style::default().fg(MUTED);

    let mut hints: Vec<(&'static str, &'static str, u8)> = vec![
        hint_tier("1", "Overview", 0),
        hint_tier("2", "Interfaces", 0),
        hint_tier("3", "FRR/BGP", 0),
        hint_tier("4", "Events", 0),
    ];
    match tab {
        Tab::Events => hints.extend([
            hint_tier("\u{2191}\u{2193}", "Scroll", 1),
            hint_tier("End", "Follow", 1),
        ]),
        _ if detail_open => hints.extend([
            hint_tier("\u{2191}\u{2193}", "Scroll", 1),
            hint_tier("Esc", "Close", 0),
        ]),
        Tab::Overview => hints.extend([
            hint_tier("\u{2191}\u{2193}", "Select", 1),
            hint_tier("Enter", "Journal", 1),
            hint_tier("PgUp/PgDn", "Page", 1),
        ]),
        _ => hints.extend([
            hint_tier("\u{2191}\u{2193}", "Select", 1),
            hint_tier("PgUp/PgDn", "Page", 1),
            hint_tier("Enter", "Detail", 0),
        ]),
    }
    hints.push(hint_tier("b", "Shell", 0));

    let width_of = |max_tier: u8| -> usize {
        hints
            .iter()
            .filter(|&&(_, _, tier)| tier <= max_tier)
            .map(|&(k, l, _)| k.chars().count() + 1 + l.chars().count() + 1)
            .sum::<usize>()
            + if max_tier >= 2 {
                format!("  (every {refresh_secs}s)").len()
            } else {
                0
            }
    };
    let mut max_tier = 2u8;
    while max_tier > 0 && width_of(max_tier) > area.width as usize {
        max_tier -= 1;
    }

    let mut spans = Vec::new();
    for &(key, label, tier) in &hints {
        if tier > max_tier {
            continue;
        }
        spans.push(Span::styled(key, key_style));
        spans.push(Span::styled(format!("{label} "), label_style));
    }
    if max_tier >= 2 {
        spans.push(Span::raw(format!(" (every {refresh_secs}s)")));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use std::collections::BTreeMap;

    use crate::frr::{BfdPeerDetail, FrrStatus};
    use crate::health::Level;
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
                    err_rate: None,
                })
                .collect(),
            selected_interface_detail: None,
            frr_service_active: false,
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

    fn draw(
        terminal: &mut Terminal<TestBackend>,
        snap: &Snapshot,
        log: &EventLog,
        state: &mut AppState,
    ) -> String {
        terminal
            .draw(|frame| render(frame, Some(snap), log, state, 5, Some(2)))
            .unwrap();
        buffer_text(terminal.backend().buffer())
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
        let log = EventLog::new(100, None);

        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(text.contains("eth0"), "eth0 should be visible unscrolled");

        // Moving the selection within the already-visible window (7 data
        // rows fit in this 15-row backend) must not scroll at all - the
        // viewport only follows the selection once it would otherwise
        // go off screen.
        state.move_selection(5);
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(
            text.contains("eth0") && text.contains("eth5"),
            "eth0..eth5 should both still be visible after move_selection(5) within the same window"
        );

        // Moving past the visible window must scroll to follow the
        // selection.
        state.move_selection(5); // now selecting eth10
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(
            !text.contains("eth0"),
            "eth0 should have scrolled off once the selection moved past the visible window"
        );
        assert!(
            text.contains("eth10"),
            "eth10 should be the selected (and visible) interface"
        );

        // Selecting far past the end must clamp (see AppState::
        // select_last's own comment on why usize::MAX is the sentinel
        // for that), not panic on an out-of-range TableState offset.
        state.select_last();
        let text = draw(&mut terminal, &snap, &log, &mut state);
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
        let log = EventLog::new(100, None);
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(text.contains("inactive"));
    }

    #[test]
    fn tab_switching_changes_the_rendered_view() {
        let snap = fake_snapshot(3);
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::default();
        let log = EventLog::new(100, None);

        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(text.contains("Sync Services"));

        state.next_tab();
        assert_eq!(state.tab, Tab::Interfaces);
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(text.contains("Network Interfaces (3 total)"));
        assert!(!text.contains("Sync Services"));
    }

    #[test]
    fn enter_opens_interface_detail_popup_for_the_selected_interface() {
        let mut snap = fake_snapshot(3);
        snap.interfaces[1].addrs = vec!["10.0.0.5/30".to_string()];
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState {
            tab: Tab::Interfaces,
            ..Default::default()
        };
        let log = EventLog::new(100, None);

        // Not open yet - none of the popup-only content should appear.
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(!text.contains("Cumulative counters"));

        state.move_selection(1); // select eth1, the one with an address
        state.toggle_detail();
        assert!(state.detail_open());
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(text.contains("eth1"), "popup should be titled after eth1");
        assert!(text.contains("10.0.0.5/30"));

        state.close_detail();
        assert!(!state.detail_open());
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(!text.contains("Cumulative counters"));
    }

    #[test]
    fn enter_opens_vrf_detail_popup_with_its_peers() {
        let mut snap = fake_snapshot(0);
        snap.frr_service_active = true;
        snap.frr.bgp_vrf_peers = vec![("vrf-tenant1".to_string(), 1, 1)];
        snap.frr.bgp_vrf_peer_detail.insert(
            "vrf-tenant1".to_string(),
            vec![BgpPeerDetail {
                peer: "198.51.100.10".to_string(),
                afi: "ipv4Unicast".to_string(),
                state: "Established".to_string(),
                remote_as: Some(65000),
                uptime: Some("01:23:45".to_string()),
                pfx_rcd: "12".to_string(),
                pfx_snt: "3".to_string(),
            }],
        );
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState {
            tab: Tab::Frr,
            ..Default::default()
        };
        let log = EventLog::new(100, None);

        state.toggle_detail();
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(text.contains("vrf-tenant1"));
        assert!(text.contains("198.51.100.10"));
        assert!(text.contains("prefixes in/out 12/3"));
    }

    // The regression this covers: a popup used to be hard-clipped at the
    // available height with no way to reach the rest - a VRF with dozens
    // of peers simply lost everything past the fold. Now the tail is
    // reachable by scrolling, and the "bottom" sentinel clamps exactly
    // to the last peer.
    #[test]
    fn vrf_popup_scrolls_when_peers_exceed_the_screen() {
        let mut snap = fake_snapshot(0);
        snap.frr_service_active = true;
        snap.frr.bgp_vrf_peers = vec![("vrf-tenant1".to_string(), 30, 30)];
        snap.frr.bgp_vrf_peer_detail.insert(
            "vrf-tenant1".to_string(),
            (0..30)
                .map(|i| BgpPeerDetail {
                    peer: format!("198.51.100.{i}"),
                    afi: "ipv4Unicast".to_string(),
                    state: "Established".to_string(),
                    remote_as: Some(65000),
                    uptime: Some("01:23:45".to_string()),
                    pfx_rcd: "1".to_string(),
                    pfx_snt: "1".to_string(),
                })
                .collect(),
        );
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState {
            tab: Tab::Frr,
            ..Default::default()
        };
        let log = EventLog::new(100, None);

        state.set_tab(Tab::Frr);
        state.toggle_detail();
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(
            text.contains("198.51.100.0"),
            "the first peer must be visible at the top of the popup"
        );
        assert!(
            !text.contains("198.51.100.9 "),
            "the tail peer cannot fit on this small backend - it must be scrolled to"
        );

        state.detail_scroll_bottom();
        let text = draw(&mut terminal, &snap, &log, &mut state);
        // Peers sort by address *string*, so the tail of the list is
        // "... .4 .5 .6 .7 .8 .9" - the last visible peer is .9.
        assert!(
            text.contains("198.51.100.9 "),
            "after scrolling to the bottom the last peer must be visible"
        );
        assert!(
            text.contains("198.51.100.8 "),
            "the popup should be showing the tail of the list now"
        );
        assert!(
            !text.contains("198.51.100.0 "),
            "the first peer should have scrolled off"
        );
    }

    #[test]
    fn select_first_and_last_clamp_to_the_list_bounds() {
        let snap = fake_snapshot(5);
        let backend = TestBackend::new(60, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState {
            tab: Tab::Interfaces,
            ..Default::default()
        };
        let log = EventLog::new(100, None);

        state.select_last();
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(text.contains("eth4"));

        state.select_first();
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(text.contains("eth0"));
    }

    #[test]
    fn overview_selects_sync_services_and_opens_their_journal() {
        let mut snap = fake_snapshot(0);
        snap.sync_units = vec![
            crate::snapshot::SyncUnit {
                label: "FRR config sync",
                service: "frr-config-sync.service",
                health: SyncHealth::Failed {
                    reason: "status stale (6m ago)".to_string(),
                },
            },
            crate::snapshot::SyncUnit {
                label: "Network config sync",
                service: "network-config-sync.service",
                health: SyncHealth::Ok { age_secs: 30 },
            },
        ];
        snap.selected_sync_journal =
            Some("Sep 22 12:00:00 test frr-config-sync[1]: ok".to_string());
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::default();
        let log = EventLog::new(100, None);

        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(text.contains("status stale"));
        assert!(text.contains("last attempt just now"));

        state.move_selection(1); // select the second (Ok) unit
        state.toggle_detail();
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(
            text.contains("frr-config-sync[1]: ok"),
            "the journal popup should show the gathered journal lines"
        );

        state.close_detail();
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(!text.contains("frr-config-sync[1]: ok"));
    }

    #[test]
    fn frr_tab_sorts_problem_vrfs_first_and_shows_routes() {
        let mut snap = fake_snapshot(0);
        snap.frr_service_active = true;
        snap.frr.bgp_vrf_peers = vec![
            ("aaa-ok".to_string(), 2, 2),
            ("zzz-broken".to_string(), 1, 2),
            ("mmm-idle".to_string(), 0, 0),
        ];
        snap.frr
            .vrf_routes
            .insert("zzz-broken".to_string(), (10, 8));
        snap.frr.vrf_routes.insert("aaa-ok".to_string(), (5, 5));
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState {
            tab: Tab::Frr,
            ..Default::default()
        };
        let log = EventLog::new(100, None);

        let text = draw(&mut terminal, &snap, &log, &mut state);
        let broken_pos = text.find("zzz-broken").unwrap();
        let ok_pos = text.find("aaa-ok").unwrap();
        let idle_pos = text.find("mmm-idle").unwrap();
        assert!(
            broken_pos < ok_pos && ok_pos < idle_pos,
            "down-sessions VRF must sort first, established next, idle 0/0 last"
        );
        assert!(text.contains("10/8"), "RIB/FIB counts should be shown");
        assert!(text.contains("Routes (RIB/FIB)"));
    }

    #[test]
    fn events_tab_renders_the_log_and_follows_the_tail() {
        let mut snap = fake_snapshot(0);
        snap.sync_units = vec![crate::snapshot::SyncUnit {
            label: "FRR config sync",
            service: "frr-config-sync.service",
            health: SyncHealth::Ok { age_secs: 10 },
        }];
        let mut log = EventLog::new(100, None);
        log.record(Level::Warn, "12:00:01", "interface eth-trunk went DOWN");
        log.record(Level::Ok, "12:00:30", "interface eth-trunk came UP");

        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState {
            tab: Tab::Events,
            ..Default::default()
        };

        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(text.contains("Event Log"));
        assert!(text.contains("went DOWN"));
        assert!(text.contains("came UP"));

        // Scrolling up suspends follow; returning to the bottom resumes it.
        state.events_scroll_by(-1);
        assert!(!state.events_follow);
        state.events_to_bottom();
        assert!(state.events_follow);
    }

    #[test]
    fn traffic_panel_renders_series_and_rates() {
        let mut snap = fake_snapshot(0);
        snap.traffic = Some(Traffic {
            name: "eth-trunk".to_string(),
            // 125 MB/s = 1 Gbit/s - the label shows network units (bits).
            series: vec![(0, 0), (125_000_000, 125_000_000)],
        });
        let backend = TestBackend::new(100, 35);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::default();
        let log = EventLog::new(100, None);

        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(
            text.contains("Traffic (eth-trunk"),
            "panel title with the trunk's name"
        );
        assert!(text.contains("1.0 Gbit/s"), "current rate label");
        assert!(text.contains("peak"));
    }

    #[test]
    fn traffic_panel_disappears_on_short_terminals() {
        let mut snap = fake_snapshot(0);
        snap.traffic = Some(Traffic {
            name: "eth-trunk".to_string(),
            series: vec![(1000, 1000)],
        });
        // 20 rows: under the 22-row floor from traffic_panel_height.
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::default();
        let log = EventLog::new(100, None);

        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(
            !text.contains("Traffic (eth-trunk"),
            "no room for the graph at this height - it must yield entirely"
        );
    }

    #[test]
    fn header_shows_the_health_verdict_and_data_age() {
        let mut snap = fake_snapshot(0);
        snap.frr_service_active = false; // -> CRIT
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::default();
        let log = EventLog::new(100, None);

        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(text.contains("CRIT (1)"));
        assert!(text.contains("data 2s ago"));

        // Healthy snapshot: badge flips to "healthy".
        let mut snap = fake_snapshot(0);
        snap.frr_service_active = true;
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(text.contains("healthy"));
    }

    #[test]
    fn footer_drops_low_tier_hints_before_important_ones_on_narrow_terms() {
        let snap = fake_snapshot(0);
        // Wide enough for everything:
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::default();
        let log = EventLog::new(100, None);
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(
            text.contains("every 5s"),
            "wide terminal keeps the refresh note"
        );

        // 60 columns: the refresh note (tier 2) and paging/journal hints
        // (tier 1) must give way, but the tab keys and shell hint (tier 0)
        // must survive.
        let backend = TestBackend::new(60, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(!text.contains("every 5s"));
        assert!(text.contains("Shell"));
        assert!(text.contains("Events"));
    }

    #[test]
    fn bootc_panel_shows_reboot_required_for_staged_updates() {
        let mut snap = fake_snapshot(0);
        snap.bootc = Some(BootcStatus {
            booted_image: Some("ghcr.io/mariusbertram/frr-bootc:latest".to_string()),
            booted_digest: Some("sha256:aaaa1111bbbb".to_string()),
            staged_image: Some("ghcr.io/mariusbertram/frr-bootc:latest".to_string()),
            staged_digest: Some("sha256:cccc3333dddd".to_string()),
            raw: None,
        });
        let backend = TestBackend::new(100, 35);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::default();
        let log = EventLog::new(100, None);

        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(text.contains("booted: frr-bootc:latest (aaaa1111bbbb)"));
        assert!(text.contains("reboot required"));
        assert!(text.contains("staged: frr-bootc:latest (cccc3333dddd)"));
    }

    #[test]
    fn frr_summary_shows_bfd_when_sessions_exist() {
        let mut snap = fake_snapshot(0);
        snap.frr_service_active = true;
        snap.frr.bfd_peers = vec![
            BfdPeerDetail {
                peer: "198.51.100.1".to_string(),
                vrf: "default".to_string(),
                status: "up".to_string(),
                uptime: Some("1h 1m".to_string()),
            },
            BfdPeerDetail {
                peer: "198.51.100.2".to_string(),
                vrf: "default".to_string(),
                status: "down".to_string(),
                uptime: Some("5s".to_string()),
            },
        ];
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState {
            tab: Tab::Frr,
            ..Default::default()
        };
        let log = EventLog::new(100, None);

        let text = draw(&mut terminal, &snap, &log, &mut state);
        assert!(
            text.contains("BFD: 1/2 up"),
            "BFD summary line with a red count"
        );
    }

    #[test]
    fn collecting_data_placeholder_renders_before_the_first_snapshot() {
        let backend = TestBackend::new(60, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::default();
        let log = EventLog::new(100, None);
        terminal
            .draw(|frame| render(frame, None, &log, &mut state, 5, None))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("collecting data"));
    }
}
