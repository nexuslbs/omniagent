#!/usr/bin/env python3
"""
Docker MCP Server for OmniAgent.

Provides Docker operations as MCP tools.
Communicates via JSON-RPC 2.0 over stdio transport.

Tools:
  - docker_ps: List running containers
  - docker_compose: Run docker compose commands (up, down, ps, logs, build, exec)
  - docker_exec: Run a command inside a running container
  - docker_build: Build a Docker image from a Dockerfile
  - docker_info: Show Docker system information

Safety:
  - All commands are restricted to `docker` invocations only
  - Shell metacharacters (|, ;, &&, ||, `, $(), >, <) are rejected
  - Timeouts prevent hung operations
"""

import json
import sys
import subprocess
import os
import shlex

# ── Safety ──────────────────────────────────────────────────────────────

SAFE_CMD_PREFIXES = ["docker"]
FORBIDDEN_CHARS = ["|", ";", "&", "`", "$", ">", "<", "*", "?", "[", "]", "{", "}", "(", ")", "!", "~"]
TIMEOUT_SECS = 120


def validate_command(tool_name, cmd_parts):
    """Validate that the command is safe to execute."""
    if not cmd_parts or not cmd_parts[0]:
        return False, "Empty command"

    if cmd_parts[0] not in SAFE_CMD_PREFIXES:
        return False, f"Only docker commands are allowed, got: {cmd_parts[0]}"

    # Check for forbidden shell metacharacters in all parts
    for i, part in enumerate(cmd_parts):
        for char in FORBIDDEN_CHARS:
            if char in part:
                return False, f"Forbidden character '{char}' in argument {i}: {part[:50]}"

    return True, ""


def run_docker(cmd_parts, timeout=TIMEOUT_SECS):
    """Run a docker command and return (stdout, stderr, returncode)."""
    valid, err = validate_command("docker", cmd_parts)
    if not valid:
        return "", f"Command rejected: {err}", -1

    try:
        result = subprocess.run(
            cmd_parts,
            capture_output=True,
            text=True,
            timeout=timeout
        )
        return result.stdout, result.stderr, result.returncode
    except subprocess.TimeoutExpired:
        return "", f"Command timed out after {timeout}s", -1
    except FileNotFoundError:
        return "", f"Command not found: {cmd_parts[0]}", -1
    except PermissionError:
        return "", f"Permission denied for: {cmd_parts[0]}", -1


# ── Tool Implementations ────────────────────────────────────────────────


def tool_docker_ps(args):
    """List running Docker containers."""
    all_flag = args.get("all", False)
    fmt = args.get("format", "table {{.Names}}\t{{.Status}}\t{{.Ports}}")

    cmd = ["docker", "ps"]
    if all_flag:
        cmd.append("--all")
    cmd.extend(["--format", fmt])

    stdout, stderr, rc = run_docker(cmd)
    if rc != 0:
        return error_result(f"Docker ps failed:\n{stderr}")

    return text_result(f"```\n{stdout}\n```" if stdout else "No containers running.")


def tool_docker_compose(args):
    """Run a docker compose command."""
    project_dir = args.get("dir", "").strip()
    command = args.get("command", "ps").strip()
    service = args.get("service", "").strip()
    extra_args = args.get("args", "").strip()

    cmd = ["docker", "compose"]
    if project_dir:
        cmd.extend(["--project-directory", project_dir])

    cmd.append(command)
    if service:
        cmd.append(service)
    if extra_args:
        # Split by spaces but respect quotes
        try:
            cmd.extend(shlex.split(extra_args))
        except ValueError as e:
            return error_result(f"Invalid extra args: {e}")

    stdout, stderr, rc = run_docker(cmd, timeout=300)
    if rc != 0:
        return error_result(f"Docker compose {command} failed:\n{stderr}")

    return text_result(f"```\n{stdout}\n```" if stdout else f"docker compose {command}: ok")


