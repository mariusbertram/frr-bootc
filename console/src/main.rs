//! Router status dashboard - runs permanently on the console (Talos-Linux
//! style), not something a login lands in. It's launched directly as the
//! console handler by frr-console-tty1.service (graphical/VNC,
//! `virtctl vnc`) and frr-console-ttyS0.service (serial,
//! `virtctl console`), which replace getty@tty1.service/
//! serial-getty@ttyS0.service outright (masked in the Containerfile) - see
//! the comment in each of those two unit files for the full boot-time
//! picture.
//!
//! This exists because there is no dashboard/alerting for this VM
//! (console access is the only way in - see the module docs at the top of
//! config-sync/src/bin/frr-config-sync.rs), so the console itself
//! should always be showing the same "is everything actually working"
//! information a human would otherwise have to piece together by hand
//! from several journalctl/vtysh/ip/systemctl calls.
//!
//! Viewing the dashboard needs no authentication - whoever already has
//! console access to this VM is already a privileged operator. 'b' drops
//! into a real, authenticated shell by exec'ing /bin/login (a normal
//! "login:"/password prompt); when that shell eventually exits, so does
//! this process (login replaced it via exec), and the owning unit's
//! Restart=always relaunches the dashboard on the same tty - so the
//! console always lands back here.
//!
//! Data gathering runs on a background thread: a stalled vty socket (its
//! read timeout is 10s - see frr_vty) or a slow `bootc status` used to
//! freeze the whole event loop, key handling included, with no on-screen
//! sign anything was stale. The UI thread now always draws the newest
//! snapshot it has and labels its age in the header (red past 2x the
//! refresh interval), so "the data is old" is visible exactly when it's
//! true.
//!
//! Previously a bash script, rewritten on top of ratatui: `Terminal::draw`
//! diffs each frame against the last one and only touches changed cells,
//! which is what actually solves flicker/stale-content bleed-through
//! (see net.rs/ui.rs comments) - the bash version's manual cursor-position
//! and erase-sequence bookkeeping was working around not having that.
//!
//! Run with stdout not a tty (piped, redirected, manual invocation, etc.)
//! it prints one snapshot and exits - as plain text, or as JSON with
//! `--json`. Both modes exit non-zero when the health verdict (see
//! health.rs) finds anything, which makes this directly usable as a
//! scripted health check (`frr-console --json || alert`) without a
//! second binary.

mod events;
mod frr;
mod health;
mod net;
mod snapshot;
mod system;
mod systemd;
mod ui;

use std::io::{self, IsTerminal, Stdout};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use events::EventLog;
use net::ThroughputSampler;
use snapshot::{Selection, Snapshot};
use ui::{AppState, Tab};

const DEFAULT_REFRESH_SECS: u64 = 5;
/// Where the event log persists across dashboard restarts - written
/// append-only, tail-restored on startup (see events.rs). /run would be
/// wiped at boot, which would defeat the point of having history.
const EVENT_LOG_PATH: &str = "/var/log/frr-console-events.log";
const EVENT_LOG_CAPACITY: usize = 500;
/// `bootc status` spawns a subprocess and inspects the container store -
/// expensive relative to everything else here, and its answer changes on
/// the scale of minutes at the very fastest. Cached worker-side for a
/// minute; every other source is re-read every tick.
const BOOTC_CACHE_TTL: Duration = Duration::from_secs(60);

struct Config {
    json: bool,
    refresh: Duration,
    refresh_secs: u64,
}

