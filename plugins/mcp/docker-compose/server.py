#!/usr/bin/env python3
"""docker-compose MCP server — run docker compose commands."""

import os
import subprocess
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "sdk"))
from server import McpServer

server = McpServer(name="docker-compose", version="0.1.0")


@server.tool(
    name="docker_compose",
    description="Run docker compose commands (up, down, ps, logs, exec, build, restart, stop). The 'project' parameter is the service name from docker-compose.yml. Use 'up -d <service>' to start, 'down' to stop all, 'ps' to list status.",
    input_schema={
        "type": "object",
        "properties": {
            "project_dir": {
                "type": "string",
                "description": "Directory containing docker-compose.yml",
            },
            "command": {
                "type": "string",
                "description": "Docker compose command and arguments (e.g. 'up -d', 'ps', 'logs --tail=50', 'exec <service> <cmd>')",
            },
        },
        "required": ["project_dir", "command"],
    },
)
def handle_compose(arguments):
    project_dir = arguments.get("project_dir", "")
    command = arguments.get("command", "")
    if not project_dir:
        return ("Error: Missing 'project_dir' argument", True)
    if not command:
        return ("Error: Missing 'command' argument", True)

    if not os.path.isdir(project_dir):
        return (f"Error: Directory not found: {project_dir}", True)

    try:
        result = subprocess.run(
            ["docker", "compose"] + command.split(),
            cwd=project_dir,
            capture_output=True,
            text=True,
            timeout=300,
        )
        output = result.stdout
        if result.stderr:
            output += "\n--- stderr ---\n" + result.stderr
        if result.returncode != 0:
            return (f"Exit code: {result.returncode}\n{output}", True)
        MAX_CHARS = 50_000
        if len(output) > MAX_CHARS:
            output = output[:MAX_CHARS] + f"\n\n[... truncated from {len(output)} to ~{MAX_CHARS} chars]"
        return (output, False)
    except subprocess.TimeoutExpired:
        return ("Error: Command timed out after 300 seconds", True)
    except FileNotFoundError:
        return ("Error: docker command not found", True)
    except Exception as e:
        return (f"Error: {e}", True)


if __name__ == "__main__":
    server.run()
