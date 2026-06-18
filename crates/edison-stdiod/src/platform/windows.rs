//! Windows Scheduled Task integration.
//!
//! The Windows analog of the macOS LaunchAgent: a per-user **logon task** that
//! runs the daemon as the logged-in user, in their session, starting at logon
//! and restarting on failure. This is deliberately NOT a Windows Service - a
//! service runs in session 0 as SYSTEM, which would break the daemon's per-user
//! model (it spawns the user's MCP child servers with the user's PATH/env and
//! reads the user's credentials).
//!
//! We register via `schtasks /create /xml` with a Task Scheduler definition
//! because the `schtasks` command-line flags can't express the settings we need
//! (Hidden, restart-on-failure, unlimited run time). The task uses
//! `LogonType=InteractiveToken` so it runs with the user's live token - no
//! stored password, no admin.
//!
//! Idempotent: install deletes any existing task before recreating, mirroring
//! the macOS bootout-before-bootstrap flow.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use tracing::{debug, info, warn};

/// Base name. The live task name appends the user's SID (see `task_name`).
const TASK_BASENAME: &str = "Edison Watch stdiod";

/// CREATE_NO_WINDOW: the daemon is GUI-subsystem, so spawning console programs
/// (schtasks/whoami) would otherwise flash a console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Per-user Scheduled Task name: `<base> <SID>`. Namespacing by SID keeps two
/// accounts on one machine from clobbering each other's task (the name lives in
/// the machine-global root task folder, and `/create /f` overwrites). Falls back
/// to the bare base name if the SID can't be resolved.
fn task_name() -> String {
    match current_user_sid() {
        Some(sid) => format!("{TASK_BASENAME} {sid}"),
        None => TASK_BASENAME.to_string(),
    }
}

/// Current user's SID via `whoami /user /fo csv /nh` -> `"DOMAIN\user","S-1-5-..."`.
fn current_user_sid() -> Option<String> {
    let out = Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .rsplit(',')
        .next()
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| s.starts_with("S-"))
}

