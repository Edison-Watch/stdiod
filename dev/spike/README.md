# v0 FastMCP transport spike

Purpose: validate that FastMCP 3.2.4's `ClientTransport` ABC can host the
symmetric `mcp_frame` envelope defined in
[stdiod/ARCHITECTURE.md](../../ARCHITECTURE.md#wire-protocol) without forcing
wire-protocol changes. Throwaway v0 prototype that informed the v1 design;
kept here as a historical record, not part of the daemon build.

## Layout

```
stub_mcp_server.py   FastMCP stdio server: add, slow_count, ask_sample, crash
edison_tunnel_protocol.py   Pydantic models for the wire envelope (ClientHello,
                     ServerHello, McpFrame, TunnelError)
tunnel_transport.py  Custom ClientTransport. Wraps SessionMessages as
                     McpFrames; unwraps inbound frames into ClientSession's
                     read stream. Surfaces TunnelError as a stream-close.
fake_daemon.py       Bridges the "tunnel" (asyncio queues) to a real stdio
                     MCP subprocess via mcp.client.stdio.stdio_client.
spike_test.py        Driver. Spawns the stub, wires the transport, runs the
                     seven validation checks.
```

The "tunnel" between transport and daemon is a pair of `asyncio.Queue`s.
What we're validating is the **wire format**, not WebSocket plumbing.

## Run

```
cd stdiod/dev/spike
uv run python spike_test.py
```

Expected output ends in `SPIKE PASS`.

## What was validated

| # | Check | Frame types exercised |
|---|---|---|
| 1 | `initialize` handshake / capability negotiation | request, response |
| 2 | `tools/list` | request, response |
| 3 | `tools/call add(a,b)` | request, response with args |
| 4 | `tools/call slow_count(n)` final result | request, response |
| 5 | Server-initiated notifications during `slow_count` | `notifications/progress` |
| 6 | Server-initiated request from `ask_sample` | `sampling/createMessage` (server→client RPC) |
| 7 | Subprocess crash mid-call | `tunnel_error` propagation |

## Findings

**The symmetric `McpFrame` envelope is sufficient.** No wire-protocol
changes required before v1. All six frame categories (client request,
server response, server-initiated notification, server-initiated request,
client response to server-initiated request, transport-level error)
round-trip cleanly through the same `{ type: "mcp_frame", server_id, frame }`
shape.

Concrete observations from getting the spike to pass:

1. **FastMCP's `ClientTransport.connect_session` is the right seam.** It
   yields an `mcp.ClientSession` backed by two anyio memory object streams
   carrying `SessionMessage`. Bridging those to the tunnel is ~40 lines of
   code per direction - no FastMCP internals needed.
2. **JSON-RPC's own envelope discriminates request/response/notification.**
   Presence of `id` + `method` = request; `id` + `result`/`error` =
   response; `method` only = notification. We do not need an outer
   correlation id in `mcp_frame`; the inner JSON-RPC `id` is enough.
3. **Server-initiated requests work transparently.** `sampling/createMessage`
   from the stub server flows daemon→backend in an `McpFrame`, the
   FastMCP `Client`'s `sampling_handler` produces the response, and that
   response flows backend→daemon in another `McpFrame`. No tunnel-side
   logic needed.
4. **Error propagation needs explicit signalling on subprocess death.**
   When the child stdio subprocess exits mid-call, the daemon's read pump
   exits silently - that alone doesn't fail the FastMCP client's in-flight
   request. The daemon must push a `TunnelError` on the outbound queue in
   its read-pump's `finally` block; the transport then closes the
   `ClientSession`'s read stream, which raises `McpError` on the pending
   request. (See [fake_daemon.py](fake_daemon.py)'s
   `subprocess_to_outbound` finally clause.) This pattern is required in
   the real implementation too.
5. **No deadlocks observed.** The two-pump pattern (one per direction,
   each owning the lifetime of its half of the streams) shuts down
   cleanly via `anyio.create_task_group` cancellation.

## What this spike does NOT cover

These are honest deferrals; none invalidates the conclusion.

- **`notifications/cancelled` (backend → daemon notification)**. Not
  exercised directly. Notifications are simpler than the
  server-initiated requests we *did* exercise (sampling), so the
  symmetric envelope handles them by construction.
- **Protocol-version mismatch handling.** That lives at the control-frame
  layer (`client_hello` / `server_hello`), not at the `mcp_frame` layer.
  The in-process tunnel skips control frames entirely; v1 must add them
  but the design in ARCHITECTURE.md already specifies the behaviour
  (close WS with `needs_upgrade` code, daemon sets `state.json` flag).
- **Real WebSocket transport.** Spike uses `asyncio.Queue`. The
  wire-format proof is independent of carrier - once you can serialise
  `TunnelFrame` to JSON over WS, the rest is identical.
- **Reconnect / reconciliation.** Out of scope for the wire-format
  spike. Covered separately in ARCHITECTURE.md's reconnect section.

## Decision

**Proceed with v1 using the wire schema as documented.** No changes
needed to
[stdiod/ARCHITECTURE.md](../../ARCHITECTURE.md#wire-protocol) as a result of
this spike.
