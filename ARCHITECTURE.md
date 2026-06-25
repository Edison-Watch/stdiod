# stdiod — Architecture

`edison-stdiod` is a small, long-lived daemon that runs on a user's machine. It
maintains a single authenticated outbound connection to a backend, supervises a
set of local **stdio** MCP subprocesses on that machine, and bridges MCP traffic
between those subprocesses and the backend over that one connection.

This document describes the daemon's own design. The backend is treated as an
opaque peer reachable at a configured URL; only the public daemon↔backend wire
contract is described here.

## Scope

- **One daemon = one device.** A user may run the daemon on many machines; each
  running daemon represents a single device.
- **Subprocesses run locally.** Every MCP stdio server the daemon manages is
  spawned as a child process on the user's machine. Nothing is spawned remotely.
- **The backend is the source of truth.** The daemon stores almost no durable
  state of its own — it connects, fetches the desired set of servers, and
  reconciles its running children against it.

## Workspace layout

A Cargo workspace with two crates:

```
crates/
  edison-stdiod/        the daemon + CLI binary
    src/
      main.rs           entry point / arg dispatch
      cli/              subcommands: login, install, server, status, logs
      daemon.rs         the run loop: connect, reconcile, supervise
      tunnel.rs         WebSocket transport + framing
      http.rs           thin HTTP client for the backend REST surface
      proc.rs           child-process spawning and stdout/stderr pumps
      state.rs          state.json (atomic writes, consumed by the tray UI)
      config.rs         config.toml (backend URL, device id, credentials)
      env_store.rs      per-server environment variable storage
      paths.rs          platform config/log/data path resolution
      platform/         macOS / Linux / Windows service integration
  tunnel-protocol/      generated Rust types for the wire protocol
schema/
  tunnel-protocol.json  JSON Schema — single source of truth for the protocol
dev/
  spike/                throwaway v0 prototype that informed the design
```

The `tunnel-protocol` crate's Rust types are generated from
`schema/tunnel-protocol.json` (via `schemars`/`typify`). The JSON Schema is the
single source of truth so the daemon and its peer can be kept in lock-step.

## Tunnel mechanism: reverse RPC over WebSocket

The daemon opens **one** outbound WebSocket to the backend:

```
GET <backend>/api/v1/stdio-tunnel/ws
Authorization: Bearer <api_key>
X-Edison-Secret-Key: <secret>
X-Edison-Device-Id: <device_id>
```

A single long-lived WebSocket is used rather than a local HTTP wrapper plus a
reverse tunnel because:

- One authentication check, one stateful connection, lowest latency.
- Server-initiated frames (desired-state pushes, credential invalidations) are
  natural — the backend can talk to the daemon at any time.
- It reuses the same outbound TLS:443 posture that already traverses corporate
  firewalls, with no third-party tunnelling dependency.

### Wire protocol

Defined as JSON Schema at `schema/tunnel-protocol.json`. Frames are JSON with a
`type` discriminator and fall into two categories.

**Control frames** (lifecycle / desired state):

- `client_hello` (daemon → backend): `protocol_version`, `device_id`,
  `hostname`, `label`, `os`, `client_version`, `currently_running: [server_id]`.
  Sent immediately after the socket is established.
- `server_hello` (backend → daemon): `protocol_version` plus a **full
  desired-state snapshot** —
  `servers: [{server_id, name, command, args, env, working_dir, enabled}]`.
  On a `protocol_version` mismatch the daemon currently logs a warning and
  continues (v1 MVP). The designed behaviour — refuse with a `needs_upgrade`
  close code, record `needs_upgrade=true` in `state.json`, and stop retrying
  until the binary is updated — is planned, not yet implemented.
- `desired_state_update` (backend → daemon): steady-state delta —
  `added` / `updated` / `removed` server lists.
- `server_env_update` (backend → daemon): env values for one server, written
  to the daemon's local `env_store` and applied on next spawn. Sent at server
  create/update; never part of the steady-state push, so secrets aren't
  re-sent.
