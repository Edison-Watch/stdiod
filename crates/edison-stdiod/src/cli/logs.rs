//! `edison-stdiod logs [--follow]` - print or tail the daemon log.
//!
//! On macOS/Linux we shell out to `tail` (its `-F` follow-by-name + retry
//! handles the file vanishing/reappearing across uninstall/install cycles).
//! Windows has no `tail`, so there we use a small Rust tailer that prints the
//! last N lines and, with `--follow`, polls for appended bytes and reopens the
//! file if it's rotated or recreated.

use anyhow::Result;
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

    #[cfg(not(target_os = "windows"))]
    {
        tail_via_command(&path, &args)
    }
    #[cfg(target_os = "windows")]
    {
        tail_in_rust(&path, &args)
    }
}

#[cfg(not(target_os = "windows"))]
fn tail_via_command(path: &std::path::Path, args: &LogsArgs) -> Result<()> {
    use anyhow::anyhow;
    use std::process::Command;

    let mut cmd = Command::new("tail");
    cmd.arg("-n").arg(args.lines.to_string());
    if args.follow {
        // `-F` is BSD `tail`'s follow-by-name + retry combo (equivalent to
        // GNU's `--follow=name --retry`). macOS ships BSD tail; GNU tail also
        // accepts `-F`, so this is portable across macOS/Linux.
        cmd.arg("-F");
    }
    cmd.arg(path);

    let status = cmd
        .status()
        .map_err(|e| anyhow!("failed to invoke tail: {e}"))?;
    if !status.success() {
        return Err(anyhow!("tail exited with status {status}"));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn tail_in_rust(path: &std::path::Path, args: &LogsArgs) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};

    // Print the last N lines.
    let initial = std::fs::read(path).unwrap_or_default();
    let text = String::from_utf8_lossy(&initial);
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(args.lines as usize);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in &all[start..] {
        writeln!(out, "{line}")?;
    }
    out.flush()?;

    if !args.follow {
        return Ok(());
    }

    // Follow: poll for appended bytes; reopen on truncation/rotation (the file
    // is deleted+recreated across uninstall/install).
    let mut pos = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let mut f = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => {
                pos = 0; // file gone (uninstall); resume from start when it returns
                continue;
            }
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < pos {
            pos = 0; // truncated/rotated
        }
        if len > pos {
            f.seek(SeekFrom::Start(pos))?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            out.write_all(&buf)?;
            out.flush()?;
            pos = len;
        }
    }
}
