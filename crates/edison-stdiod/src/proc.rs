//! Subprocess management for stdio MCP servers.
//!
//! Each [`ChildServer`] owns a child process plus two pump tasks:
//! - **outbound** pump: receives [`McpFrame`]s addressed to this server and
//!   writes the JSON-RPC body (with a trailing newline) to the child's
//!   stdin.
//! - **inbound** pump: reads newline-delimited JSON-RPC frames from the
//!   child's stdout and sends them as [`TunnelFrame::McpFrame`] over the
//!   tunnel.
//!
//! When the child exits or the inbound pump errors, the inbound pump emits a
//! `tunnel_error { code: "server_offline" }` so the backend's transport
//! fails in-flight requests cleanly - this is the load-bearing pattern
//! surfaced by the v0 spike (see `stdiod/ARCHITECTURE.md`).

use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use tunnel_protocol::{DesiredServer, McpFrame, TunnelError, TunnelFrame};

use crate::tunnel::OutgoingHandle;

/// Build the base `Command` for a child MCP server.
///
/// On Unix this is just `Command::new(program).args(args)` - the inherited PATH
/// (set wide by the macOS LaunchAgent plist) resolves `npx`/`uvx`. On Windows we
/// do two extra things: (1) append the bundled runtimes dir to PATH so child
/// servers find `npx`/`uvx` even when Node/uv aren't installed (system copies
/// still win - bundled is a fallback), and (2) resolve the program against
/// PATHEXT and run `.cmd`/`.bat` shims (like `npx.cmd`) through `cmd /c`, because
/// CreateProcess can't launch them directly and `Command::new` doesn't apply
/// PATHEXT.
fn build_child_command(program: &str, args: &[String]) -> Command {
    #[cfg(target_os = "linux")]
    {
        // Resolve against the augmented PATH (interactive login-shell PATH ∪ the
        // daemon PATH) so version-manager node/npx (nvm/fnm/volta) is found, and
        // set it on the child so a resolved `npx` can in turn find `node`. Linux
        // only: the systemd `--user` unit's PATH omits shell-rc additions.
        let path = linux::augmented_path();
        let mut cmd = match linux::resolve_program(program, &path) {
            Some(abs) => Command::new(abs),
            None => Command::new(program),
        };
        cmd.args(args);
        cmd.env("PATH", &path);
        cmd
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // macOS / other Unix: inherit the daemon PATH as-is (the macOS
        // LaunchAgent plist already sets it wide enough for npx/uvx).
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd
    }
    #[cfg(windows)]
    {
        let path = win::augmented_path();
        let resolved = win::resolve_program(program, &path);
        tracing::info!(
            program,
            resolved = ?resolved,
            runtime_dirs = ?win::bundled_runtime_dirs(),
            "windows child command resolution"
        );
        let mut cmd = match resolved {
            // `.cmd`/`.bat` must run through cmd.exe. Use `/d /s /c "<line>"`
            // and raw_arg so cmd strips only the outer quotes and runs the rest
            // verbatim - the standard way to invoke a batch file with args that
            // may contain spaces (e.g. the fs server's directory path), which
            // plain `cmd /c "path" "arg"` mis-parses.
            Some(p) if win::is_batch(&p) => {
                let mut c = Command::new("cmd");
                c.raw_arg("/d")
                    .raw_arg("/s")
                    .raw_arg("/c")
                    .raw_arg(format!("\"{}\"", win::build_cmd_line(&p, args)));
                c
            }
            Some(p) => {
                let mut c = Command::new(&p);
                c.args(args);
                c
            }
            None => {
                tracing::warn!(
                    program,
                    "child program not found on system PATH or bundled runtimes"
                );
                let mut c = Command::new(program);
                c.args(args);
                c
            }
        };
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd.env("PATH", path);
        cmd
    }
}

