#!/usr/bin/env python3
"""Shared MCP server framework — standard MCP JSON-RPC over stdio.

Usage:
    from mcp_server_sdk import McpServer, ToolDef

    server = McpServer(name="my-tool", version="0.1.0")

    @server.tool(
        name="my_tool",
        description="Does something useful",
        input_schema={...}
    )
    def handle_my_tool(arguments):
        # Return (text, is_error)
        return ("result", False)

    server.run()
"""

import json
import logging
import os
import sys
import threading
from functools import wraps
from typing import Any, Callable, Optional

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(name)s] %(levelname)s %(message)s",
    stream=sys.stderr,
)

MCP_PROTOCOL_VERSION = "2025-03-26"


def _send_json(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def _make_success(req_id: int, result: dict) -> dict:
    return {"jsonrpc": "2.0", "id": req_id, "result": result}


def _make_error(req_id: int, code: int, message: str) -> dict:
    return {"jsonrpc": "2.0", "id": req_id, "error": {"code": code, "message": message}}


class McpServer:
    """An MCP stdio server that dispatches to registered tool handlers."""

    def __init__(self, name: str, version: str = "0.1.0"):
        self.name = name
        self.version = version
        self._tools: list[dict] = []
        self._handlers: dict[str, Callable] = {}
        self._initialized = False
        self._stdin_closed = threading.Event()
        self.log = logging.getLogger(name)

    def tool(
        self,
        name: str,
        description: str = "",
        input_schema: Optional[dict] = None,
    ):
        """Decorator to register a tool handler.

        The handler receives (arguments: dict) and must return
        (text: str, is_error: bool).
        """
        def decorator(func: Callable) -> Callable:
            self._tools.append({
                "name": name,
                "description": description,
                "inputSchema": input_schema or {
                    "type": "object",
                    "properties": {},
                    "required": [],
                },
            })
            self._handlers[name] = func

            @wraps(func)
            def wrapper(args: dict) -> tuple[str, bool]:
                return func(args)

            return wrapper
        return decorator

    def run(self):
        """Run the MCP stdio event loop."""
        self.log.info("%s MCP server starting (PID=%d)", self.name, os.getpid())

        # Background stdin monitor
        def _monitor():
            self._stdin_closed.wait()
            self.log.info("stdin closed detected")
        monitor = threading.Thread(target=_monitor, daemon=True)
        monitor.start()

        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue

            if line == "__EOF__":
                self._stdin_closed.set()
                continue

            try:
                request = json.loads(line)
            except json.JSONDecodeError as e:
                self.log.error("Failed to parse JSON-RPC: %s", e)
                continue

            method = request.get("method", "")
            req_id = request.get("id")

            try:
                self._dispatch(method, req_id, request.get("params", {}))
            except Exception as e:
                self.log.error("Handler error: %s", e)
                if req_id is not None:
                    _send_json(_make_error(req_id, -32603, str(e)))

        self.log.info("%s MCP server shutting down (stdin closed)", self.name)

    def _dispatch(self, method: str, req_id: Optional[int], params: dict):
        if method == "initialize":
            if req_id is not None:
                _send_json(_make_success(req_id, {
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {"tools": {"listChanged": False}},
                    "serverInfo": {"name": self.name, "version": self.version},
                }))
                self._initialized = True
                self.log.info("Initialized: %s v%s", self.name, self.version)

        elif method == "notifications/initialized":
            self.log.info("Client initialized notification received")

        elif method == "tools/list":
            if not self._initialized:
                if req_id is not None:
                    _send_json(_make_error(req_id, -32000, "Server not initialized"))
                return
            if req_id is not None:
                _send_json(_make_success(req_id, {"tools": self._tools}))
                self.log.info("tools/list returned %d tool(s)", len(self._tools))

        elif method == "tools/call":
            if not self._initialized:
                if req_id is not None:
                    _send_json(_make_error(req_id, -32000, "Server not initialized"))
                return
            if req_id is not None:
                tool_name = params.get("name", "")
                arguments = params.get("arguments", {}) or {}
                self._handle_call(req_id, tool_name, arguments)

        else:
            self.log.warning("Unknown method: %s", method)
            if req_id is not None:
                _send_json(_make_error(req_id, -32601, f"Method not found: {method}"))

    def _handle_call(self, req_id: int, tool_name: str, arguments: dict):
        handler = self._handlers.get(tool_name)
        if not handler:
            _send_json(_make_error(req_id, -32602, f"Unknown tool: {tool_name}"))
            return

        try:
            text, is_error = handler(arguments)
            _send_json(_make_success(req_id, {
                "content": [{"type": "text", "text": text}],
                "isError": is_error,
            }))
        except Exception as e:
            self.log.error("Tool '%s' failed: %s", tool_name, e)
            _send_json(_make_success(req_id, {
                "content": [{"type": "text", "text": f"Error: {e}"}],
                "isError": True,
            }))