/// `DOMAIN\User` (or bare `User`) for the task principal + logon trigger.
fn current_user() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    if domain.is_empty() || user.is_empty() {
        user
    } else {
        format!("{domain}\\{user}")
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Task Scheduler 1.2 XML. Hidden + restart-on-failure + no time limit; runs as
/// the current user with their interactive token at every logon.
fn render_task_xml(binary: &Path, user: &str) -> String {
    let bin = xml_escape(&binary.display().to_string());
    let u = xml_escape(user);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Edison Watch local MCP tunnel daemon</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{u}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{u}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{bin}</Command>
      <Arguments>run</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// schtasks `/create /xml` wants the file as UTF-16LE with a BOM; UTF-8 is
/// rejected as malformed on many Windows builds.
fn write_task_xml(path: &Path, body: &str) -> Result<()> {
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16LE BOM
    for unit in body.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn schtasks(args: &[&str]) -> Result<std::process::Output> {
    debug!(?args, "schtasks");
    Command::new("schtasks")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("failed to invoke schtasks")
}

/// Write the task XML + register it via schtasks, then start it now.
/// Idempotent (deletes any existing task first).
pub fn install() -> Result<()> {
    // Refuse to install without credentials - the daemon would just spin in a
    // reconnect loop. Surface the error now, not later in the logs.
    let cfg = crate::config::PersistedConfig::load()?;
    if cfg.api_key.as_deref().unwrap_or("").is_empty()
        || cfg.backend_url.as_deref().unwrap_or("").is_empty()
    {
        return Err(anyhow!(
            "no credentials on disk. Run `edison-stdiod login --backend ... --api-key ...` first.",
        ));
    }

    let binary = std::env::current_exe().context("could not resolve current exe path")?;
    let user = current_user();
    if user.is_empty() {
        return Err(anyhow!("could not resolve current user (USERNAME unset)"));
    }
    let task = task_name();
    let xml = render_task_xml(&binary, &user);
    let xml_path = std::env::temp_dir().join("edison-stdiod-task.xml");
    write_task_xml(&xml_path, &xml)?;
    info!(path = %xml_path.display(), "wrote scheduled task definition");

    // Idempotent: drop any existing task (stops it too) before recreating, so
    // re-running picks up a moved binary or a changed definition. Also drop a
    // legacy task registered under the old un-SID'd base name (pre-migration).
    let _ = schtasks(&["/delete", "/tn", &task, "/f"]);
    if task != TASK_BASENAME {
        let _ = schtasks(&["/delete", "/tn", TASK_BASENAME, "/f"]);
    }
    let out = schtasks(&[
        "/create",
        "/tn",
        &task,
        "/xml",
        xml_path.to_string_lossy().as_ref(),
        "/f",
    ])?;
    let _ = std::fs::remove_file(&xml_path);
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(anyhow!(
            "schtasks /create failed: {} {}",
            stderr.trim(),
            stdout.trim()
        ));
    }
    info!(task = %task, "scheduled task created");

    // Start now (the RunAtLoad equivalent). If it fails, the logon trigger
    // still starts it at next sign-in, so don't treat it as fatal.
    match schtasks(&["/run", "/tn", &task]) {
        Ok(o) if o.status.success() => {}
        Ok(o) => warn!(
            stderr = %String::from_utf8_lossy(&o.stderr),
            "schtasks /run failed; task will start at next logon"
        ),
        Err(e) => warn!(error = %e, "could not start task now; will start at next logon"),
    }

    println!("Installed scheduled task: {task}");
    println!("Daemon is running. Tail logs with `edison-stdiod logs --follow`.");
    Ok(())
}

/// Stop + remove the task. Idempotent.
pub fn uninstall() -> Result<()> {
    let task = task_name();
    let _ = schtasks(&["/end", "/tn", &task]); // stop a running instance
    // Also clean up a legacy base-named task from before SID namespacing.
    if task != TASK_BASENAME {
        let _ = schtasks(&["/end", "/tn", TASK_BASENAME]);
        let _ = schtasks(&["/delete", "/tn", TASK_BASENAME, "/f"]);
    }
    let out = schtasks(&["/delete", "/tn", &task, "/f"])?;
    if out.status.success() {
        info!(task = %task, "removed scheduled task");
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Benign: the task doesn't exist (already uninstalled).
        let benign = stderr.contains("does not exist")
            || stderr.contains("cannot find")
            || stderr.to_lowercase().contains("the system cannot find");
        if benign {
            info!("no scheduled task to remove");
        } else {
            warn!(stderr = %stderr, "schtasks /delete reported an error; continuing");
        }
    }
    println!("Uninstalled scheduled task. Config + logs left in place (--purge to wipe).");
    Ok(())
}

/// True iff the scheduled task exists.
#[allow(dead_code)] // consumed by the `status` subcommand
pub fn is_installed() -> Result<bool> {
    Ok(schtasks(&["/query", "/tn", &task_name()])?.status.success())
}

/// True iff the task is currently executing (its action process is alive).
/// Parses `schtasks /query /v`'s "Status:" field - English-locale, mirroring
/// the macOS `launchctl print` parse.
#[allow(dead_code)] // consumed by the `status` subcommand
pub fn is_running() -> Result<bool> {
    let out = schtasks(&["/query", "/tn", &task_name(), "/fo", "LIST", "/v"])?;
    if !out.status.success() {
        return Ok(false);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix("Status:") {
            return Ok(rest.trim().eq_ignore_ascii_case("Running"));
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_task_xml_includes_binary_args_and_trigger() {
        let body = render_task_xml(Path::new(r"C:\Apps\edison-stdiod.exe"), r"WS\dimi");
        assert!(body.contains(r"<Command>C:\Apps\edison-stdiod.exe</Command>"));
        assert!(body.contains("<Arguments>run</Arguments>"));
        assert!(body.contains("<LogonTrigger>"));
        assert!(body.contains("<UserId>WS\\dimi</UserId>"));
        assert!(body.contains("<LogonType>InteractiveToken</LogonType>"));
    }

    #[test]
    fn render_task_xml_sets_hidden_restart_and_no_timelimit() {
        let body = render_task_xml(Path::new(r"C:\x.exe"), "u");
        assert!(body.contains("<Hidden>true</Hidden>"));
        assert!(body.contains("<RestartOnFailure>"));
        assert!(body.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
    }

    #[test]
    fn xml_escape_escapes_markup() {
        assert_eq!(xml_escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
    }
}
