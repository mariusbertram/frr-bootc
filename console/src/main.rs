//! Router status dashboard - runs permanently on the console (Talos-Linux
//! style), not something a login lands in. It's launched directly as the
//! console handler by frr-console-tty1.service (graphical/VNC,
//! `virtctl vnc`) and frr-console-ttyS0.service (serial,
//! `virtctl console`), which replace getty@tty1.service/
//! serial-getty@ttyS0.service outright (masked in the Containerfile) - see
//! the comment in each of those two unit files for the full boot-time
//! picture.
//!
//! This exists because there is no dashboard/alerting for this VM (see the
//! comment at the top of frr-config-sync) - console access is the only way
//! in, so the console itself should always be showing the same "is
//! everything actually working" information a human would otherwise have
//! to piece together by hand from several journalctl/vtysh/ip/systemctl
//! calls.
//!
//! Viewing the dashboard needs no authentication - whoever already has
//! console access to this VM is already a privileged operator. 'b' drops
//! into a real, authenticated shell by exec'ing /bin/login (a normal
//! "login:"/password prompt); when that shell eventually exits, so does
//! this process (login replaced it via exec), and the owning unit's
//! Restart=always relaunches the dashboard on the same tty - so the
//! console always lands back here.
//!
//! Previously a bash script, rewritten on top of ratatui: `Terminal::draw`
//! diffs each frame against the last one and only touches changed cells,
//! which is what actually solves flicker/stale-content bleed-through
//! (see net.rs/ui.rs comments) - the bash version's manual cursor-position
//! and erase-sequence bookkeeping was working around not having that.
//!
//! Run with stdout not a tty (piped, redirected, manual invocation, etc.)
//! it just prints one plain-text snapshot and exits - useful for a quick
//! manual check without taking over a console.

mod frr;
mod net;
mod snapshot;
mod system;
mod systemd;
mod ui;

use std::io::{self, IsTerminal, Stdout};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use net::ThroughputSampler;
use snapshot::Snapshot;
use ui::{AppState, Tab};

const REFRESH_SECS: u64 = 5;
const REFRESH: Duration = Duration::from_secs(REFRESH_SECS);

fn main() {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        let mut net = ThroughputSampler::new();
        print_plain(&snapshot::gather(&mut net));
        return;
    }

    install_panic_hook();

    let mut terminal = setup_terminal().expect("failed to set up terminal");
    let mut net = ThroughputSampler::new();
    let mut state = AppState::default();

    let mut snap = snapshot::gather(&mut net);
    let mut last_refresh = Instant::now();

    loop {
        terminal
            .draw(|frame| ui::render(frame, &snap, &mut state, REFRESH_SECS))
            .expect("failed to draw frame");

        // Redraws happen immediately on any key (switching tabs/scrolling
        // shouldn't wait for the next data refresh), but the data itself
        // still only refreshes on its own REFRESH_SECS cadence - this
        // waits only as long as there's time left until that, not the
        // full interval every time, so a key press right before a
        // refresh was due doesn't delay it.
        //
        // Ctrl-C arrives here as a plain KeyEvent (raw mode disables the
        // terminal driver's SIGINT translation), so it just falls through
        // the match below unhandled - same as the bash version's
        // `trap '' INT`, without needing to say so explicitly.
        let timeout = REFRESH.saturating_sub(last_refresh.elapsed());
        if event::poll(timeout).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read() {
                match key.code {
                    KeyCode::Char('b') | KeyCode::Char('B') => {
                        restore_terminal(&mut terminal).ok();
                        // exec_login only returns on failure - success
                        // replaces this process entirely.
                        eprintln!("/bin/login: {}", exec_login());
                        std::thread::sleep(Duration::from_secs(2));
                        terminal = setup_terminal().expect("failed to re-enter terminal");
                    }
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
                    KeyCode::Tab | KeyCode::Right => state.next_tab(),
                    KeyCode::BackTab | KeyCode::Left => state.prev_tab(),
                    KeyCode::Down => state.scroll_by(1),
                    KeyCode::Up => state.scroll_by(-1),
                    KeyCode::PageDown => state.scroll_by(10),
                    KeyCode::PageUp => state.scroll_by(-10),
                    KeyCode::Home => state.scroll_to_top(),
                    KeyCode::End => state.scroll_to_bottom(),
                    _ => {}
                }
            }
        }

        if last_refresh.elapsed() >= REFRESH {
            snap = snapshot::gather(&mut net);
            last_refresh = Instant::now();
        }
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
    let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Term) -> io::Result<()> {
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
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
        default_hook(info);
    }));
}

fn print_plain(snap: &Snapshot) {
    println!("frr-bootc - {} - {}", snap.hostname, snap.now);
    println!("  Uptime: {}", snap.uptime);
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

    println!("Network Interfaces");
    for iface in &snap.interfaces {
        let state = if iface.up { "UP" } else { "DOWN" };
        let rate = match iface.rate {
            Some((rx, tx)) => format!("rx {} / tx {}", net::fmt_rate(rx), net::fmt_rate(tx)),
            None => "throughput: -".to_string(),
        };
        println!("  {:<16} {:<9} {}", iface.name, state, rate);
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
        if !snap.frr.bgp_vrf_peers.is_empty() {
            println!("  BGP peers (per VRF):");
            for (vrf, estab, total) in &snap.frr.bgp_vrf_peers {
                println!("    {vrf:<20} {estab}/{total} established");
            }
        }
    }
    println!();

    println!("Sync Services");
    for unit in &snap.sync_units {
        let text = match unit.health {
            systemd::SyncHealth::Failed => {
                format!("FAILED (see: journalctl -u {})", unit.service)
            }
            systemd::SyncHealth::TimerNotActive => "timer not active".to_string(),
            systemd::SyncHealth::Ok => "ok".to_string(),
        };
        println!("  {:<24} {}", unit.label, text);
    }
    println!();

    if let Some(status) = &snap.bootc_status {
        println!("System Image (bootc)");
        for line in status.lines() {
            println!("  {line}");
        }
    }
}
