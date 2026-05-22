"""Minimal wire protocol for the v0 transport spike.

This is the Python-side hand-written equivalent of what we'll later generate
from ``stdiod/schema/tunnel-protocol.json``. The spike validates that this
shape is sufficient - any discovered gap is folded back into the schema
before v1.

The frame envelope is intentionally **symmetric** and **opaque**: both
directions exchange the same `TunnelFrame` types, and `McpFrame.frame` is a
raw JSON-RPC body that the tunnel never inspects.
"""

from __future__ import annotations

from typing import Annotated, Any, Literal

from pydantic import BaseModel, Field


class ClientHello(BaseModel):
    """daemon → backend, first frame after connect."""

    type: Literal["client_hello"] = "client_hello"
    protocol_version: int
    device_id: str
    hostname: str
    label: str


class ServerHello(BaseModel):
    """backend → daemon, response to ``client_hello``."""

    type: Literal["server_hello"] = "server_hello"
    protocol_version: int


class McpFrame(BaseModel):
    """Symmetric MCP frame envelope.

    ``frame`` is the JSON-RPC body verbatim - request, response, or
    notification - addressed to ``server_id`` (a logical id for one of the
    daemon's child stdio servers). The same shape flows in both directions.
    """

    type: Literal["mcp_frame"] = "mcp_frame"
    server_id: str
    frame: dict[str, Any]


class TunnelError(BaseModel):
    """Structured non-JSON-RPC error (server crashed, unknown server_id, …)."""

    type: Literal["tunnel_error"] = "tunnel_error"
    server_id: str | None
    related_jsonrpc_id: int | str | None = None
    code: str
    message: str


TunnelFrame = Annotated[
    ClientHello | ServerHello | McpFrame | TunnelError,
    Field(discriminator="type"),
]


PROTOCOL_VERSION = 1
