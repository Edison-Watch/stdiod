//! macOS LaunchAgent integration.
//!
//! Writes a plist to `~/Library/LaunchAgents/watch.edison.stdiod.plist` and
//! loads it via the modern `launchctl bootstrap gui/$UID …` flow (not the
//! deprecated `launchctl load`). All operations are per-user - no `sudo`,
//! no system-level LaunchDaemon - so the daemon runs as the logged-in user
//! and has access to the user's keychain and HOME.
//!
//! The bundled plist sets:
//!
//! - `RunAtLoad=true`     - start the daemon now and on every login.
//! - `KeepAlive=true`     - launchd restarts the binary on crash.
//! - `StandardOutPath` / `StandardErrorPath` route stdout+stderr into
//!   `~/Library/Logs/edison-stdiod/daemon.log` so `edison-stdiod logs`
//!   has something to tail.
//!
//! Bootstrapping is idempotent: install always boots-out the existing unit
//! (if any) before bootstrapping the fresh one, so re-running after a
//! credential rotation or a binary path change reliably picks up changes.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use tracing::{debug, info, warn};

use crate::paths;

const LABEL: &str = "watch.edison.stdiod";
const PLIST_FILENAME: &str = "watch.edison.stdiod.plist";

/// `~/Library/LaunchAgents/watch.edison.stdiod.plist`.
fn plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("HOME not set"))?;
    let dir = home.join("Library/LaunchAgents");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(PLIST_FILENAME))
}

/// `gui/<uid>` - the modern launchctl domain target for per-user agents.
fn user_domain() -> String {
    // SAFETY: getuid is always available on macOS via libc::getuid; but to
    // avoid pulling libc just for this we shell out to `id -u`. The
    // overhead (one fork) only happens at install/uninstall/status time,
    // never on the hot path.
    let out = Command::new("id").arg("-u").output();
    let uid = out
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "0".to_string());
    format!("gui/{uid}")
}

/// `gui/<uid>/watch.edison.stdiod` - full service target for
/// `launchctl print` / `launchctl kickstart`.
fn service_target() -> String {
    format!("{}/{}", user_domain(), LABEL)
}

fn render_plist(binary: &Path, log_path: &Path) -> String {
    // Hand-rolled XML rather than pulling a plist crate. The schema is
    // documented at developer.apple.com/library/.../launchd.plist.5.html
    // and the fields we set are stable across macOS releases.
    //
    // ``EnvironmentVariables.PATH`` - launchd's per-user default PATH is
    // ``/usr/bin:/bin:/usr/sbin:/sbin``, which is fine for the daemon's
    // own runtime but breaks every child it spawns (railway CLI, uvx,
    // npx-wrapped MCP servers, etc.) because Homebrew lives at
    // ``/opt/homebrew/bin`` and many node tools live at
    // ``/usr/local/bin``. Without this override every ``Command::new("railway")``
    // -style spawn fails with "command not found" and the backend's
    // ``import_server`` hangs on the missing child until it 60s-times out.
    // Order matches Homebrew's own LaunchAgents - Homebrew dirs first so
    // user-installed tools shadow system equivalents (e.g. brew's python3
    // over the macOS-provided one).
    let bin = binary.display();
    let log = log_path.display();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{bin}</string>
    <string>run</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ProcessType</key>
  <string>Background</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
    )
}

fn write_plist(path: &Path, body: &str) -> Result<()> {
    let tmp = path.with_extension("plist.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming -> {}", path.display()))?;
    Ok(())
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    debug!(?args, "launchctl");
    let out = Command::new("launchctl")
        .args(args)
        .output()
        .context("failed to invoke launchctl")?;
    Ok(out)
}

/// `bootout` the current unit if loaded. Ignores "not loaded" / "not
/// found" so the call is safe pre-install.
fn bootout_quiet() -> Result<()> {
    let out = launchctl(&["bootout", &service_target()])?;
    if !out.status.success() {
        // Common case: nothing is loaded yet. launchctl returns 113 /
        // "Could not find specified service". Don't surface that as an
        // error.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let benign = stderr.contains("Could not find specified service")
            || stderr.contains("No such process")
            || out.status.code() == Some(113);
        if !benign {
            warn!(stderr = %stderr, "launchctl bootout reported an error; continuing");
        }
    }
    Ok(())
}

