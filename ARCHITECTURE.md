# stdiod - Architecture

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
  state of its own - it connects, fetches the desired set of servers, and
  reconciles its running children against it.

## Components

`edison-stdiod` is a single binary that is both the long-lived service and the
control CLI. Its responsibilities - described here by role, independent of how
the source happens to be arranged on disk - are:

```
   control commands                  ┌──────────────────────────────┐
   (login · install ·                │  Supervisor                   │
    status · logs ·   ──── config ──▶│  connect → reconcile →        │
    server …)                        │  supervise the run loop       │
                                     └────────┬───────────┬─────────┘
                                              │           │
                                  ┌───────────▼──┐   ┌────▼───────────┐
                                  │ Tunnel        │   │ Child          │
                                  │ transport     │   │ supervision    │
                                  │ (WebSocket,   │   │ (spawn + stdio │
                                  │ opaque frames)│   │ pumps)         │
                                  └───────┬───────┘   └────────┬───────┘
                            outbound WS   │                    │ stdio
                                          ▼                    ▼
                                   Edison backend       local MCP servers
```

- **Control surface** - the CLI subcommands a user runs to authenticate,
  register the OS service, manage servers, and inspect state. They persist
  configuration; they do not carry MCP traffic.
- **Supervisor** - the long-lived run loop: connect, fetch desired state,
  reconcile running children against it, and supervise.
- **Tunnel transport** - the single outbound WebSocket and its framing. It is
  MCP-agnostic: MCP frames are forwarded as opaque bytes (see
  [MCP-agnostic by design](#mcp-agnostic-by-design)).
- **Child supervision** - spawning each desired server as a subprocess and
  pumping its stdio to and from the tunnel.

Cross-cutting concerns sit beneath all of the above: the thin HTTP client for
the backend's REST surface, on-disk config and state persistence, and the
platform-specific service integration (macOS / Linux / Windows).

The wire-protocol Rust types are **generated from a JSON Schema** (via
`schemars`/`typify`). The schema is the single source of truth so the daemon and
its peer can be kept in lock-step; see
[`schema/edison-tunnel-protocol.json`](./schema/edison-tunnel-protocol.json).

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
  natural - the backend can talk to the daemon at any time.
- It reuses the same outbound TLS:443 posture that already traverses corporate
  firewalls, with no third-party tunnelling dependency.

### Wire protocol

Defined as JSON Schema at `schema/edison-tunnel-protocol.json`. Frames are JSON with a
`type` discriminator and fall into two categories.

**Control frames** (lifecycle / desired state):

- `client_hello` (daemon → backend): `protocol_version`, `device_id`,
  `hostname`, `label`, `os`, `client_version`, `currently_running: [server_id]`.
  Sent immediately after the socket is established.
- `server_hello` (backend → daemon): `protocol_version` plus a **full
  desired-state snapshot** -
  `servers: [{server_id, name, command, args, env, working_dir, enabled}]`.
  If the daemon's `protocol_version` is below the minimum the backend supports,
  the upgrade is refused with a `needs_upgrade` close code; the daemon records
  `needs_upgrade=true` in `state.json` and stops retrying until the binary is
  updated.
- `desired_state_update` (backend → daemon): steady-state delta -
  `added` / `updated` / `removed` server lists.
- `device_status` (daemon → backend): periodic snapshot of which children are
  running and their last health timestamp.
- `announce_server` (daemon → backend): the user added a server via the local
  CLI; the backend records it for review.
- `creds_invalidated` (backend → daemon): the user's credentials were rotated.
  The daemon closes the connection, sets `needs_reauth=true` in `state.json`,
  fires a single OS notification, and waits for credentials to change before
  retrying.
- `fetch_logs_request` / `fetch_logs_response`: an operator-initiated, bounded
  (default 200 lines) pull of a child's recent `stdout`/`stderr`. Never streamed
  continuously, to keep bandwidth predictable.
- `ping` / `pong` (both directions): heartbeat - see
  [Disconnect handling](#disconnect-handling).

The `request_id` on `fetch_logs_*` is a control-layer correlation id, distinct
from the JSON-RPC `id` carried inside MCP frames.

**MCP frames** (symmetric, per-server):

- `mcp_frame` (both directions): a JSON-RPC frame addressed to or originating
  from a specific child. Fields: `server_id` and `frame` (the JSON-RPC body
  verbatim - request, response, or notification).
- `tunnel_error` (both directions): a structured, non-JSON-RPC error
  (subprocess crashed, unknown server, transport fault). Carries the inner
  JSON-RPC `id` it relates to when applicable, so the receiver can fail the
  matching outstanding call.

A single symmetric frame type captures every MCP interaction because JSON-RPC's
own envelope already distinguishes requests (`id` + `method`), responses (`id` +
`result`/`error`), and notifications (`method`, no `id`). JSON-RPC `id`s are
scoped to the originator, so the inner `id` is the correlation key - no outer
`request_id` is needed for MCP traffic.

### MCP-agnostic by design

The transport is **MCP-agnostic**: the daemon's `tunnel` module treats every
`frame` field as opaque bytes and never inspects its contents. This is a
load-bearing invariant - any temptation to sniff a method name or peek at
`params` inside the daemon is a smell; that logic belongs above the transport,
on the backend.

Concrete consequences:

- **Server-initiated requests** (e.g. `sampling/createMessage`,
  `elicitation/create`) flow naturally in either direction with no
  special-casing.
- **Bidirectional notifications** (e.g. `notifications/cancelled`,
  `notifications/progress`) are just notification-shaped `mcp_frame`s.
- **MCP version bumps and new methods** require no changes anywhere in the
  daemon - `initialize` negotiation happens between the backend and the stdio
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
opaque" alone - it is a deliberate active signal the daemon must produce.

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
~/Library/Logs/edison-stdiod/      (macOS - platform-equivalent paths elsewhere)
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

- The daemon sends a WS Ping every 15s and closes + reconnects if no Pong
  arrives within 10s.
- TCP keepalive is enabled to detect zombie sockets faster (e.g. a laptop lid
  closed mid-connection).
- A wall-clock gap detector notices sleep/resume jumps and restarts the
  WebSocket immediately rather than waiting out the heartbeat timeout.

### Reconnect policy

- Exponential backoff with jitter: 1s, 2s, 4s, 8s … capped at 60s, ±25% jitter
  to avoid a thundering herd against the backend after a deploy.
- **Retry forever** on transient errors (network down, DNS failure, connection
  refused, 5xx upgrade response).
- **Stop and notify on auth failure** (401/403 on upgrade): set
  `needs_reauth=true` in `state.json`, fire one OS notification, then wait for
  credentials to change before retrying.
- **Other 4xx** (device disabled, version too old): back off to a steady 60s and
  log clearly.

### Reconciliation on (re)connect

Every (re)connect runs the same protocol:

1. Daemon sends `client_hello { device_id, currently_running: [...] }`.
2. Backend replies `server_hello { servers: [...] }` - a full desired-state
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
retries - the calling agent decides whether to retry.

## CLI

The same binary is the daemon and the control CLI:

- `edison-stdiod login --backend <url> --api-key <key>` - store credentials.
- `edison-stdiod install` / `uninstall` - manage the OS service unit.
- `edison-stdiod run` - run the daemon (normally invoked by the service unit).
- `edison-stdiod server …` - add / list / remove locally-defined servers.
- `edison-stdiod status` - show connection and per-child state.
- `edison-stdiod logs` - tail daemon / child logs.