fn parse_config() -> Config {
    let mut json = false;
    let mut refresh_secs = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--refresh" => {
                refresh_secs = args.next().and_then(|v| v.parse().ok());
            }
            other => {
                eprintln!(
                    "unknown argument: {other} (usage: frr-console [--json] [--refresh <secs>])"
                );
                std::process::exit(64);
            }
        }
    }
    let refresh_secs = refresh_secs
        .or_else(|| {
            std::env::var("FRR_CONSOLE_REFRESH_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(DEFAULT_REFRESH_SECS)
        .clamp(1, 3600);
    Config {
        json,
        refresh: Duration::from_secs(refresh_secs),
        refresh_secs,
    }
}

fn main() {
    let config = parse_config();

    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        run_once(&config);
        return;
    }

    install_panic_hook();

    let mut terminal = setup_terminal().expect("failed to set up terminal");
    let mut events = EventLog::new(
        EVENT_LOG_CAPACITY,
        Some(std::path::Path::new(EVENT_LOG_PATH)),
    );
    events.record(health::Level::Ok, &system::now_local(), "dashboard started");

    // The gather thread owns the sampler (its rate history must persist
    // across ticks) and the bootc cache; it hands finished snapshots to
    // the UI thread over a channel and never touches the terminal. The
    // only thing flowing the other way is which popups are open (via the
    // shared `Selection`), so on-demand reads track what's on screen.
    let (tx, rx) = mpsc::channel::<Snapshot>();
    let selection = Arc::new(Mutex::new(Selection::default()));
    {
        let selection = Arc::clone(&selection);
        let refresh = config.refresh;
        std::thread::spawn(move || {
            let mut net = ThroughputSampler::new();
            let mut bootc_cache: Option<(Instant, Option<snapshot::BootcStatus>)> = None;
            loop {
                let tick_started = Instant::now();
                let sel = selection.lock().map(|s| s.clone()).unwrap_or_default();
                let bootc = match &bootc_cache {
                    Some((at, status)) if at.elapsed() < BOOTC_CACHE_TTL => status.clone(),
                    _ => {
                        let status = snapshot::bootc_status();
                        bootc_cache = Some((Instant::now(), status.clone()));
                        status
                    }
                };
                let snap = snapshot::gather(&mut net, &sel, bootc);
                if tx.send(snap).is_err() {
                    return; // UI thread gone - nothing left to serve
                }
                // Sleep only the *remaining* interval, so a slow gather
                // (bootc, vty timeouts) doesn't stretch the documented
                // cadence into gather-time + interval.
                std::thread::sleep(refresh.saturating_sub(tick_started.elapsed()));
            }
        });
    }

    run_ui(&mut terminal, &rx, &mut events, &selection, &config);
}

fn run_ui(
    terminal: &mut Term,
    rx: &Receiver<Snapshot>,
    events: &mut EventLog,
    selection: &Arc<Mutex<Selection>>,
    config: &Config,
) {
    let mut state = AppState::default();
    let mut snap: Option<Snapshot> = None;
    // The previous snapshot, kept only for events::diff's transition
    // detection - the UI itself always renders the current one.
    let mut prev: Option<Snapshot> = None;
    let mut last_data: Option<Instant> = None;

    loop {
        loop {
            match rx.try_recv() {
                Ok(new) => {
                    events.diff(prev.as_ref(), &new);
                    prev = snap.take();
                    snap = Some(new);
                    last_data = Some(Instant::now());
                    update_selection(selection, &state, snap.as_ref());
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // The gather thread only exits when the channel's
                    // receiver is gone, i.e. this thread already ended;
                    // if somehow reached anyway, get out rather than
                    // spin on an empty channel.
                    return;
                }
            }
        }

        let age_secs = last_data.as_ref().map(|t| t.elapsed().as_secs());
        let log = &*events;
        let drawn = terminal.draw(|frame| {
            ui::render(
                frame,
                snap.as_ref(),
                log,
                &mut state,
                config.refresh_secs,
                age_secs,
            )
        });
        if let Err(e) = drawn {
            let _ = restore_terminal(terminal);
            eprintln!("failed to draw frame: {e}");
            std::process::exit(1);
        }

        // Redraws happen immediately on any key (switching tabs/scrolling
        // shouldn't wait for the next data refresh), but the data itself
        // still only refreshes on the gather thread's cadence. The poll
        // timeout bounds how long the header's data-age label can lag
        // reality, not the data interval - the gather thread pushes
        // whenever it's ready.
        //
        // Ctrl-C arrives here as a plain KeyEvent (raw mode disables the
        // terminal driver's SIGINT translation), so it just falls through
        // the match below unhandled - same as the bash version's
        // `trap '' INT`, without needing to say so explicitly.
        if event::poll(Duration::from_millis(250)).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if handle_key(key.code, &mut state, terminal) == KeyResult::Login {
                        return;
                    }
                }
                Ok(Event::Mouse(mouse)) => match mouse.kind {
                    MouseEventKind::ScrollUp => scroll_by(&mut state, -2),
                    MouseEventKind::ScrollDown => scroll_by(&mut state, 2),
                    _ => {}
                },
                // Resize needs no state - the loop redraws from
                // frame.area() on the next iteration, ~250ms at most.
                _ => {}
            }
            update_selection(selection, &state, snap.as_ref());
        }
    }
}