/// One-time child-spawn environment setup, run once at daemon startup.
///
/// On Linux, resolves the user's *interactive login-shell* PATH so child MCP
/// servers can find node/npx/uvx installed via a version manager (nvm/fnm/volta/
/// asdf) - whose PATH additions live in shell rc files that the systemd `--user`
/// service never sources. No-op on macOS (the LaunchAgent plist already sets a
/// wide PATH) and Windows (bundled-runtimes PATH augmentation in `win`).
pub(crate) async fn init_child_env() {
    #[cfg(target_os = "linux")]
    linux::init_augmented_path().await;
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashSet;
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::time::Duration;

    // Login-shell PATH ∪ daemon PATH, resolved once by init_augmented_path().
    static AUGMENTED_PATH: OnceLock<OsString> = OnceLock::new();

    /// Resolve + cache the augmented PATH. Idempotent; best-effort (on failure
    /// or timeout the cache stays empty and augmented_path() uses the daemon PATH).
    pub async fn init_augmented_path() {
        if AUGMENTED_PATH.get().is_some() {
            return;
        }
        let merged = merge_paths(login_shell_path().await, std::env::var_os("PATH"));
        let _ = AUGMENTED_PATH.set(merged);
    }

    /// PATH to use for child spawns: the cached login-shell∪daemon PATH, or the
    /// daemon PATH if init hasn't run / the shell probe failed.
    pub fn augmented_path() -> OsString {
        AUGMENTED_PATH
            .get()
            .cloned()
            .unwrap_or_else(|| std::env::var_os("PATH").unwrap_or_default())
    }

    /// Capture `$PATH` as the user's interactive login shell sees it (so
    /// nvm/fnm/volta setup in rc files is applied). Markers isolate the value
    /// from any banner an rc file prints to stdout; stdin is null so an rc that
    /// reads input can't hang; the probe is bounded by a timeout.
    async fn login_shell_path() -> Option<OsString> {
        const START: &str = "__EW_PATH_START__";
        const END: &str = "__EW_PATH_END__";
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/bash"));
        let mut cmd = tokio::process::Command::new(&shell);
        cmd.arg("-lic")
            .arg(format!("printf '{START}%s{END}' \"$PATH\""))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let out = tokio::time::timeout(Duration::from_secs(5), cmd.output())
            .await
            .ok()? // timed out
            .ok()?; // spawn/exec failed
        let stdout = String::from_utf8_lossy(&out.stdout);
        let path = stdout.split_once(START)?.1.split_once(END)?.0;
        (!path.is_empty()).then(|| OsString::from(path))
    }

    /// Merge two PATH values: entries from `primary` first, then any from
    /// `secondary` not already present. Order-preserving, de-duplicated.
    fn merge_paths(primary: Option<OsString>, secondary: Option<OsString>) -> OsString {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        for src in [primary, secondary].into_iter().flatten() {
            for dir in std::env::split_paths(&src) {
                if seen.insert(dir.clone()) {
                    dirs.push(dir);
                }
            }
        }
        std::env::join_paths(dirs).unwrap_or_default()
    }

    /// Resolve `program` to an absolute path against `path`. A name that already
    /// contains '/' is returned as-is; a bare name is searched on each PATH dir.
    /// None when not found (the caller keeps the bare name, so spawn still errors
    /// with a clear "not found" rather than silently doing nothing).
    pub fn resolve_program(program: &str, path: &OsStr) -> Option<PathBuf> {
        if program.contains('/') {
            return Some(PathBuf::from(program));
        }
        std::env::split_paths(path)
            .map(|dir| dir.join(program))
            .find(|cand| is_executable(cand))
    }

    fn is_executable(p: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};

    /// Bundled runtimes live next to the daemon exe (`resources/bin/edison-stdiod.exe`
    /// -> `resources/runtimes/{node,uv}`). Also accept `<exe_dir>/runtimes/*` for
    /// standalone testing. Returns the dirs that actually exist.
    pub fn bundled_runtime_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            // Build candidate bases WITHOUT a ".." component (keeps the paths
            // clean for PATH/cmd): <exe_dir>/runtimes (standalone) and
            // <exe_dir>/../runtimes == resources/runtimes (bundled).
            let mut bases: Vec<PathBuf> = Vec::new();
            if let Some(bin) = exe.parent() {
                bases.push(bin.join("runtimes"));
                if let Some(res) = bin.parent() {
                    bases.push(res.join("runtimes"));
                }
            }
            for base in bases {
                for sub in ["node", "uv"] {
                    let d = base.join(sub);
                    if d.is_dir() {
                        dirs.push(d);
                    }
                }
            }
        }
        dirs
    }

    /// Inherited PATH with the bundled runtimes dirs appended (system wins).
    ///
    /// Appends to the raw PATH string rather than split_paths/join_paths, which
    /// silently returns empty if any existing PATH entry is unjoinable (e.g.
    /// contains a quote) - that would wipe the whole PATH and break resolution.
    pub fn augmented_path() -> OsString {
        let mut path = std::env::var_os("PATH").unwrap_or_default();
        for dir in bundled_runtime_dirs() {
            if !path.is_empty() {
                path.push(";");
            }
            path.push(dir.as_os_str());
        }
        path
    }

    fn pathext() -> Vec<String> {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect()
    }

    /// Find `program` on `path`, applying PATHEXT for extensionless names.
    pub fn resolve_program(program: &str, path: &OsStr) -> Option<PathBuf> {
        let p = Path::new(program);
        if p.is_absolute() && p.is_file() {
            return Some(p.to_path_buf());
        }
        let has_ext = p.extension().is_some();
        let exts = pathext();
        for dir in std::env::split_paths(path) {
            if has_ext {
                // Caller gave an explicit extension - use it as-is.
                let direct = dir.join(program);
                if direct.is_file() {
                    return Some(direct);
                }
            } else {
                // No extension: only PATHEXT variants are runnable on Windows.
                // Do NOT match a bare extensionless file - Node ships an `npx`
                // Unix shell script next to `npx.cmd`, and CreateProcess can't
                // run the script. Prefer .cmd/.exe/etc. via PATHEXT.
                for ext in &exts {
                    let cand = dir.join(format!("{program}{ext}"));
                    if cand.is_file() {
                        return Some(cand);
                    }
                }
            }
        }
        None
    }

    pub fn is_batch(p: &Path) -> bool {
        matches!(
            p.extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_ascii_lowercase())
                .as_deref(),
            Some("cmd") | Some("bat")
        )
    }

    /// Quote a token for the cmd.exe command line (wrap if it has spaces or cmd
    /// metacharacters; cmd escapes an embedded `"` by doubling it).
    fn cmd_quote(s: &str) -> String {
        if !s.is_empty() && !s.chars().any(|c| " \t\"&|<>^()".contains(c)) {
            s.to_string()
        } else {
            format!("\"{}\"", s.replace('"', "\"\""))
        }
    }

    /// Build the inner command line for `cmd /s /c "<...>"`: the quoted program
    /// path followed by quoted args.
    pub fn build_cmd_line(prog: &Path, args: &[String]) -> String {
        let mut parts = vec![cmd_quote(&prog.display().to_string())];
        parts.extend(args.iter().map(|a| cmd_quote(a)));
        parts.join(" ")
    }
}

