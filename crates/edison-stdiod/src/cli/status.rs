//! `edison-stdiod status` - one-shot snapshot of the daemon's health.
//!
//! Reads the LaunchAgent state from `launchctl` (via
//! `crate::platform::is_installed` / `is_running`) and the daemon's own
//! liveness from `~/.config/edison-stdiod/state.json`. Output is
//! intentionally compact, designed to be tail-grepped by humans rather
//! than parsed by machines - the tray icon should read state.json
//! directly.

use anyhow::Result;
use clap::Args;

use crate::paths;
use crate::platform;
use crate::state::{ConnectionState, ServerStatus, State};

#[derive(Debug, Args)]
pub struct StatusArgs {}

pub fn run(_args: StatusArgs) -> Result<()> {
    let installed = platform::is_installed().unwrap_or(false);
    let running = if installed {
        platform::is_running().unwrap_or(false)
    } else {
        false
    };

    println!("Supervisor unit: {}", supervisor_line(installed, running));
    println!("Config file:     {}", config_path_line());
    println!("State file:      {}", state_path_line());
    println!();

    match State::load() {
        Ok(s) => print_state(&s, running),
        Err(_) => {
            println!("Connection:      no state.json yet (daemon hasn't run, or supervisor not installed)");
        }
    }
    Ok(())
}

fn supervisor_line(installed: bool, running: bool) -> &'static str {
    match (installed, running) {
        (true, true) => "installed, running",
        (true, false) => "installed, not running (check `edison-stdiod logs`)",
        (false, _) => "not installed (run `edison-stdiod install`)",
    }
}

fn config_path_line() -> String {
    match paths::config_file() {
        Ok(p) if p.exists() => p.display().to_string(),
        Ok(p) => format!("{} (missing; run `edison-stdiod login`)", p.display()),
        Err(e) => format!("(unavailable: {e})"),
    }
}

fn state_path_line() -> String {
    match paths::state_file() {
        Ok(p) if p.exists() => p.display().to_string(),
        Ok(p) => format!("{} (missing)", p.display()),
        Err(e) => format!("(unavailable: {e})"),
    }
}

fn print_state(s: &State, running: bool) {
    // If the supervisor isn't reporting a live PID, every field below
    // comes from a state.json that hasn't been rewritten since the
    // daemon's last run. Mark it as stale so callers don't read a
    // dead-since-Tuesday "Connection: connected" as current truth.
    if !running {
        println!("(daemon not currently running - fields below are from the last run)");
    }

    let conn = match s.connection_state {
        ConnectionState::Starting => "starting",
        ConnectionState::Connected => "connected",
        ConnectionState::Reconnecting => "reconnecting",
        ConnectionState::NeedsReauth => "needs reauth (run `edison-stdiod login`)",
        ConnectionState::NeedsUpgrade => "needs upgrade (daemon binary too old for backend)",
    };
    let label = if running { "Connection:" } else { "Last state:" };
    println!("{:<16} {}", label, conn);
    if let Some(url) = &s.backend_url {
        println!("Backend:         {}", url);
    }
    if let Some(d) = &s.device_id {
        let label = s.device_label.as_deref().unwrap_or("");
        if label.is_empty() {
            println!("Device:          {}", d);
        } else {
            println!("Device:          {} ({})", d, label);
        }
    }
    if let Some(ts) = s.last_connected_at {
        println!("Last connected:  {}", ts.to_rfc3339());
    }
    if let Some(err) = &s.last_error {
        println!("Last error:      {}", err);
    }
    if s.servers.is_empty() {
        println!("Servers:         (none)");
    } else {
        println!("Servers:");
        for srv in &s.servers {
            let state = match srv.state {
                ServerStatus::Starting => "starting",
                ServerStatus::Running => "running",
                ServerStatus::Crashed => "crashed",
            };
            let pid = srv.pid.map(|p| format!("pid {p}")).unwrap_or_default();
            println!("  - {:<24} {} {}", srv.name, state, pid);
        }
    }
}