fn scroll_by(state: &mut AppState, delta: i32) {
    if state.detail_open() {
        state.detail_scroll_by(delta);
    } else if state.tab == Tab::Events {
        state.events_scroll_by(delta);
    } else {
        state.move_selection(delta);
    }
}

#[derive(PartialEq)]
enum KeyResult {
    Handled,
    Login,
}

fn handle_key(code: KeyCode, state: &mut AppState, terminal: &mut Term) -> KeyResult {
    // While a detail popup is open it owns the scroll keys - moving the
    // underlying selection instead would silently swap the popup to a
    // different item (its content is looked up from the selection), the
    // exact trap that made long VRF peer lists unreadable.
    if state.detail_open() {
        match code {
            KeyCode::Down => state.detail_scroll_by(1),
            KeyCode::Up => state.detail_scroll_by(-1),
            KeyCode::PageDown => state.detail_scroll_by(10),
            KeyCode::PageUp => state.detail_scroll_by(-10),
            KeyCode::Home => state.detail_scroll_top(),
            KeyCode::End => state.detail_scroll_bottom(),
            KeyCode::Enter | KeyCode::Esc => state.close_detail(),
            KeyCode::Char('b') | KeyCode::Char('B') => return login(terminal),
            _ => {}
        }
        return KeyResult::Handled;
    }

    match code {
        KeyCode::Char('b') | KeyCode::Char('B') => return login(terminal),
        // Number keys jump straight to a tab (btop's own way
        // of switching its boxes) - Tab/Shift+Tab and Left/
        // Right cycle, for anyone who'd rather not look down
        // at the number row. Scrolling is plain arrow keys/
        // PageUp/PageDown/Home/End throughout, the same as
        // htop's process list - no vi bindings, to keep this
        // one consistent, recognizable control scheme rather
        // than two overlapping ones.
        KeyCode::Char('1') => state.set_tab(Tab::Overview),
        KeyCode::Char('2') => state.set_tab(Tab::Interfaces),
        KeyCode::Char('3') => state.set_tab(Tab::Frr),
        KeyCode::Char('4') => state.set_tab(Tab::Events),
        KeyCode::Tab | KeyCode::Right => state.next_tab(),
        KeyCode::BackTab | KeyCode::Left => state.prev_tab(),
        KeyCode::Down => {
            if state.tab == Tab::Events {
                state.events_scroll_by(1)
            } else {
                state.move_selection(1)
            }
        }
        KeyCode::Up => {
            if state.tab == Tab::Events {
                state.events_scroll_by(-1)
            } else {
                state.move_selection(-1)
            }
        }
        KeyCode::PageDown => {
            if state.tab == Tab::Events {
                state.events_scroll_by(10)
            } else {
                state.move_selection(10)
            }
        }
        KeyCode::PageUp => {
            if state.tab == Tab::Events {
                state.events_scroll_by(-10)
            } else {
                state.move_selection(-10)
            }
        }
        KeyCode::Home => {
            if state.tab == Tab::Events {
                state.events_to_top()
            } else {
                state.select_first()
            }
        }
        KeyCode::End => {
            if state.tab == Tab::Events {
                state.events_to_bottom()
            } else {
                state.select_last()
            }
        }
        // Enter drills into whichever item is currently highlighted -
        // an interface/VRF on its tab, a sync service's journal on the
        // Overview (see ui::interface_detail_popup/vrf_detail_popup and
        // snapshot::Selection).
        KeyCode::Enter => state.toggle_detail(),
        KeyCode::Esc => state.close_detail(),
        _ => {}
    }
    KeyResult::Handled
}

