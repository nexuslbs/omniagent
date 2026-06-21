#!/usr/bin/env python3
"""test-python-tool MCP server — implements standard MCP JSON-RPC over stdio.

Tools:
  - wait: Sleep for N seconds (default 900)
  - echo: Echo back input text

Cancellation is detected when stdin closes (EOF).
"""

import json
import os
import sys
import time
import logging
import threading
from datetime import datetime, timezone

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [test-python-tool] %(levelname)s %(message)s",
    stream=sys.stderr,
)
log = logging.getLogger("mcp")

MCP_PROTOCOL_VERSION = "2025-03-26"
initialized = False
stdin_closed = threading.Event()


def send_json(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def make_success(req_id, result):
    return {"jsonrpc": "2.0", "id": req_id, "result": result}


def make_error(req_id, code, message):
    return {"jsonrpc": "2.0", "id": req_id, "error": {"code": code, "message": message}}


def handle_initialize(req_id):
    result = {
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {"tools": {"listChanged": False}},
        "serverInfo": {"name": "test-python-tool", "version": "0.1.0"},
    }
    send_json(make_success(req_id, result))
    log.info("Initialized: test-python-tool v0.1.0")


def handle_tools_list(req_id):
    tools = [
        {
            "name": "wait",
            "description": "Sleep for a specified duration in seconds (default 900 = 15 minutes)",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "duration_secs": {
                        "type": "integer",
                        "description": "Seconds to wait",
                        "default": 900,
                    }
                },
                "required": [],
            },
        },
        {
            "name": "echo",
            "description": "Echo back a greeting: 'Hello, {input}'",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "input": {"type": "string", "description": "Name to greet (default: GREETING_NAME env var or 'World')"}
                },
                "required": [],
            },
        },
        {
            "name": "save_datetime",
            "description": "Write the current date/time (ISO 8601 format) to a file",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to write the datetime to"}
                },
                "required": ["path"],
            },
        },
    ]
    send_json(make_success(req_id, {"tools": tools}))
    log.info("tools/list returned 3 tools")


def handle_wait(req_id, arguments):
    duration_secs = arguments.get("duration_secs", 900) if arguments else 900
    log.info("wait tool called: sleeping for %s second(s)", duration_secs)

    slept = 0
    cancelled = False
    for _ in range(duration_secs):
        if stdin_closed.is_set():
            cancelled = True
            break
        time.sleep(1)
        slept += 1

    if cancelled:
        send_json(
            make_success(
                req_id,
                {
                    "content": [
                        {
                            "type": "text",
                            "text": f"Waited for {slept} second(s) before cancellation",
                        }
                    ],
                    "isError": False,
                },
            )
        )
        log.info("wait tool cancelled after %s second(s)", slept)
        sys.exit(0)
    else:
        send_json(
            make_success(
                req_id,
                {
                    "content": [
                        {
                            "type": "text",
                            "text": f"Waited for {duration_secs} seconds",
                        }
                    ],
                    "isError": False,
                },
            )
        )
        log.info("wait tool completed: slept for %s second(s)", duration_secs)


def handle_echo(req_id, arguments):
    name = (arguments or {}).get("input", "")
    greeting_name = os.environ.get("GREETING_NAME", "World")
    if not name:
        name = greeting_name
    text = f"Hello, {name}"
    log.info("echo tool called: text='%s'", text)
    send_json(
        make_success(
            req_id,
            {
                "content": [{"type": "text", "text": text}],
                "isError": False,
            },
        )
    )
    log.info("echo tool completed")


def handle_save_datetime(req_id, arguments):
    path = (arguments or {}).get("path")
    if not path:
        send_json(
            make_success(
                req_id,
                {
                    "content": [{"type": "text", "text": "Error: 'path' argument is required"}],
                    "isError": True,
                },
            )
        )
        log.warning("save_datetime tool called without path argument")
        return

    datetime_str = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    log.info("save_datetime tool called: path='%s'", path)

    try:
        with open(path, "w") as f:
            f.write(datetime_str)
        send_json(
            make_success(
                req_id,
                {
                    "content": [{"type": "text", "text": f"Saved datetime to {path}: {datetime_str}"}],
                    "isError": False,
                },
            )
        )
        log.info("save_datetime tool completed: wrote to %s", path)
    except Exception as e:
        send_json(
            make_success(
                req_id,
                {
                    "content": [{"type": "text", "text": f"Error writing to {path}: {e}"}],
                    "isError": True,
                },
            )
        )
        log.warning("save_datetime tool failed to write to %s: %s", path, e)


def main():
    global initialized

    log.info("test-python-tool MCP server starting (PID=%d)", os.getpid())

    # Background thread to detect stdin EOF
    def monitor_stdin():
        stdin_closed.wait()
        log.info("stdin closed detected - will cancel on next iteration")

    monitor = threading.Thread(target=monitor_stdin, daemon=True)
    monitor.start()

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue

        # If we detect stdin is about to close, signal it
        if line == "__EOF__":
            stdin_closed.set()
            continue

        try:
            request = json.loads(line)
        except json.JSONDecodeError as e:
            log.error("Failed to parse JSON-RPC: %s", e)
            continue

        method = request.get("method", "")
        req_id = request.get("id")

        if method == "initialize":
            if req_id is not None:
                handle_initialize(req_id)
                initialized = True

        elif method == "notifications/initialized":
            log.info("Client initialized notification received")

        elif method == "tools/list":
            if not initialized:
                if req_id is not None:
                    send_json(make_error(req_id, -32000, "Server not initialized"))
                continue
            if req_id is not None:
                handle_tools_list(req_id)

        elif method == "tools/call":
            if not initialized:
                if req_id is not None:
                    send_json(make_error(req_id, -32000, "Server not initialized"))
                continue
            if req_id is not None:
                params = request.get("params", {})
                tool_name = params.get("name", "")
                arguments = params.get("arguments", {})

                if tool_name == "wait":
                    handle_wait(req_id, arguments)
                elif tool_name == "echo":
                    handle_echo(req_id, arguments)
                elif tool_name == "save_datetime":
                    handle_save_datetime(req_id, arguments)
                else:
                    if req_id is not None:
                        send_json(
                            make_error(
                                req_id, -32602, f"Unknown tool: {tool_name}"
                            )
                        )

        else:
            log.warning("Unknown method: %s", method)
            if req_id is not None:
                send_json(make_error(req_id, -32601, f"Method not found: {method}"))

    log.info("test-python-tool MCP server shutting down (stdin closed)")


if __name__ == "__main__":
    main()
