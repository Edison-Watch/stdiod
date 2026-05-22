"""Stub stdio MCP server used by the v0 transport spike.

Exposes:

- ``add(a, b)`` - basic request/response.
- ``slow_count(n)`` - emits ``n`` progress notifications during execution.
- ``crash()`` - exits the process to validate error propagation.

Run as a stdio MCP server: ``python stub_mcp_server.py``.
"""

from __future__ import annotations

import asyncio
import os
import sys

from fastmcp import Context, FastMCP

mcp: FastMCP = FastMCP("stub")


@mcp.tool
def add(a: int, b: int) -> int:
    """Return the sum of two integers."""
    return a + b


@mcp.tool
async def slow_count(n: int, ctx: Context) -> str:
    """Emit `n` progress notifications then return a summary string."""
    for i in range(n):
        await ctx.report_progress(progress=i + 1, total=n, message=f"step {i + 1}/{n}")
        await asyncio.sleep(0.01)
    return f"counted to {n}"


@mcp.tool
async def ask_sample(prompt: str, ctx: Context) -> str:
    """Exercise server-initiated requests: server asks client to sample an LLM.

    For the spike the client returns a canned echo so the round-trip is
    deterministic.
    """
    result = await ctx.sample(prompt)
    return f"sampled:{result.text}"


@mcp.tool
def crash() -> str:
    """Hard-exit the subprocess to test error propagation."""
    sys.stdout.flush()
    sys.stderr.flush()
    # Bypass normal shutdown - the client should see the stream close mid-call.
    os._exit(1)


if __name__ == "__main__":
    mcp.run()
