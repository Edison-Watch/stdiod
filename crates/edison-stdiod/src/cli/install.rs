//! `edison-stdiod install` / `uninstall` - register/unregister the
//! per-user supervisor unit.
//!
//! Delegates the platform-specific plumbing to ``crate::platform``;
//! this file exists only to host the clap [`Args`] structs and the
//! [`--purge`] flag so the daemon module stays focused on the
//! supervisor loop.

use anyhow::{Context, Result};
use clap::Args;
use tracing::info;

use crate::paths;
use crate::platform;

#[derive(Debug, Args)]
pub struct InstallArgs {}

#[derive(Debug, Args)]
pub struct UninstallArgs {
    /// Also delete the daemon's persisted config and logs.
    ///
    /// Without `--purge` the user data under `~/.config/edison-stdiod/`
    /// and `~/Library/Logs/edison-stdiod/` is left in place so a later
    /// `install` picks up the previously-saved credentials. Pass
    /// `--purge` to wipe everything stdiod-related.
    #[arg(long)]
    pub purge: bool,
}

pub fn install(_args: InstallArgs) -> Result<()> {
    platform::install()
}

pub fn uninstall(args: UninstallArgs) -> Result<()> {
    platform::uninstall()?;
    // state.json is runtime data (snapshot of the daemon's connection
    // loop, rewritten on every transition by ``StateWriter``). With the
    // daemon stopped, the file becomes a misleading "last seen" record -
    // ``status`` would happily report a Connection / Backend / Servers
    // section sourced from a long-dead supervisor. Remove on every
    // uninstall, even without ``--purge``. config.toml stays because it
    // carries the user's credentials.
    if let Ok(state_path) = paths::state_file() {
        match std::fs::remove_file(&state_path) {
            Ok(()) => info!(path = %state_path.display(), "removed state.json"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(error = %e, "failed to remove state.json; continuing");
            }
        }
    }
    if args.purge {
        purge_user_data().context("purge")?;
    }
    Ok(())
}

fn purge_user_data() -> Result<()> {
    let config_dir = paths::config_dir()?;
    if config_dir.exists() {
        std::fs::remove_dir_all(&config_dir)
            .with_context(|| format!("removing {}", config_dir.display()))?;
        info!(path = %config_dir.display(), "purged config dir");
    }
    let log_dir = paths::log_dir()?;
    if log_dir.exists() {
        std::fs::remove_dir_all(&log_dir)
            .with_context(|| format!("removing {}", log_dir.display()))?;
        info!(path = %log_dir.display(), "purged log dir");
    }
    println!("Purged config and logs.");
    Ok(())
}
