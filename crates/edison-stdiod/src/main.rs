//! `edison-stdiod` - Edison Watch stdiod daemon.
//!
//! Bridges local stdio MCP server subprocesses to the Edison Watch backend
//! over a WebSocket tunnel. v1 MVP scope: connect, receive desired state,
//! spawn subprocesses, forward MCP frames. Reconnect / heartbeat / install
//! are deferred to v1.1 per the implementation plan in
//! `stdiod/REQUIREMENTS.md`.

use clap::{Parser, Subcommand};

mod cli;
mod config;
mod daemon;
mod http;
mod paths;
mod platform;
mod proc;
mod state;
mod tunnel;

/// Edison Watch stdiod daemon and CLI.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon in the foreground. v1 MVP entry point.
    Run(daemon::RunArgs),
    /// Persist credentials + backend URL to `~/.config/edison-stdiod/config.toml`.
    Login(cli::login::LoginArgs),
    /// Register the OS supervisor unit (macOS LaunchAgent) so the daemon
    /// starts at login and is restarted on crash. Requires `login` first.
    Install(cli::install::InstallArgs),
    /// Stop and remove the OS supervisor unit. Pass `--purge` to also
    /// delete the persisted config and logs.
    Uninstall(cli::install::UninstallArgs),
    /// Print a one-shot summary of daemon health (supervisor unit
    /// status, connection state, and currently-running child servers).
    Status(cli::status::StatusArgs),
    /// Print the daemon log. Pass `--follow` to tail in real time.
    Logs(cli::logs::LogsArgs),
    /// Register, list, or remove stdio_tunnel servers for this device.
    #[command(subcommand)]
    Server(cli::server::ServerCommand),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,edison_stdiod=debug")),
        )
        .with_writer(std::io::stderr)
        .init();

    match cli.command {
        Command::Run(args) => daemon::run(args).await,
        Command::Login(args) => cli::login::run(args),
        Command::Install(args) => cli::install::install(args),
        Command::Uninstall(args) => cli::install::uninstall(args),
        Command::Status(args) => cli::status::run(args),
        Command::Logs(args) => cli::logs::run(args),
        Command::Server(args) => cli::server::run(cli::server::ServerArgs { command: args }).await,
    }
}