/// One running child stdio MCP server.
pub struct ChildServer {
    /// Logical id matching `desired.server_id`. Useful for logging/diagnostics
    /// even though the supervisor keys children by id externally.
    #[allow(dead_code)]
    pub server_id: String,
    /// The pre-enrichment ``DesiredServer`` as the backend sent it - args
    /// still contain ``{KEY}`` placeholders and env still holds raw values
    /// (no env_store overlay applied yet). Stored so respawns triggered by
    /// ``ServerSpecUpdate`` / ``ServerEnvUpdate`` can re-run ``enrich``
    /// against the freshly-updated env_store; re-enriching an already-
    /// substituted spec would be a no-op and the old values would stay
    /// baked in. ``apply_snapshot``/``apply_delta`` also compare this
    /// against the incoming raw to decide whether a kill+respawn is needed.
    pub desired_raw: DesiredServer,
    pub child: Child,
    pub outbound_tx: mpsc::Sender<serde_json::Value>,
    pub stdin_pump: JoinHandle<()>,
    pub stdout_pump: JoinHandle<()>,
}

impl ChildServer {
    /// Spawn the subprocess and start the two pumps.
    ///
    /// Takes both the ``raw`` (backend-authoritative, template placeholders
    /// intact) and ``enriched`` (env_store overlaid + ``templated_args``
    /// substituted) views. The subprocess is launched from ``enriched``;
    /// ``raw`` is stowed on the handle so later ``ServerSpecUpdate`` /
    /// ``ServerEnvUpdate`` can re-enrich from a fresh env_store read.
    ///
    /// `tunnel_outgoing` is the broker handle the inbound pump uses to send
    /// frames upstream to the backend. It survives WS reconnects - sends
    /// during a disconnect drop silently.
    pub fn spawn(
        raw: &DesiredServer,
        enriched: &DesiredServer,
        tunnel_outgoing: OutgoingHandle,
    ) -> Result<Self> {
        info!(
            server_id = %enriched.server_id,
            command = %enriched.command,
            "spawning stdio MCP subprocess",
        );

        let mut cmd = build_child_command(&enriched.command, &enriched.args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // Start with current env, then layer the configured env values.
        for (k, v) in &enriched.env {
            cmd.env(k, v);
        }
        if let Some(wd) = &enriched.working_dir {
            cmd.current_dir(wd);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn `{}`", enriched.command))?;

        let stdin = child.stdin.take().context("child stdin not captured")?;
        let stdout = child.stdout.take().context("child stdout not captured")?;
        let stderr = child.stderr.take().context("child stderr not captured")?;

        let (outbound_tx, outbound_rx) = mpsc::channel::<serde_json::Value>(64);
        let stdin_pump = tokio::spawn(stdin_pump(enriched.server_id.clone(), stdin, outbound_rx));
        let stdout_pump = tokio::spawn(stdout_pump(
            enriched.server_id.clone(),
            stdout,
            tunnel_outgoing,
        ));

        // Drain stderr into our log so child diagnostics aren't lost.
        let server_id = enriched.server_id.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                debug!(server_id = %server_id, "[child stderr] {}", line);
            }
        });