def tool_docker_exec(args):
    """Run a command inside a running container."""
    container = args.get("container", "").strip()
    if not container:
        return error_result("Missing required parameter: container")

    command = args.get("command", "").strip()
    if not command:
        return error_result("Missing required parameter: command")

    interactive = args.get("interactive", False)

    cmd = ["docker", "exec"]
    if interactive:
        cmd.append("-i")
    cmd.extend([container] + shlex.split(command))

    stdout, stderr, rc = run_docker(cmd, timeout=60)
    if rc != 0:
        return error_result(f"Docker exec failed:\n{stderr}")

    return text_result(f"```\n{stdout}\n```" if stdout else "Command completed (no output)")


def tool_docker_build(args):
    """Build a Docker image from a Dockerfile."""
    path = args.get("path", "").strip()
    if not path:
        return error_result("Missing required parameter: path")

    tag = args.get("tag", "").strip()
    dockerfile = args.get("dockerfile", "").strip()
    no_cache = args.get("no_cache", False)

    cmd = ["docker", "build", path]
    if tag:
        cmd.extend(["-t", tag])
    if dockerfile:
        cmd.extend(["-f", dockerfile])
    if no_cache:
        cmd.append("--no-cache")

    stdout, stderr, rc = run_docker(cmd, timeout=600)
    if rc != 0:
        return error_result(f"Docker build failed:\n{stderr}")

    return text_result(f"Build complete for {tag or path} ✅\n```\n{stdout[-1000:]}\n```")


def tool_docker_info(args):
    """Show Docker system information."""
    section = args.get("section", "version")

    if section == "version":
        cmd = ["docker", "version", "--format", "{{.Server.Version}}"]
    elif section == "info":
        cmd = ["docker", "info", "--format",
               "{{.ContainersRunning}} running, {{.ContainersStopped}} stopped, {{.Images}} images"]
    elif section == "df":
        cmd = ["docker", "system", "df"]
    else:
        return error_result(f"Unknown section: {section} (use: version, info, df)")

    stdout, stderr, rc = run_docker(cmd)
    if rc != 0:
        return error_result(f"Docker info failed:\n{stderr}")

    return text_result(f"Docker {section}:\n```\n{stdout}\n```")


def tool_docker_run(args):
    """Run a raw docker command (advanced users — all args go to docker)."""
    docker_args = args.get("args", "").strip()
    if not docker_args:
        return error_result("Missing required parameter: args")

    # Parse the raw docker args
    try:
        cmd_parts = shlex.split(docker_args)
    except ValueError as e:
        return error_result(f"Invalid args: {e}")

    cmd = ["docker"] + cmd_parts
    stdout, stderr, rc = run_docker(cmd, timeout=300)
    if rc != 0:
        return error_result(f"Docker command failed:\n{stderr}")

    return text_result(f"```\n{stdout}\n```" if stdout else "Command completed")


# ── MCP Protocol ────────────────────────────────────────────────────────


def text_result(text):
    return {"content": [{"type": "text", "text": text}]}


def error_result(message):
    return {"content": [{"type": "text", "text": f"ERROR: {message}"}], "is_error": True}