- `server_spec_update` (backend → daemon): per-server template *values* (env +
  per-placeholder `templated_args`) collected on the dashboard; merged into the
  `env_store`. Command/args structure/working_dir stay authoritative on the
  backend and arrive via `desired_state_update`.
- `server_spawn_result` (daemon → backend): outcome of a spawn attempt, so the
  backend can gate its create/update HTTP response on a real spawn instead of
  fire-and-forget.
- `announce_server` (daemon → backend): defined in the protocol for a daemon
  that registers a server locally. **Not emitted in v1** — the `server add`
  CLI registers over the HTTP API instead; the frame is reserved.
- `ping` / `pong` (both directions): heartbeat — see
  [Disconnect handling](#disconnect-handling).

**Planned control frames (designed, not implemented in v1):**

- `device_status` (daemon → backend): periodic snapshot of which children are
  running and their last health timestamp.
- `creds_invalidated` (backend → daemon): on credential rotation, the daemon
  would close, set `needs_reauth=true` in `state.json`, fire one OS
  notification, and wait for credentials to change before retrying.
- `fetch_logs_request` / `fetch_logs_response`: an operator-initiated, bounded
  (default 200 lines) pull of a child's recent `stdout`/`stderr`.

The `request_id` on `fetch_logs_*` is a control-layer correlation id, distinct
from the JSON-RPC `id` carried inside MCP frames.

**MCP frames** (symmetric, per-server):

- `mcp_frame` (both directions): a JSON-RPC frame addressed to or originating
  from a specific child. Fields: `server_id` and `frame` (the JSON-RPC body
  verbatim — request, response, or notification).
- `tunnel_error` (both directions): a structured, non-JSON-RPC error
  (subprocess crashed, unknown server, transport fault). Carries the inner
  JSON-RPC `id` it relates to when applicable, so the receiver can fail the
  matching outstanding call.

A single symmetric frame type captures every MCP interaction because JSON-RPC's
own envelope already distinguishes requests (`id` + `method`), responses (`id` +
`result`/`error`), and notifications (`method`, no `id`). JSON-RPC `id`s are
scoped to the originator, so the inner `id` is the correlation key — no outer
`request_id` is needed for MCP traffic.

### MCP-agnostic by design

The transport is **MCP-agnostic**: the daemon's `tunnel` module treats every
`frame` field as opaque bytes and never inspects its contents. This is a
load-bearing invariant — any temptation to sniff a method name or peek at
`params` inside the daemon is a smell; that logic belongs above the transport,
on the backend.

Concrete consequences:

- **Server-initiated requests** (e.g. `sampling/createMessage`,
  `elicitation/create`) flow naturally in either direction with no
  special-casing.
- **Bidirectional notifications** (e.g. `notifications/cancelled`,
  `notifications/progress`) are just notification-shaped `mcp_frame`s.
- **MCP version bumps and new methods** require no changes anywhere in the
  daemon — `initialize` negotiation happens between the backend and the stdio
  server, both outside the transport.

## Child-process supervision

The daemon spawns each desired server as a child process and runs two pumps per
child: subprocess `stdout` → tunnel frames, and tunnel frames → subprocess
`stdin`. `stderr` is captured to a per-child log file.

**Active failure signalling.** When a child's `stdout → tunnel` pump exits (the
subprocess crashed or hard-exited), the pump **must**, on its shutdown path,
emit a `tunnel_error` frame for that `server_id` before exiting:

```
tunnel_error {
  server_id: "<the dead server>",
  related_jsonrpc_id: null,
  code: "server_offline",
  message: "stdio subprocess exited",
}
```

Without this, an in-flight tool call against the dead child would hang forever
waiting for a response that never arrives. The WebSocket itself stays open and
other children on the same device are unaffected; the supervisor then decides
whether and when to respawn the dead child per the latest desired state. This
was the one behaviour the early spike could not derive from "treat MCP frames as
opaque" alone — it is a deliberate active signal the daemon must produce.

