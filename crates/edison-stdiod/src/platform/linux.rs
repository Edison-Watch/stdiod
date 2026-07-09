//! Linux systemd **user** service integration.
//!
//! Writes a unit to `~/.config/systemd/user/edison-stdiod.service` and
//! manages it with `systemctl --user` (no `sudo`, no system-level unit) so
//! the daemon runs as the logged-in user with their HOME, PATH, and
//! secrets - the exact analog of the macOS LaunchAgent.
//!
//! The unit sets:
//!
//! - `Restart=on-failure`     - systemd restarts the binary on crash
//!   (the daemon also has its own reconnect loop, so only a full process
//!   exit needs this).
//! - `WantedBy=default.target` - start on login. We deliberately do **not**
//!   call `loginctl enable-linger`, so the daemon runs only while the user
//!   has a session - matching the macOS LaunchAgent and the Windows
//!   "run only when logged on" task.
//!
//! ## systemd is not guaranteed
//!
//! Most distros use systemd, but Alpine (OpenRC - and our prime static-musl
//! target), Void (runit), Artix/Devuan (OpenRC/runit/sysvinit) and bare
//! containers do not. We therefore **detect** a reachable systemd user
//! instance and, when absent, return an actionable error rather than writing
//! a unit that never starts. The daemon itself still runs fine via
//! `edison-stdiod run` under any init system or process manager.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use tracing::{debug, info, warn};

use crate::paths;

const UNIT_NAME: &str = "edison-stdiod.service";

/// `~/.config/systemd/user/edison-stdiod.service`.
fn unit_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("HOME not set"))?;
    let dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(UNIT_NAME))
}

/// Run `systemctl --user <args>`.
fn systemctl(args: &[&str]) -> Result<std::process::Output> {
    debug!(?args, "systemctl --user");
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("failed to invoke systemctl (is it installed?)")?;
    Ok(out)
}

/// True iff a systemd **user** instance is reachable for this session.
///
/// Checks two things: that `systemctl` is invokable at all, and that a user
/// bus answers. When there is no user manager (OpenRC/runit distros, many
/// containers) `systemctl --user` fails to connect to the bus - we detect
/// that distinct failure so callers can degrade gracefully.
pub fn systemd_user_available() -> bool {
    // `is-system-running` is read-only and answers whenever the bus is up
    // (it may report "degraded"/"starting" with a non-zero exit, which is
    // still "available" - so we gate on the bus-connection error text, not
    // the exit code).
    match systemctl(&["is-system-running"]) {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            !stderr.contains("Failed to connect to bus")
        }
        // systemctl missing / not executable -> not a systemd system.
        Err(_) => false,
    }
}

fn render_unit(binary: &Path, log: &Path) -> String {
    // systemd `%h` expands to the user's home at load time. We set an
    // explicit PATH for the same reason macOS does: a user-service inherits
    // a minimal environment, which breaks child MCP spawns (npx/uvx/etc.)
    // that live in ~/.local/bin or /usr/local/bin. The daemon also augments
    // child PATH at spawn, but seeding it here keeps the common case working
    // out of the box.
    //
    // StandardOutput/StandardError=append: route the daemon's stderr into the
    // same `daemon.log` file `edison-stdiod logs` tails (the daemon logs to
    // stderr on Linux, and under a user service that would otherwise only land
    // in journald). Mirrors the macOS plist's StandardErrorPath. `append:`
    // needs systemd v240+ (2018); its parent dir is created at install time.
    let bin = binary.display();
    let log = log.display();
    format!(
        "[Unit]\n\
         Description=Edison Watch stdio tunnel daemon\n\
         Documentation=https://edison.watch\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=\"{bin}\" run\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         Environment=PATH=%h/.local/bin:%h/bin:/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin\n\
         StandardOutput=append:{log}\n\
         StandardError=append:{log}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
    )
}

fn write_unit(path: &Path, body: &str) -> Result<()> {
    let tmp = path.with_extension("service.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming -> {}", path.display()))?;
    Ok(())
}