fn login(terminal: &mut Term) -> KeyResult {
    restore_terminal(terminal).ok();
    // exec_login only returns on failure - success replaces this process
    // entirely.
    eprintln!("/bin/login: {}", exec_login());
    std::thread::sleep(Duration::from_secs(2));
    KeyResult::Login
}

/// What the gather thread should read *extra* data for right now, from
/// whichever popups are open - re-derived after every key event and
/// every new snapshot so closing a popup stops the extra reads on the
/// very next tick.
fn update_selection(selection: &Arc<Mutex<Selection>>, state: &AppState, snap: Option<&Snapshot>) {
    let mut sel = Selection::default();
    if let Some(snap) = snap {
        if state.detail_open() {
            match state.tab {
                Tab::Interfaces => {
                    sel.interface = snap
                        .interfaces
                        .get(state.interfaces_selected())
                        .map(|iface| iface.name.clone());
                }
                Tab::Overview => {
                    sel.sync_service = snap
                        .sync_units
                        .get(state.sync_selected())
                        .map(|unit| unit.service.to_string());
                }
                _ => {}
            }
        }
    }
    if let Ok(mut guard) = selection.lock() {
        *guard = sel;
    }
}

/// `.exec()` only returns when the exec itself failed - on success it
/// replaces this process image entirely (see `CommandExt::exec`), so the
/// return type here is the error, not a `Result`.
fn exec_login() -> io::Error {
    Command::new("/bin/login").exec()
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> io::Result<Term> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    io::stdout().execute(EnableMouseCapture)?;
    let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Term) -> io::Result<()> {
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.backend_mut().execute(DisableMouseCapture)?;
    terminal.show_cursor()
}

/// A panic anywhere in the draw loop must not leave the tty stuck in raw
/// mode / the alternate screen - that would leave the console unusable
/// (and, since this is PID 1 of a systemd unit with Restart=always, stuck
/// looping the same way) until something else resets it.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
        let _ = io::stdout().execute(DisableMouseCapture);
        default_hook(info);
    }));
}

/// One non-interactive snapshot to stdout, then exit - plain text, or
/// JSON with `--json`. Both carry the health verdict; both exit with 0
/// (healthy), 1 (warnings) or 2 (critical findings), so this doubles as
/// a scriptable health check with no second binary.
fn run_once(config: &Config) {
    let mut net = ThroughputSampler::new();
    let selection = Selection::default();
    let bootc = snapshot::bootc_status();
    let snap = snapshot::gather(&mut net, &selection, bootc);

    // Restore whatever history the previous dashboard runs logged, so
    // one-shot modes can answer "what happened before this check" too.
    let log = EventLog::new(
        EVENT_LOG_CAPACITY,
        Some(std::path::Path::new(EVENT_LOG_PATH)),
    );
    let verdict = health::assess(&snap);
    let exit_code = match verdict.level {
        health::Level::Ok => 0,
        health::Level::Warn => 1,
        health::Level::Critical => 2,
    };

    if config.json {
        let report = serde_json::json!({
            "health": verdict,
            "snapshot": &snap,
            "events": log.tail(20),
        });
        println!("{report}");
    } else {
        print_plain(&snap, &verdict, &log);
    }
    std::process::exit(exit_code);
}