/// Write the plist + `launchctl bootstrap` it. Idempotent.
pub fn install() -> Result<()> {
    // Refuse to install if there are no credentials on disk - the daemon
    // would just spin in a reconnect loop. The user sees this error
    // immediately rather than discovering it in the logs later.
    let cfg = crate::config::PersistedConfig::load()?;
    if cfg.api_key.as_deref().unwrap_or("").is_empty()
        || cfg.backend_url.as_deref().unwrap_or("").is_empty()
    {
        return Err(anyhow!(
            "no credentials on disk. Run `edison-stdiod login --backend ... --api-key ...` first.",
        ));
    }

    let binary = std::env::current_exe().context("could not resolve current exe path")?;
    let log = paths::daemon_log_file()?;
    let plist = plist_path()?;
    let body = render_plist(&binary, &log);
    write_plist(&plist, &body)?;
    info!(path = %plist.display(), "wrote LaunchAgent plist");

    // Always bootout before bootstrap so re-running install picks up a
    // moved binary, an updated plist, or a previously-broken unit.
    bootout_quiet()?;
    let out = launchctl(&[
        "bootstrap",
        &user_domain(),
        plist.to_string_lossy().as_ref(),
    ])?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(anyhow!("launchctl bootstrap failed: {}", stderr.trim()));
    }
    info!(label = LABEL, "LaunchAgent loaded");
    println!("Installed LaunchAgent: {}", plist.display());
    println!("Daemon is running. Tail logs with `edison-stdiod logs --follow`.");
    Ok(())
}

/// `launchctl bootout` + remove the plist. Idempotent.
pub fn uninstall() -> Result<()> {
    bootout_quiet()?;
    let plist = plist_path()?;
    match std::fs::remove_file(&plist) {
        Ok(()) => info!(path = %plist.display(), "removed LaunchAgent plist"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("no LaunchAgent plist to remove");
        }
        Err(e) => return Err(e).with_context(|| format!("removing {}", plist.display())),
    }
    println!("Uninstalled LaunchAgent. Config + logs left in place (--purge to wipe).");
    Ok(())
}

/// True iff the plist exists on disk. We don't shell out to `launchctl
/// print` here because that's slow and the file's presence is the
/// canonical "did install run" signal.
#[allow(dead_code)] // wired up by the `status` subcommand (next commit)
pub fn is_installed() -> Result<bool> {
    Ok(plist_path()?.exists())
}

/// True iff `launchctl print` reports the service is loaded AND has a
/// running PID. Used by ``status`` to distinguish "installed but not
/// running" from "running healthily".
#[allow(dead_code)] // wired up by the `status` subcommand (next commit)
pub fn is_running() -> Result<bool> {
    let out = launchctl(&["print", &service_target()])?;
    if !out.status.success() {
        return Ok(false);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // launchctl print emits "pid = 12345" when the service is alive and
    // "state = running" alongside it. Either is sufficient; we check for
    // a non-zero pid since "state = running" briefly appears during
    // bootstrap before a pid is assigned.
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("pid =") {
            if rest.trim().parse::<u32>().is_ok() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_plist_includes_label_and_paths() {
        let body = render_plist(
            Path::new("/usr/local/bin/edison-stdiod"),
            Path::new("/tmp/x.log"),
        );
        assert!(body.contains("<string>watch.edison.stdiod</string>"));
        assert!(body.contains("<string>/usr/local/bin/edison-stdiod</string>"));
        assert!(body.contains("<string>run</string>"));
        assert!(body.contains("<string>/tmp/x.log</string>"));
        assert!(body.contains("<key>RunAtLoad</key>"));
        assert!(body.contains("<key>KeepAlive</key>"));
    }

    #[test]
    fn render_plist_extends_path_for_child_spawns() {
        let body = render_plist(Path::new("/bin/x"), Path::new("/tmp/x.log"));
        assert!(body.contains("<key>EnvironmentVariables</key>"));
        assert!(body.contains("/opt/homebrew/bin"));
        assert!(body.contains("/usr/local/bin"));
    }

    #[test]
    fn render_plist_is_valid_xml_prologue() {
        let body = render_plist(Path::new("/bin/x"), Path::new("/tmp/x.log"));
        assert!(body.starts_with("<?xml version=\"1.0\""));
        assert!(body.contains("<!DOCTYPE plist"));
        assert!(body.trim_end().ends_with("</plist>"));
    }
}
