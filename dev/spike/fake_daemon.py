"""A minimal "daemon" for the v0 spike.

Bridges the opaque tunnel to a real stdio MCP subprocess, in-process.
Production-shape: read TunnelFrames inbound, dispatch to the right
subprocess; read subprocess stdout, wrap into TunnelFrames outbound.

For the spike there's only one subprocess (one server_id), no real
WebSocket. This file owns the subprocess lifecycle.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import sys
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager

import anyio
from anyio.streams.memory import MemoryObjectReceiveStream, MemoryObjectSendStream
from mcp import StdioServerParameters
from mcp.client.stdio import stdio_client
from mcp.shared.message import SessionMessage
from mcp.types import JSONRPCMessage
from tunnel_protocol import McpFrame, TunnelError, TunnelFrame

log = logging.getLogger(__name__)


@asynccontextmanager
async def run_fake_daemon(  # noqa: C901 - spike: deliberately one-shot
    server_id: str,
    command: str,
    args: list[str],
    backend_to_daemon: asyncio.Queue[TunnelFrame],
    daemon_to_backend: asyncio.Queue[TunnelFrame],
) -> AsyncIterator[None]:
    """Spawn the subprocess and run two pumps for as long as the context lives."""

    params = StdioServerParameters(command=command, args=args)

    async def outbound_to_subprocess(
        write_stream: MemoryObjectSendStream[SessionMessage],
    ) -> None:
        """tunnel (backend → daemon) → subprocess stdin."""
        try:
            while True:
                frame = await backend_to_daemon.get()
                if not isinstance(frame, McpFrame):
                    log.warning("daemon: dropping non-McpFrame: %s", type(frame).__name__)
                    continue
                if frame.server_id != server_id:
                    log.warning("daemon: unknown server_id=%s", frame.server_id)
                    continue
                try:
                    message = JSONRPCMessage.model_validate(frame.frame)
                except Exception:
                    log.exception("daemon: bad inbound JSONRPC")
                    continue
                await write_stream.send(SessionMessage(message))
        except (anyio.ClosedResourceError, anyio.BrokenResourceError):
            pass

    async def subprocess_to_outbound(
        read_stream: MemoryObjectReceiveStream[SessionMessage | Exception],
    ) -> None:
        """subprocess stdout → tunnel (daemon → backend)."""
        try:
            async with read_stream:
                async for item in read_stream:
                    if isinstance(item, Exception):
                        await daemon_to_backend.put(
                            TunnelError(
                                server_id=server_id,
                                related_jsonrpc_id=None,
                                code="subprocess_parse_error",
                                message=str(item),
                            )
                        )
                        continue
                    body = item.message.model_dump(by_alias=True, exclude_none=True, mode="json")
                    await daemon_to_backend.put(McpFrame(server_id=server_id, frame=body))
        except (anyio.ClosedResourceError, anyio.BrokenResourceError):
            pass
        finally:
            # The subprocess has exited (or we're shutting down). Push a
            # tunnel_error so the backend's in-flight requests fail cleanly
            # instead of hanging waiting for a response.
            with contextlib.suppress(Exception):
                daemon_to_backend.put_nowait(
                    TunnelError(
                        server_id=server_id,
                        related_jsonrpc_id=None,
                        code="server_offline",
                        message="stdio subprocess exited",
                    )
                )

    async with (
        stdio_client(params, errlog=sys.stderr) as (read_stream, write_stream),
        anyio.create_task_group() as tg,
    ):
        tg.start_soon(outbound_to_subprocess, write_stream)
        tg.start_soon(subprocess_to_outbound, read_stream)
        try:
            yield None
        finally:
            # Closing the stdio streams (handled by stdio_client's
            # __aexit__) will cancel the pumps via ClosedResourceError.
            # subprocess_to_outbound's finally handler pushes the
            # tunnel_error on shutdown.
            tg.cancel_scope.cancel()