TOOLS = [
    {
        "name": "ps",
        "description": "List running Docker containers. Use all=true to include stopped ones.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "all": {"type": "boolean", "description": "Include stopped containers"},
                "format": {"type": "string", "description": "Go template format for output (optional)"}
            }
        }
    },
    {
        "name": "compose",
        "description": "Run docker compose commands (ps, up, down, logs, build, exec, stop, restart)",
        "inputSchema": {
            "type": "object",
            "properties": {
                "dir": {"type": "string", "description": "Project directory containing docker-compose.yml"},
                "command": {"type": "string", "description": "Compose subcommand (ps, up, down, logs, build, exec, stop, restart)"},
                "service": {"type": "string", "description": "Service name (optional, for targeted commands)"},
                "args": {"type": "string", "description": "Extra arguments (e.g., '-d' for up -d, '--tail=50' for logs)"}
            },
            "required": ["command"]
        }
    },
    {
        "name": "exec",
        "description": "Run a command inside a running container",
        "inputSchema": {
            "type": "object",
            "properties": {
                "container": {"type": "string", "description": "Container name or ID"},
                "command": {"type": "string", "description": "Command to run inside the container"},
                "interactive": {"type": "boolean", "description": "Keep stdin open (default: false)"}
            },
            "required": ["container", "command"]
        }
    },
    {
        "name": "build",
        "description": "Build a Docker image from a Dockerfile in a directory",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the build context directory"},
                "tag": {"type": "string", "description": "Image tag (e.g., 'my-app:latest')"},
                "dockerfile": {"type": "string", "description": "Alternative Dockerfile path (relative to context)"},
                "no_cache": {"type": "boolean", "description": "Disable build cache"}
            },
            "required": ["path"]
        }
    },
    {
        "name": "info",
        "description": "Get Docker system information (version, running containers, disk usage)",
        "inputSchema": {
            "type": "object",
            "properties": {
                "section": {
                    "type": "string",
                    "enum": ["version", "info", "df"],
                    "description": "Info section: 'version' (Docker version), 'info' (container count), 'df' (disk usage)"
                }
            }
        }
    },
    {
        "name": "run",
        "description": "Run a raw docker command (advanced). Args are passed directly to 'docker ...'",
        "inputSchema": {
            "type": "object",
            "properties": {
                "args": {"type": "string", "description": "Arguments to pass to docker CLI (e.g., 'system df', 'network ls', 'images')"}
            },
            "required": ["args"]
        }
    }
]


def handle_initialize(msg_id):
    return {
        "jsonrpc": "2.0",
        "id": msg_id,
        "result": {
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mcp-docker-server", "version": "1.0.0"}
        }
    }


def handle_list_tools(msg_id):
    return {"jsonrpc": "2.0", "id": msg_id, "result": {"tools": TOOLS}}


def handle_call_tool(msg_id, params):
    tool_name = params.get("name", "")
    arguments = params.get("arguments", {})

    tool_map = {
        "ps": tool_docker_ps,
        "compose": tool_docker_compose,
        "exec": tool_docker_exec,
        "build": tool_docker_build,
        "info": tool_docker_info,
        "run": tool_docker_run,
    }

    handler = tool_map.get(tool_name)
    if not handler:
        return {
            "jsonrpc": "2.0",
            "id": msg_id,
            "error": {"code": -32601, "message": f"Tool not found: {tool_name}"}
        }

    try:
        result = handler(arguments)
        return {"jsonrpc": "2.0", "id": msg_id, "result": result}
    except Exception as e:
        import traceback
        sys.stderr.write(f"ERROR in tool {tool_name}: {traceback.format_exc()}\n")
        sys.stderr.flush()
        return {
            "jsonrpc": "2.0",
            "id": msg_id,
            "error": {"code": -32603, "message": f"Internal error: {str(e)}"}
        }


def main():
    sys.stderr.write(f"[mcp-docker-server] Starting with Python {sys.version}\n")
    sys.stderr.write(f"[mcp-docker-server] Docker available: ")
    try:
        r = subprocess.run(["docker", "--version"], capture_output=True, text=True, timeout=5)
        sys.stderr.write(r.stdout.strip() + "\n")
    except Exception as e:
        sys.stderr.write(f"NO: {e}\n")
    sys.stderr.flush()

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
            response = handle_initialize(msg_id)
        elif method == "notifications/initialized":
            continue
        elif method == "tools/list":
            response = handle_list_tools(msg_id)
        elif method == "tools/call":
            response = handle_call_tool(msg_id, msg.get("params", {}))
        elif method == "shutdown":
            sys.exit(0)
        else:
            if msg_id:
                response = {
                    "jsonrpc": "2.0", "id": msg_id,
                    "error": {"code": -32601, "message": f"Method not found: {method}"}
                }
            else:
                continue

        sys.stdout.write(json.dumps(response) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