## Persistence and survival

### OS-level supervision

`edison-stdiod install` writes a platform-appropriate service unit;
`uninstall` removes it.

- **macOS**: LaunchAgent plist at
  `~/Library/LaunchAgents/watch.edison.stdiod.plist` with `KeepAlive=true`,
  `RunAtLoad=true`. No admin privileges needed.
- **Linux**: user systemd unit at
  `~/.config/systemd/user/edison-stdiod.service` with `Restart=always`,
  `RestartSec=5s`, `WantedBy=default.target`, started via
  `systemctl --user enable --now`. `loginctl enable-linger` is opt-in.
- **Windows**: a Scheduled Task with an "at log on" trigger and a
  restart-on-failure policy. No admin install required.

### Local files

The daemon keeps almost nothing durable; the backend is the source of truth.

```
~/.config/edison-stdiod/
  config.toml      backend URL, device_id, api_key, secret
  state.json       atomic writes; consumed by the desktop tray UI
~/Library/Logs/edison-stdiod/      (macOS — platform-equivalent paths elsewhere)
  daemon.log       rotated daily
  child-<name>.log per-child stdout/stderr capture
```

`state.json` example:

```json
{
  "connection_state": "connected",
  "backend_url": "https://<your-backend>",
  "device_label": "my-laptop",
  "last_connected_at": "2026-05-21T11:32:08Z",
  "last_error": null,
  "servers": [
    { "name": "filesystem", "state": "running", "pid": 81342 },
    { "name": "fetch",      "state": "starting" }
  ]
}
```

## Disconnect handling

### Heartbeats

- The daemon sends a WS Ping every 15s and tears the session down if no
  traffic of any kind arrives for 25s (any inbound frame counts as liveness,
  not just a Pong).
- A wall-clock gap detector notices sleep/resume jumps larger than 45s and
  restarts the WebSocket immediately rather than waiting out the heartbeat
  timeout.

### Reconnect policy

- Exponential backoff with jitter: 1s, 2s, 4s, 8s … capped at 30s, ±25% jitter
  to avoid a thundering herd against the backend after a deploy.
- **Retry forever** on any connect/session error (network down, DNS failure,
  connection refused, non-2xx upgrade response). v1 does not special-case the
  failure: it logs the error into `state.last_error` and retries with backoff.
- **Planned (not in v1):** distinguish auth failure (401/403) — set
  `needs_reauth=true`, fire one OS notification, and wait for credentials to
  change before retrying — and other 4xx (device disabled, version too old) by
  backing off to a steady 60s. Today all of these just retry with backoff.

### Reconciliation on (re)connect

Every (re)connect runs the same protocol:

1. Daemon sends `client_hello { device_id, currently_running: [...] }`.
2. Backend replies `server_hello { servers: [...] }` — a full desired-state
   snapshot for this device.
3. Daemon diffs:
   - Start any enabled server not currently running.
   - Kill any running server absent from the snapshot or marked disabled.
   - Restart any whose `command` / `args` / `env` / `working_dir` changed.
4. Steady-state changes arrive as `desired_state_update` deltas; the snapshot on
   the next reconnect is always authoritative.

### In-flight requests on disconnect

Every outbound `mcp_frame` carries a JSON-RPC `id` used as the correlation key.
On socket close, all outstanding calls are failed cleanly (the backend surfaces
a `device_offline`-style JSON-RPC error to the caller); there are no automatic
retries — the calling agent decides whether to retry.

## CLI

The same binary is the daemon and the control CLI:

- `edison-stdiod login --backend <url> --api-key <key>` — store credentials.
- `edison-stdiod install` / `uninstall` — manage the OS service unit.
- `edison-stdiod run` — run the daemon (normally invoked by the service unit).
- `edison-stdiod server …` — add / list / remove locally-defined servers.
- `edison-stdiod status` — show connection and per-child state.
- `edison-stdiod logs` — tail daemon / child logs.
