//! `edison-stdiod logs [--follow]` - print or tail
//! `~/Library/Logs/edison-stdiod/daemon.log`.
//!
//! Shells out to `tail` because that's the path everyone already knows
//! and it gets `--follow --retry` semantics for free (the file vanishes
//! and reappears across `uninstall` / `install` cycles). Avoiding a
//! Rust-side tailer also keeps the binary small and dodges
//! cross-platform inotify/kqueue plumbing.

use std::process::Command;

use anyhow::{anyhow, Result};
use clap::Args;

use crate::paths;

#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Follow the log file in real time (like `tail -f`). Survives the
    /// log being rotated or recreated.
    #[arg(short = 'f', long)]
    pub follow: bool,
    /// Number of trailing lines to print before following. Mirrors
    /// `tail -n`.
    #[arg(short = 'n', long, default_value_t = 200)]
    pub lines: u32,
}

pub fn run(args: LogsArgs) -> Result<()> {
    let path = paths::daemon_log_file()?;
    if !path.exists() {
        eprintln!(
            "No daemon.log at {}. Has the daemon run yet? Try `edison-stdiod install`.",
            path.display(),
        );
        return Ok(());
    }

    let mut cmd = Command::new("tail");
    cmd.arg("-n").arg(args.lines.to_string());
    if args.follow {
        // `-F` is BSD `tail`'s follow-by-name + retry combo (equivalent
        // to GNU's `--follow=name --retry`). macOS ships BSD tail; GNU
        // tail also accepts `-F`, so this is portable.
        cmd.arg("-F");
    }
    cmd.arg(&path);

    let status = cmd
        .status()
        .map_err(|e| anyhow!("failed to invoke tail: {e}"))?;
    if !status.success() {
        return Err(anyhow!("tail exited with status {status}"));
    }
    Ok(())
}