fn print_plain(snap: &Snapshot, verdict: &health::Verdict, log: &EventLog) {
    println!("frr-bootc - {} - {}", snap.hostname, snap.now);
    println!("  Uptime: {}", snap.uptime);
    println!("  Health: {}", verdict.badge());
    for problem in &verdict.problems {
        println!("    - [{:?}] {}", problem.level, problem.text);
    }
    println!();

    println!("System");
    println!(
        "  Load average (1/5/15m): {}   ({} CPUs)",
        snap.load_average, snap.cpu_count
    );
    match &snap.memory {
        Some(m) => println!(
            "  Memory: {} used / {} total ({} available)",
            system::fmt_bytes(m.used),
            system::fmt_bytes(m.total),
            system::fmt_bytes(m.available)
        ),
        None => println!("  Memory: unknown"),
    }
    match &snap.disk {
        Some(d) => println!(
            "  Disk (/): {} used / {} total ({}% used)",
            system::fmt_bytes(d.used),
            system::fmt_bytes(d.total),
            d.percent_used
        ),
        None => println!("  Disk (/): unknown"),
    }
    println!();

    if let Some(traffic) = &snap.traffic {
        println!("Traffic ({})", traffic.name);
        match (traffic.latest(), traffic.peak()) {
            (Some((rx, tx)), Some((prx, ptx))) => println!(
                "  rx {} (peak {}) / tx {} (peak {})",
                net::fmt_rate(rx),
                net::fmt_rate(prx),
                net::fmt_rate(tx),
                net::fmt_rate(ptx)
            ),
            _ => println!("  (warming up)"),
        }
        println!();
    }

    println!("Network Interfaces");
    for iface in &snap.interfaces {
        let state = if iface.up { "UP" } else { "DOWN" };
        let rate = match iface.rate {
            Some((rx, tx)) => format!("rx {} / tx {}", net::fmt_rate(rx), net::fmt_rate(tx)),
            None => "throughput: -".to_string(),
        };
        println!("  {:<16} {:<9} {}", iface.name, state, rate);
        if let Some((rx, tx)) = iface.err_rate {
            if rx > 0 || tx > 0 {
                println!("  {:<16} error/drop rate: rx {rx}/s tx {tx}/s", "");
            }
        }
        if !iface.addrs.is_empty() {
            println!("  {:<16} {}", "", iface.addrs.join(" "));
        }
    }
    println!();

    println!("FRR");
    println!(
        "  frr.service: {}",
        if snap.frr_service_active {
            "active"
        } else {
            "inactive"
        }
    );
    if snap.frr_service_active {
        if !snap.frr.daemons.is_empty() {
            println!("  Active daemons: {}", snap.frr.daemons);
        }
        if let Some(routes) = &snap.frr.route_summary {
            println!("  IPv4 RIB: {routes}");
        }
        for error in &snap.frr.query_errors {
            println!("  vty: {error}");
        }
        for bfd in &snap.frr.bfd_peers {
            println!("  BFD {:<16} {:<20} {:<8}", bfd.peer, bfd.vrf, bfd.status);
        }
        if !snap.frr.bgp_vrf_peers.is_empty() {
            println!("  BGP peers (per VRF):");
            for (vrf, estab, total) in &snap.frr.bgp_vrf_peers {
                let routes = snap
                    .frr
                    .vrf_routes
                    .get(vrf)
                    .map_or_else(|| "-".to_string(), |(r, f)| format!("{r}/{f}"));
                println!("    {vrf:<20} {estab}/{total} established   routes {routes}");
                for peer in snap.frr.bgp_vrf_peer_detail.get(vrf).into_iter().flatten() {
                    let as_text = peer
                        .remote_as
                        .map_or_else(|| "-".to_string(), |a| a.to_string());
                    let uptime = peer.uptime.as_deref().unwrap_or("-");
                    println!(
                        "      {:<16} {:<12} {:<11} AS {as_text:<6} up {uptime:<10} pfx in/out {}/{}",
                        peer.peer, peer.afi, peer.state, peer.pfx_rcd, peer.pfx_snt
                    );
                }
            }
        }
    }
    println!();

    println!("Sync Services");
    for unit in &snap.sync_units {
        println!(
            "  {:<24} {:<18} {}",
            unit.label,
            unit.health.status_word(),
            unit.health.detail()
        );
    }
    println!();

    if let Some(bootc) = &snap.bootc {
        println!("System Image (bootc)");
        if bootc.reboot_pending() {
            println!(
                "  staged update: {} - reboot required",
                bootc.staged_digest.as_deref().unwrap_or("?")
            );
        }
        if let Some(raw) = &bootc.raw {
            for line in raw.lines() {
                println!("  {line}");
            }
        } else {
            println!(
                "  booted: {}",
                bootc.booted_digest.as_deref().unwrap_or("?")
            );
        }
    }
    println!();

    let recent = log.tail(10);
    if !recent.is_empty() {
        println!("Recent Events");
        for event in recent {
            println!("  [{}] {:?} {}", event.time, event.level, event.text);
        }
    }
}
