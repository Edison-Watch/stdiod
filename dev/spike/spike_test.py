"""Driver for the v0 FastMCP transport spike.

Exercises every MCP frame type described in ARCHITECTURE.md against the
symmetric ``McpFrame`` envelope:

- initialize handshake / capability negotiation
- request / response (``tools/list``, ``tools/call``)
- server-initiated notifications (progress)
- error propagation on mid-call subprocess crash

A pass means the symmetric envelope is sufficient and no wire-protocol
change is needed. Any failure must be folded back into the schema before
v1 implementation.
"""

from __future__ import annotations

import asyncio
import logging
import sys
from pathlib import Path

from fake_daemon import run_fake_daemon
from fastmcp import Client
from mcp.types import (
    CreateMessageRequestParams,
    CreateMessageResult,
    TextContent,
)
from tunnel_protocol import TunnelFrame
from tunnel_transport import TunnelTransport

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s")
log = logging.getLogger("spike")


STUB_PATH = Path(__file__).parent / "stub_mcp_server.py"


def _ok(label: str) -> None:
    print(f"\033[32m✓ {label}\033[0m")


def _fail(label: str, detail: str) -> None:
    print(f"\033[31m✗ {label}: {detail}\033[0m")


async def run_spike() -> int:  # noqa: C901 - spike: linear sequence of checks
    """Returns the number of failed checks."""
    failures = 0

    backend_to_daemon: asyncio.Queue[TunnelFrame] = asyncio.Queue()
    daemon_to_backend: asyncio.Queue[TunnelFrame] = asyncio.Queue()

    transport = TunnelTransport(
        server_id="stub",
        outgoing=backend_to_daemon,
        incoming=daemon_to_backend,
    )

    progress_events: list[tuple[float, float | None, str | None]] = []

    async def progress_handler(progress: float, total: float | None, message: str | None) -> None:
        progress_events.append((progress, total, message))

    sampling_calls: list[str] = []

    async def sampling_handler(
        messages, params: CreateMessageRequestParams, ctx
    ) -> CreateMessageResult | str:
        # The first user message text - what the server asked us about.
        for m in messages:
            if isinstance(m.content, TextContent):
                sampling_calls.append(m.content.text)
                break
        return "echo:" + (sampling_calls[-1] if sampling_calls else "")

    async with run_fake_daemon(
        server_id="stub",
        command=sys.executable,
        args=[str(STUB_PATH)],
        backend_to_daemon=backend_to_daemon,
        daemon_to_backend=daemon_to_backend,
    ):
        client = Client(
            transport,
            progress_handler=progress_handler,
            sampling_handler=sampling_handler,
        )
        async with client:
            # 1. initialize / capability negotiation - implicit in `async with`.
            _ok("initialize handshake completed")

            # 2. tools/list - basic request/response.
            tools = await client.list_tools()
            tool_names = sorted(t.name for t in tools)
            expected = ["add", "ask_sample", "crash", "slow_count"]
            if tool_names == expected:
                _ok(f"tools/list returned {tool_names}")
            else:
                failures += 1
                _fail("tools/list", f"got {tool_names}, expected {expected}")

            # 3. tools/call - basic request/response with args.
            result = await client.call_tool("add", {"a": 7, "b": 5})
            _first = result.content[0] if result.content else None
            content = _first.text if isinstance(_first, TextContent) else None
            if content == "12":
                _ok("tools/call add(7,5) -> 12")
            else:
                failures += 1
                _fail("tools/call add", f"got {content!r}")

            # 4. Server-initiated notifications (progress) during a tool call.
            progress_events.clear()
            result = await client.call_tool("slow_count", {"n": 3})
            _first = result.content[0] if result.content else None
            content = _first.text if isinstance(_first, TextContent) else None
            if content == "counted to 3":
                _ok("tools/call slow_count(3) returned final result")
            else:
                failures += 1
                _fail("tools/call slow_count result", f"got {content!r}")
            if len(progress_events) == 3 and [p[0] for p in progress_events] == [1.0, 2.0, 3.0]:
                _ok(f"received {len(progress_events)} progress notifications with correct ordering")
            else:
                failures += 1
                _fail(
                    "progress notifications",
                    f"got {progress_events!r}",
                )

            # 5. Server-initiated request: tools/call invokes ctx.sample(),
            # which sends sampling/createMessage from the stub server back to
            # this client via the tunnel.
            sampling_calls.clear()
            result = await client.call_tool("ask_sample", {"prompt": "hello world"})
            _first = result.content[0] if result.content else None
            content = _first.text if isinstance(_first, TextContent) else None
            if sampling_calls == ["hello world"] and content == "sampled:echo:hello world":
                _ok("server-initiated sampling/createMessage round-tripped via tunnel")
            else:
                failures += 1
                _fail(
                    "sampling round-trip",
                    f"calls={sampling_calls!r}, content={content!r}",
                )

            # 6. Error propagation: tool call against a crashing subprocess.
            try:
                await client.call_tool("crash", {})
                failures += 1
                _fail("crash error propagation", "expected an exception, none raised")
            except Exception as exc:  # noqa: BLE001
                _ok(f"crash propagated cleanly as {type(exc).__name__}")

    print()
    if failures == 0:
        print("\033[32mSPIKE PASS\033[0m - symmetric McpFrame envelope is sufficient.")
    else:
        print(f"\033[31mSPIKE FAIL\033[0m - {failures} check(s) failed.")
    return failures


if __name__ == "__main__":
    rc = asyncio.run(run_spike())
    sys.exit(1 if rc else 0)
