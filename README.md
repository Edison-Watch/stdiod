# stdiod

[![CI](https://github.com/Edison-Watch/stdiod/actions/workflows/ci.yml/badge.svg)](https://github.com/Edison-Watch/stdiod/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)

**stdiod** is a small daemon that bridges local [stdio MCP servers](https://modelcontextprotocol.io/) to the Edison Watch backend over a single outbound WebSocket tunnel.

It runs on a user's machine, dials out to the backend (no inbound ports), and lets the backend drive locally-spawned MCP server subprocesses — forwarding MCP frames in both directions. An AI client talking to the backend's gateway reaches these local servers as if they were hosted remotely, while the processes (and their filesystem/credentials) stay on the user's device.

```
AI client ──▶ Edison backend gateway ──▶  WebSocket tunnel  ──▶ stdiod ──▶ local MCP server subprocess
                                          (outbound, from the device)
```

> **Status: experimental (v0.0.1).** This is early software under active development. It has **not** had an independent security audit. The wire protocol, CLI surface, and on-disk formats may change without notice before a 1.0 release. Today the daemon runs as a supervised service on **macOS only**; Linux and Windows support is on the roadmap (the CLI will tell you when a step is unsupported on your platform).

## How it works

- **Outbound-only.** The daemon opens one WebSocket to `<backend>/api/v1/stdio-tunnel/ws` and authenticates with a Bearer API key. There are no inbound listening ports.
- **Reverse RPC.** A single symmetric `mcp_frame` envelope carries every MCP interaction (requests, responses, server-initiated sampling, notifications, errors) in both directions over the one connection.
- **Child supervision.** The backend pushes a desired set of servers; the daemon spawns/stops the matching subprocesses and pumps their stdio.
- **Survival.** It reconnects with backoff across network blips and machine sleep/resume, and reconciles desired state on every (re)connect.

See [`ARCHITECTURE.md`](./ARCHITECTURE.md) for the full design and [`schema/tunnel-protocol.json`](./schema/tunnel-protocol.json) for the wire protocol (the single source of truth for the frame types).

## Install

Requires a [Rust toolchain](https://rustup.rs/) (the pinned channel is in [`rust-toolchain.toml`](./rust-toolchain.toml)).

```sh
# Build and install the `edison-stdiod` binary from a checkout:
cargo install --path crates/edison-stdiod

# …or build in place:
cargo build --release   # binary at target/release/edison-stdiod
```

The repository is a Cargo workspace; the `edison-stdiod` binary is the daemon **and** the control CLI.

## Quickstart

```sh
# 1. Store credentials + backend URL in ~/.config/edison-stdiod/config.toml (mode 0600).
edison-stdiod login \
  --backend https://dashboard.edison.watch \
  --api-key <YOUR_API_KEY>

# 2. Register the OS supervisor unit (macOS LaunchAgent) so the daemon
#    starts at login and is restarted on crash. Requires `login` first.
edison-stdiod install

# 3. Check connection + per-child health at any time.
edison-stdiod status

# 4. Tail the logs (-f to follow).
edison-stdiod logs -f
```

To run the daemon in the foreground without installing a service unit (useful for development):

```sh
edison-stdiod run --backend http://localhost:3001 --api-key <KEY>
# or rely on the persisted config from `login`:
edison-stdiod run
```

### Registering a local MCP server

```sh
# Expose a local stdio MCP server through the tunnel. Tool calls appear in
# the gateway namespaced as `<name>_<tool>`.
edison-stdiod server add filesystem \
  --command npx \
  --arg -y --arg @modelcontextprotocol/server-filesystem --arg "$HOME"

edison-stdiod server list
edison-stdiod server remove filesystem
```

## Configuration

Settings resolve in two layers, highest precedence first:

1. **CLI flags / environment variables** — handy for development overrides.
2. **`~/.config/edison-stdiod/config.toml`** — written by `edison-stdiod login`; this is what the OS supervisor unit reads (service units don't carry secrets in their environment).

| Field (`config.toml`) | Env var | Description |
| --- | --- | --- |
| `backend_url` | `EDISON_BACKEND_URL` | Backend base URL (`http://localhost:3001` for dev, `https://dashboard.edison.watch` for prod). |
| `api_key` | `EDISON_API_KEY` | Bearer API key issued by the backend. Stored in plaintext at mode `0600`. |
| `edison_secret_key` | `EDISON_SECRET_KEY` | Optional `X-Edison-Secret-Key` for per-user secret decryption. |
| `device_id` | `EDISON_DEVICE_ID` | Stable device identifier; defaults to the machine hostname. |
| `device_label` | `EDISON_DEVICE_LABEL` | Human-readable label shown in the dashboard. |

Credentials live in a `0600` file under `~/.config/edison-stdiod/`. Rotate the API key by re-running `edison-stdiod login --api-key …`. To remove everything, run `edison-stdiod uninstall --purge`.

## Development

```sh
cargo build --workspace      # build
cargo test --workspace       # run tests
cargo fmt --all --check      # formatting
cargo clippy --workspace --all-targets -- -D warnings   # lints
```

[`dev/spike/`](./dev/spike/) holds a throwaway v0 Python prototype that validated the wire protocol before the Rust daemon was written; it is kept as a historical record and is not part of the build.

See [`CONTRIBUTING.md`](./CONTRIBUTING.md) for the contribution workflow and [`SECURITY.md`](./SECURITY.md) for how to report vulnerabilities.

## License

Licensed under the [MIT License](./LICENSE).