        Ok(Self {
            server_id: enriched.server_id.clone(),
            desired_raw: raw.clone(),
            child,
            outbound_tx,
            stdin_pump,
            stdout_pump,
        })
    }

    /// Kill the child and abort the pumps.
    pub async fn shutdown(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        self.stdin_pump.abort();
        self.stdout_pump.abort();
    }
}

async fn stdin_pump(
    server_id: String,
    mut stdin: tokio::process::ChildStdin,
    mut rx: mpsc::Receiver<serde_json::Value>,
) {
    while let Some(body) = rx.recv().await {
        let mut line = match serde_json::to_vec(&body) {
            Ok(v) => v,
            Err(e) => {
                warn!(server_id = %server_id, error = %e, "skipping unserialisable JSON-RPC frame");
                continue;
            }
        };
        line.push(b'\n');
        if let Err(e) = stdin.write_all(&line).await {
            warn!(server_id = %server_id, error = %e, "stdin write failed; ending pump");
            return;
        }
        if let Err(e) = stdin.flush().await {
            warn!(server_id = %server_id, error = %e, "stdin flush failed; ending pump");
            return;
        }
    }
}

async fn stdout_pump(
    server_id: String,
    stdout: tokio::process::ChildStdout,
    tunnel_outgoing: OutgoingHandle,
) {
    let mut reader = BufReader::new(stdout).lines();
    loop {
        match reader.next_line().await {
            Ok(Some(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(value) => {
                        let frame = TunnelFrame::McpFrame(McpFrame {
                            server_id: server_id.clone(),
                            frame: value,
                        });
                        // ``send`` is a no-op when disconnected; we keep
                        // reading so reconnect doesn't drop us.
                        tunnel_outgoing.send(frame).await;
                    }
                    Err(e) => {
                        warn!(
                            server_id = %server_id,
                            error = %e,
                            "child emitted non-JSON line; skipping",
                        );
                    }
                }
            }
            Ok(None) => break, // EOF - child exited.
            Err(e) => {
                warn!(server_id = %server_id, error = %e, "stdout read failed; ending pump");
                break;
            }
        }
    }

    // Load-bearing per v0 spike + ARCHITECTURE.md: tell the backend that
    // this server's subprocess is gone so any in-flight tool calls fail
    // cleanly instead of hanging.
    tunnel_outgoing
        .send(TunnelFrame::TunnelError(TunnelError {
            server_id: Some(server_id.clone()),
            related_jsonrpc_id: None,
            code: "server_offline".into(),
            message: "stdio subprocess exited".into(),
        }))
        .await;
    info!(server_id = %server_id, "child stdout pump ended");
}