/// Write the unit + `systemctl --user enable --now`. Idempotent.
pub fn install() -> Result<()> {
    // Refuse to install without credentials - the daemon would just spin in
    // a reconnect loop. Surface it now, not later in the logs. (Mirrors the
    // macOS path.)
    let cfg = crate::config::PersistedConfig::load()?;
    if cfg.api_key.as_deref().unwrap_or("").is_empty()
        || cfg.backend_url.as_deref().unwrap_or("").is_empty()
    {
        return Err(anyhow!(
            "no credentials on disk. Run `edison-stdiod login --backend ... --api-key ...` first.",
        ));
    }

    if !systemd_user_available() {
        return Err(anyhow!(
            "no systemd user instance is available on this system.\n\
             edison-stdiod still runs fine without it - start the daemon under\n\
             your init system or process manager instead, e.g.:\n\
             \n    edison-stdiod run\n\n\
             (OpenRC/runit/s6 service files and containers should exec that\n\
             command directly.)",
        ));
    }

    let binary = std::env::current_exe().context("could not resolve current exe path")?;
    let log = paths::daemon_log_file()?;
    // `StandardError=append:` writes to the file but does not create its
    // parent directory - do that here so the first start doesn't fail.
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating log dir {}", parent.display()))?;
    }
    let unit = unit_path()?;
    let body = render_unit(&binary, &log);
    write_unit(&unit, &body)?;
    info!(path = %unit.display(), "wrote systemd user unit");

    // daemon-reload so a re-install picks up a moved binary or edited unit.
    let reload = systemctl(&["daemon-reload"])?;
    if !reload.status.success() {
        warn!(
            stderr = %String::from_utf8_lossy(&reload.stderr),
            "systemctl --user daemon-reload reported an error; continuing"
        );
    }

    // enable = start on login; --now = start immediately.
    let out = systemctl(&["enable", "--now", UNIT_NAME])?;
    if !out.status.success() {
        return Err(anyhow!(
            "systemctl --user enable --now {UNIT_NAME} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    info!(unit = UNIT_NAME, "systemd user service enabled and started");
    println!("Installed systemd user service: {}", unit.display());
    println!("Daemon is running. Tail logs with `edison-stdiod logs --follow`.");
    Ok(())
}

/// `systemctl --user disable --now` + remove the unit. Idempotent.
pub fn uninstall() -> Result<()> {
    // disable --now even if the unit file is already gone; ignore failures
    // (e.g. "not loaded") so the call is safe to repeat. Only attempt it
    // when a user bus exists - otherwise there is nothing to talk to.
    if systemd_user_available() {
        let out = systemctl(&["disable", "--now", UNIT_NAME])?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            debug!(stderr = %stderr, "systemctl --user disable reported an error; continuing");
        }
    }

    let unit = unit_path()?;
    match std::fs::remove_file(&unit) {
        Ok(()) => info!(path = %unit.display(), "removed systemd user unit"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("no systemd user unit to remove");
        }
        Err(e) => return Err(e).with_context(|| format!("removing {}", unit.display())),
    }

    if systemd_user_available() {
        let _ = systemctl(&["daemon-reload"]);
    }
    println!("Uninstalled systemd user service. Config + logs left in place (--purge to wipe).");
    Ok(())
}

/// True iff the unit file exists on disk - the canonical "did install run"
/// signal (cheap, no bus round-trip).
#[allow(dead_code)] // consumed by the `status` subcommand
pub fn is_installed() -> Result<bool> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return Ok(false),
    };
    Ok(home.join(".config/systemd/user").join(UNIT_NAME).exists())
}

/// True iff `systemctl --user is-active` reports the unit active.
#[allow(dead_code)] // consumed by the `status` subcommand
pub fn is_running() -> Result<bool> {
    if !systemd_user_available() {
        return Ok(false);
    }
    // `is-active` prints "active" / "inactive" / "failed" to stdout and sets
    // the exit code accordingly. The text is locale-independent (an enum
    // serialisation), unlike the launchctl/schtasks parses on other OSes.
    let out = systemctl(&["is-active", UNIT_NAME])?;
    let state = String::from_utf8_lossy(&out.stdout);
    Ok(state.trim() == "active")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_unit_includes_execstart_and_install() {
        let body = render_unit(
            Path::new("/usr/local/bin/edison-stdiod"),
            Path::new("/home/me/.local/state/edison-stdiod/daemon.log"),
        );
        assert!(body.contains("ExecStart=\"/usr/local/bin/edison-stdiod\" run"));
        assert!(body.contains("Restart=on-failure"));
        assert!(body.contains("WantedBy=default.target"));
        assert!(body.contains("[Service]"));
    }

    #[test]
    fn render_unit_seeds_path_for_child_spawns() {
        let body = render_unit(Path::new("/bin/x"), Path::new("/tmp/x.log"));
        assert!(body.contains("Environment=PATH="));
        assert!(body.contains("%h/.local/bin"));
        assert!(body.contains("/usr/local/bin"));
    }

    #[test]
    fn render_unit_routes_logs_to_daemon_log() {
        let body = render_unit(Path::new("/bin/x"), Path::new("/tmp/x.log"));
        assert!(body.contains("StandardOutput=append:/tmp/x.log"));
        assert!(body.contains("StandardError=append:/tmp/x.log"));
    }

    #[test]
    fn render_unit_has_no_linger_directive() {
        // We intentionally rely on the default (run-while-logged-in)
        // semantics; lingering would change that and must not creep in.
        let body = render_unit(Path::new("/bin/x"), Path::new("/tmp/x.log"));
        assert!(!body.to_lowercase().contains("linger"));
    }
}
