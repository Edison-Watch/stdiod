"""Manual checkpoint harness for the stdiod feature.

Spins up the minimum stack needed to drive a stdio_tunnel server with the
MCP Inspector:

- A tempfile SQLite runtime DB with just the schema (no rows). The
  desired server is pushed to the daemon via a ``desired_state_update``
  frame after it connects, so the daemon's ``EDISON_DEVICE_ID`` can be
  anything - the harness adapts.
- The backend's ``/api/v1/stdio-tunnel/ws`` endpoint, so the Rust daemon
  has somewhere to connect.
- A FastMCP HTTP gateway at ``/mcp/<api-key>/`` that proxies through
  ``StdioTunnelTransport`` - exactly the path a real AI client would
  take in production.

Usage::

    # Terminal 1 - harness (prints the exact daemon command + inspector URL)
    uv run python -m stdiod.tools.inspector_harness --server filesystem

    # Terminal 2 - daemon: any device_id is fine (the harness adapts).
    EDISON_BACKEND_URL=http://127.0.0.1:8765 \\
      EDISON_API_KEY=anything \\
      EDISON_DEVICE_ID=manual-dev \\
      stdiod/target/debug/edison-stdiod run

    # Terminal 3 - inspector
    npx @modelcontextprotocol/inspector
    # In the UI: Transport=Streamable HTTP, URL=http://127.0.0.1:8765/mcp/<api-key>/

Ctrl-C the harness to tear everything down.
"""

from __future__ import annotations

import argparse
import asyncio
import os
import tempfile
from pathlib import Path
from unittest.mock import patch

import uvicorn
from fastapi import FastAPI
from fastmcp import Client, FastMCP
from fastmcp.server import create_proxy
from sqlalchemy import create_engine

import src.db.runtime_session as runtime_session
import src.stdio_tunnel.registry as reg_mod
from src.api.v1.routes import stdio_tunnel as stdio_tunnel_routes
from src.db.models.runtime_db import Base as RuntimeBase
from src.stdio_tunnel.protocol import DesiredServer, DesiredStateUpdate
from src.stdio_tunnel.registry import get_registry
from src.stdio_tunnel.transport import StdioTunnelTransport
from tests.stdio_tunnel._e2e_common import API_KEY, ServerSpec, fake_validate

# Pre-baked specs for the four reference servers. The user can also pass
# --command/--arg/--arg to roll their own.
PRESETS: dict[str, ServerSpec] = {
    "filesystem": ServerSpec(
        name="filesystem",
        command="npx",
        args=("-y", "@modelcontextprotocol/server-filesystem", str(Path.home())),
    ),
    "time": ServerSpec(name="time", command="uvx", args=("mcp-server-time",)),
    "playwright": ServerSpec(
        name="playwright",
        command="npx",
        args=("-y", "@playwright/mcp@latest"),
    ),
    "railway": ServerSpec(name="railway", command="railway", args=("mcp",)),
}


def _parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--server",
        choices=sorted(PRESETS),
        help="Pre-baked reference server preset.",
    )
    p.add_argument(
        "--name",
        default=None,
        help="MCP prefix name (defaults to the preset name or 'custom').",
    )
    p.add_argument("--command", help="Override command for a custom server.")
    p.add_argument(
        "--arg",
        action="append",
        default=[],
        help="Override arg (repeatable). Used with --command.",
    )
    p.add_argument("--port", type=int, default=8765, help="Backend port.")
    return p.parse_args()


def _resolve_spec(args: argparse.Namespace) -> ServerSpec:
    if args.command:
        return ServerSpec(
            name=args.name or "custom",
            command=args.command,
            args=tuple(args.arg),
        )
    if not args.server:
        raise SystemExit("pick a --server preset or pass --command")
    spec = PRESETS[args.server]
    if args.name:
        spec = ServerSpec(name=args.name, command=spec.command, args=spec.args)
    return spec


async def _wait_for_any_device() -> str:
    """Block until the WS endpoint registers a daemon; return its device_id."""
    registry = get_registry()
    while True:
        for did, conn in list(registry._devices.items()):  # noqa: SLF001
            if not conn.is_closed:
                return did
        await asyncio.sleep(0.2)


