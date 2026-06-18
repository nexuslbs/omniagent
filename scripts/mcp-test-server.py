#!/usr/bin/env python3
"""
Minimal MCP (Model Context Protocol) test server.
Listens on stdin/stdout using JSON-RPC 2.0 over stdio transport.

Provides two tools:
  - echo: Echoes back the input arguments
  - add: Adds two numbers

Usage: python3 mcp_test_server.py
"""

import json
import sys
import traceback


def main():
    # Step 1: Wait for initialize request
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue

        method = msg.get("method")
        msg_id = msg.get("id")

        if method == "initialize":
            # Respond with server info
            response = {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "mcp-test-server",
                        "version": "1.0.0"
                    }
                }
            }
            sys.stdout.write(json.dumps(response) + "\n")
            sys.stdout.flush()

        elif method == "notifications/initialized":
            # No response needed for notifications
            pass

        elif method == "tools/list":
            # List available tools
            response = {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {
                    "tools": [
                        {
                            "name": "echo",
                            "description": "Echo back the provided message",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "message": {
                                        "type": "string",
                                        "description": "Message to echo back"
                                    }
                                },
                                "required": ["message"]
                            }
                        },
                        {
                            "name": "add",
                            "description": "Add two numbers together",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "a": {
                                        "type": "number",
                                        "description": "First number"
                                    },
                                    "b": {
                                        "type": "number",
                                        "description": "Second number"
                                    }
                                },
                                "required": ["a", "b"]
                            }
                        }
                    ]
                }
            }
            sys.stdout.write(json.dumps(response) + "\n")
            sys.stdout.flush()

        elif method == "tools/call":
            params = msg.get("params", {})
            tool_name = params.get("name", "")
            arguments = params.get("arguments", {})

            if tool_name == "echo":
                message = arguments.get("message", "")
                result_data = {
                    "content": [
                        {
                            "type": "text",
                            "text": f"Echo: {message}"
                        }
                    ]
                }
                response = {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "result": result_data
                }

            elif tool_name == "add":
                a = arguments.get("a", 0)
                b = arguments.get("b", 0)
                total = a + b
                result_data = {
                    "content": [
                        {
                            "type": "text",
                            "text": str(total)
                        }
                    ]
                }
                response = {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "result": result_data
                }

            else:
                response = {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "error": {
                        "code": -32601,
                        "message": f"Tool not found: {tool_name}"
                    }
                }

            sys.stdout.write(json.dumps(response) + "\n")
            sys.stdout.flush()

        elif method == "shutdown":
            sys.exit(0)

        else:
            # Unknown method
            if msg_id:
                response = {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "error": {
                        "code": -32601,
                        "message": f"Method not found: {method}"
                    }
                }
                sys.stdout.write(json.dumps(response) + "\n")
                sys.stdout.flush()


if __name__ == "__main__":
    main()
