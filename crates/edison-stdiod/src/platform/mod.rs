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

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "windows")]
pub use windows::{install, uninstall};

#[cfg(target_os = "windows")]
#[allow(unused_imports)]
pub use windows::{is_installed, is_running};

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "linux")]
pub use linux::{install, uninstall};

#[cfg(target_os = "linux")]
#[allow(unused_imports)]
pub use linux::{is_installed, is_running};

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod stub {
    use anyhow::{anyhow, Result};

    pub fn install() -> Result<()> {
        Err(anyhow!(
            "install is implemented for macOS, Linux (systemd), and Windows. \
             This OS has no supervisor integration - run `edison-stdiod run` \
             directly. See stdiod/REQUIREMENTS.md.",
        ))
    }

    pub fn uninstall() -> Result<()> {
        Err(anyhow!(
            "uninstall is implemented for macOS, Linux (systemd), and Windows.",
        ))
    }

    pub fn is_installed() -> Result<bool> {
        Ok(false)
    }

    pub fn is_running() -> Result<bool> {
        Ok(false)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub use stub::{install, is_installed, is_running, uninstall};
