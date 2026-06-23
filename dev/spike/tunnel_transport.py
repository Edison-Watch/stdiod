"""``TunnelTransport`` - the v0 spike target.

Implements FastMCP's ``ClientTransport`` so that a FastMCP ``Client`` (the
backend's side of the gateway proxy) talks to a remote stdio MCP server
*through* an opaque tunnel. For the spike the "tunnel" is a pair of
in-process queues - the wire format is what we're validating, not the
wire transport.

Bridge layout:

    FastMCP Client
        │
        ▼
    ClientSession(read_stream, write_stream)        ← MCP SDK
        ▲                       │
        │                       ▼
    [_inbound_pump]         [_outbound_pump]        ← this file
        ▲                       │
        │                       ▼
    incoming TunnelFrames   outgoing TunnelFrames   ← the "tunnel"
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
from collections.abc import AsyncIterator
from typing import Unpack

import anyio
from anyio.streams.memory import MemoryObjectReceiveStream, MemoryObjectSendStream
from fastmcp.client.transports.base import ClientTransport, SessionKwargs
from mcp import ClientSession
from mcp.shared.message import SessionMessage
from mcp.types import JSONRPCMessage
from tunnel_protocol import McpFrame, TunnelError, TunnelFrame

log = logging.getLogger(__name__)


class TunnelTransport(ClientTransport):
    """Connects a FastMCP client to a remote stdio MCP server via an opaque
    tunnel.

    ``outgoing`` carries TunnelFrames produced by this transport (destined
    for the remote daemon). ``incoming`` carries TunnelFrames arriving from
    the remote daemon. Both are plain ``asyncio.Queue``\\s for the spike;
    in production they'd be the read/write halves of a WebSocket.
    """

    def __init__(
        self,
        server_id: str,
        outgoing: asyncio.Queue[TunnelFrame],
        incoming: asyncio.Queue[TunnelFrame],
    ):
        self.server_id = server_id
        self.outgoing = outgoing
        self.incoming = incoming

    @contextlib.asynccontextmanager
    async def connect_session(
        self, **session_kwargs: Unpack[SessionKwargs]
    ) -> AsyncIterator[ClientSession]:
        # Streams the ClientSession reads from / writes to.
        read_writer: MemoryObjectSendStream[SessionMessage | Exception]
        read_reader: MemoryObjectReceiveStream[SessionMessage | Exception]
        write_writer: MemoryObjectSendStream[SessionMessage]
        write_reader: MemoryObjectReceiveStream[SessionMessage]
        read_writer, read_reader = anyio.create_memory_object_stream(0)
        write_writer, write_reader = anyio.create_memory_object_stream(0)

        async with anyio.create_task_group() as tg:
            tg.start_soon(self._outbound_pump, write_reader)
            tg.start_soon(self._inbound_pump, read_writer)
            async with ClientSession(
                read_reader,
                write_writer,
                **session_kwargs,  # type: ignore[arg-type]
            ) as session:
                try:
                    yield session
                finally:
                    # Closing the streams cancels the pumps.
                    with contextlib.suppress(Exception):
                        await write_writer.aclose()
                    with contextlib.suppress(Exception):
                        await read_writer.aclose()
                    tg.cancel_scope.cancel()

    async def _outbound_pump(self, write_reader: MemoryObjectReceiveStream[SessionMessage]) -> None:
        """ClientSession → tunnel: wrap each SessionMessage as an ``McpFrame``."""
        try:
            async with write_reader:
                async for session_message in write_reader:
                    body = session_message.message.model_dump(
                        by_alias=True, exclude_none=True, mode="json"
                    )
                    await self.outgoing.put(McpFrame(server_id=self.server_id, frame=body))
        except anyio.ClosedResourceError:
            pass

    async def _inbound_pump(
        self, read_writer: MemoryObjectSendStream[SessionMessage | Exception]
    ) -> None:
        """tunnel → ClientSession: unwrap ``McpFrame``s and surface errors."""
        try:
            async with read_writer:
                while True:
                    frame = await self.incoming.get()
                    if isinstance(frame, McpFrame):
                        if frame.server_id != self.server_id:
                            log.warning("ignoring frame for other server_id=%s", frame.server_id)
                            continue
                        try:
                            message = JSONRPCMessage.model_validate(frame.frame)
                        except Exception as exc:  # noqa: BLE001
                            log.exception("failed to parse JSONRPC inbound frame")
                            await read_writer.send(exc)
                            continue
                        await read_writer.send(SessionMessage(message))
                    elif isinstance(frame, TunnelError):
                        log.error("tunnel_error: %s %s", frame.code, frame.message)
                        await read_writer.send(
                            RuntimeError(f"tunnel_error[{frame.code}]: {frame.message}")
                        )
                        # Closing the stream signals "server gone" to ClientSession.
                        return
                    else:
                        log.warning("unexpected frame type on inbound: %s", type(frame).__name__)
        except anyio.ClosedResourceError:
            pass
