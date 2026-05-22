//! Per-OS supervisor integration (launchd / systemd / Scheduled Task).
//!
//! v1 ships macOS only; Linux and Windows ports land in v1.1. Each platform
//! exposes the same surface:
//!
//! - [`install`] - writes the supervisor unit pointing at the current
//!   binary and starts the daemon now. Idempotent (re-running replaces).
//! - [`uninstall`] - stops + removes the unit. Idempotent.
//! - [`is_installed`] / [`is_running`] - used by ``status``.
//!
//! Non-macOS builds compile and surface "not yet supported on this
//! platform" at runtime rather than failing to build, so dev iteration on
//! Linux/Windows still produces a working binary.

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub use macos::{install, uninstall};

// is_installed / is_running are consumed by the `status` subcommand
// (next commit). Pull them through with an allow so the unused-import
// lint doesn't fire until that subcommand lands.
#[cfg(target_os = "macos")]
#[allow(unused_imports)]
pub use macos::{is_installed, is_running};

#[cfg(not(target_os = "macos"))]
mod stub {
    use anyhow::{anyhow, Result};

    pub fn install() -> Result<()> {
        Err(anyhow!(
            "install is only implemented for macOS in v1. \
             See stdiod/REQUIREMENTS.md for the Linux / Windows roadmap.",
        ))
    }

    pub fn uninstall() -> Result<()> {
        Err(anyhow!(
            "uninstall is only implemented for macOS in v1.",
        ))
    }

    pub fn is_installed() -> Result<bool> {
        Ok(false)
    }

    pub fn is_running() -> Result<bool> {
        Ok(false)
    }
}

#[cfg(not(target_os = "macos"))]
pub use stub::{install, is_installed, is_running, uninstall};