async def _amain() -> None:
    args = _parse_args()
    spec = _resolve_spec(args)

    # Empty per-run DB - the daemon's client_hello will return an empty
    # server_hello and we push the actual desired state afterwards.
    workspace = Path(tempfile.mkdtemp(prefix="stdiod-inspector-"))
    db_path = workspace / "sessions.db"
    engine = create_engine(f"sqlite:///{db_path}")
    RuntimeBase.metadata.create_all(engine)
    engine.dispose()

    # Reset module-globals so we hit the new DB / fresh registry.
    runtime_session._engine = None
    reg_mod._REGISTRY = None
    os.environ["RUNTIME_DB_URL"] = f"sqlite:///{db_path}"

    # Backend app: WS router + FastMCP HTTP gateway. Same structure as
    # src/server.py:232 in production.
    app = FastAPI()
    app.include_router(stdio_tunnel_routes.router, prefix="/api/v1")

    gateway = FastMCP(name="stdiod-inspector-gateway")
    mcp_asgi = gateway.http_app(path=f"/mcp/{API_KEY}/", stateless_http=False)
    app.mount("", mcp_asgi)

    # FastMCP's http_app() ships a Starlette lifespan that creates the
    # StreamableHTTPSessionManager. ``app.mount()`` does NOT propagate
    # lifespans, so drive it ourselves. Mirrors src/server.py:235.
    session_ready = asyncio.Event()

    async def _drive_mcp_lifespan() -> None:
        async with mcp_asgi.router.lifespan_context(mcp_asgi):  # type: ignore[attr-defined]
            session_ready.set()
            await asyncio.Event().wait()

    async def _mount_when_device_connects() -> None:
        # Wait for ANY daemon to connect, push the desired server, then
        # mount a gateway proxy at /mcp/<api-key>/.
        print("⏳ waiting for daemon to connect…", flush=True)
        device_id = await _wait_for_any_device()
        registry = get_registry()
        conn = registry.get(device_id)
        assert conn is not None
        print(
            f"✅ daemon `{device_id}` connected; pushing desired_state_update for `{spec.name}`",
            flush=True,
        )
        await conn.send_frame(
            DesiredStateUpdate(
                added=[
                    DesiredServer(
                        server_id=spec.name,
                        name=spec.name,
                        command=spec.command,
                        args=list(spec.args),
                        env=dict(spec.env or {}),
                        working_dir=None,
                        enabled=True,
                    )
                ],
            )
        )
        # Let the daemon spawn the subprocess before the Inspector calls
        # list_tools (otherwise it sees an empty surface).
        await asyncio.sleep(4.0)
        transport = StdioTunnelTransport(
            device_id=device_id, server_id=spec.name, registry=registry
        )
        proxy = create_proxy(Client(transport))
        await gateway.import_server(proxy, prefix=spec.name)
        print(
            f"🚀 gateway ready - open MCP Inspector and connect to:\n"
            f"     http://127.0.0.1:{args.port}/mcp/{API_KEY}/\n"
            f"   tools will be prefixed with `{spec.name}_`.",
            flush=True,
        )

    binary = Path(__file__).resolve().parents[1] / "target" / "debug" / "edison-stdiod"
    print("=" * 72)
    print("stdiod inspector harness ready.")
    print(f"  preset / spec: {spec.name}  ({spec.command} {' '.join(spec.args)})")
    print(f"  runtime DB:    {db_path}")
    print()
    print("Run the daemon in another terminal (any EDISON_DEVICE_ID / _API_KEY):")
    print(
        f"  EDISON_BACKEND_URL=http://127.0.0.1:{args.port} \\\n"
        f"    EDISON_API_KEY=anything \\\n"
        f"    EDISON_DEVICE_ID=manual-dev \\\n"
        f"    EDISON_DEVICE_LABEL=manual \\\n"
        f"    {binary} run"
    )
    print()
    print("Then run the inspector and connect to:")
    print(f"  http://127.0.0.1:{args.port}/mcp/{API_KEY}/")
    print("=" * 72)

    with patch("src.api.v1.routes.stdio_tunnel.validate_api_key", fake_validate):
        lifespan_task = asyncio.create_task(_drive_mcp_lifespan())
        await asyncio.wait_for(session_ready.wait(), timeout=5.0)
        mount_task = asyncio.create_task(_mount_when_device_connects())
        try:
            config = uvicorn.Config(app, host="127.0.0.1", port=args.port, log_level="info")
            server = uvicorn.Server(config)
            await server.serve()
        finally:
            mount_task.cancel()
            lifespan_task.cancel()


if __name__ == "__main__":
    asyncio.run(_amain())
