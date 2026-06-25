#!/usr/bin/env python3
"""filesystem MCP server — read, write, list, search, info tools."""

import os
import stat
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "sdk"))
from server import McpServer

server = McpServer(name="filesystem", version="0.1.0")

# Allowed base directories from env
DATA_DIR = os.environ.get("OMNI_DATA_DIR", "/opt/data")
WORKSPACE_DIR = os.environ.get("WORKSPACE_DIR", "/opt/workspace")


def _resolve_path(path: str) -> str:
    """Resolve and validate a path is within allowed directories."""
    requested = os.path.realpath(os.path.abspath(os.path.expanduser(path)))
    data_real = os.path.realpath(DATA_DIR)
    ws_real = os.path.realpath(WORKSPACE_DIR)
    if not requested.startswith(data_real) and not requested.startswith(ws_real):
        raise PermissionError(f"Access denied: path '{path}' is outside data or workspace directory")
    return requested


@server.tool(
    name="filesystem_read",
    description="READ A LOCAL FILE from disk. Use this to read any file on the filesystem.",
    input_schema={
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Absolute path to the file to read",
            },
        },
        "required": ["path"],
    },
)
def handle_read(arguments):
    path = arguments.get("path", "")
    if not path:
        return ("Error: Missing 'path' argument", True)
    safe_path = _resolve_path(path)
    try:
        with open(safe_path, "r", encoding="utf-8", errors="replace") as f:
            content = f.read()
        MAX_CHARS = 50_000
        if len(content) > MAX_CHARS:
            content = content[:MAX_CHARS] + f"\n\n[... truncated from {len(content)} to ~{MAX_CHARS} chars]"
        return (content, False)
    except FileNotFoundError:
        return (f"Error: File not found: {safe_path}", True)
    except PermissionError:
        return (f"Error: Permission denied: {safe_path}", True)
    except Exception as e:
        return (f"Error reading file: {e}", True)


@server.tool(
    name="filesystem_write",
    description="WRITE/CREATE A LOCAL FILE on disk. Creates parent directories automatically.",
    input_schema={
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Absolute path to the file to write",
            },
            "content": {
                "type": "string",
                "description": "The content to write to the file",
            },
        },
        "required": ["path", "content"],
    },
)
def handle_write(arguments):
    path = arguments.get("path", "")
    content = arguments.get("content", "")
    if not path:
        return ("Error: Missing 'path' argument", True)
    if not content:
        return ("Error: Missing 'content' argument", True)

    # Validate path
    safe_path = os.path.realpath(os.path.abspath(os.path.expanduser(path)))
    data_real = os.path.realpath(DATA_DIR)
    ws_real = os.path.realpath(WORKSPACE_DIR)
    if not safe_path.startswith(data_real) and not safe_path.startswith(ws_real):
        return (f"Access denied: path is outside the data or workspace directory", True)

    try:
        os.makedirs(os.path.dirname(safe_path), exist_ok=True)
        with open(safe_path, "w", encoding="utf-8") as f:
            f.write(content)
        return (f"Successfully wrote {len(content.encode('utf-8'))} bytes to {safe_path}", False)
    except Exception as e:
        return (f"Error writing file: {e}", True)


@server.tool(
    name="filesystem_list",
    description="LIST the contents of a directory on the filesystem.",
    input_schema={
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Absolute path to the directory to list",
            },
        },
        "required": ["path"],
    },
)
def handle_list(arguments):
    path = arguments.get("path", "")
    if not path:
        return ("Error: Missing 'path' argument", True)
    safe_path = _resolve_path(path)
    try:
        entries = os.listdir(safe_path)
        entries.sort()
        lines = []
        for name in entries:
            full = os.path.join(safe_path, name)
            try:
                st = os.stat(full)
                if stat.S_ISDIR(st.st_mode):
                    lines.append(f"[DIR] {name}")
                else:
                    size = st.st_size
                    mtime = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(st.st_mtime))
                    if size < 1024:
                        lines.append(f"[FILE] {name} ({size} bytes, {mtime})")
                    elif size < 1024 * 1024:
                        lines.append(f"[FILE] {name} ({size / 1024:.1f} KB, {mtime})")
                    else:
                        lines.append(f"[FILE] {name} ({size / (1024 * 1024):.1f} MB, {mtime})")
            except OSError:
                lines.append(f"[?]    {name}")
        return ("\n".join(lines) if lines else "(empty directory)", False)
    except FileNotFoundError:
        return (f"Error: Directory not found: {safe_path}", True)
    except PermissionError:
        return (f"Error: Permission denied: {safe_path}", True)
    except Exception as e:
        return (f"Error listing directory: {e}", True)


@server.tool(
    name="filesystem_search",
    description="SEARCH FOR FILES by glob pattern on the filesystem.",
    input_schema={
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Glob pattern to search for (e.g. '*.py', '**/*.rs')",
            },
            "path": {
                "type": "string",
                "description": "Directory to search (default: data dir)",
            },
        },
        "required": ["pattern"],
    },
)
def handle_search(arguments):
    import glob as glob_mod

    pattern = arguments.get("pattern", "")
    base_path = arguments.get("path", DATA_DIR)
    if not pattern:
        return ("Error: Missing 'pattern' argument", True)
    safe_base = _resolve_path(base_path)
    try:
        full_pattern = os.path.join(safe_base, pattern)
        matches = glob_mod.glob(full_pattern, recursive=True)
        matches.sort()
        if not matches:
            return (f"No files matching '{pattern}' in {safe_base}", False)
        result = "\n".join(matches[:200])
        if len(matches) > 200:
            result += f"\n... and {len(matches) - 200} more"
        return (result, False)
    except Exception as e:
        return (f"Error searching files: {e}", True)


@server.tool(
    name="filesystem_info",
    description="GET FILE/DIRECTORY INFO — size, permissions, type, timestamps.",
    input_schema={
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Absolute path to the file or directory",
            },
        },
        "required": ["path"],
    },
)
def handle_info(arguments):
    path = arguments.get("path", "")
    if not path:
        return ("Error: Missing 'path' argument", True)
    safe_path = _resolve_path(path)
    try:
        st = os.stat(safe_path)
        is_dir = stat.S_ISDIR(st.st_mode)
        file_type = "Directory" if is_dir else "File"
        mode_str = oct(stat.S_IMODE(st.st_mode))
        created = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(st.st_ctime))
        modified = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(st.st_mtime))
        accessed = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(st.st_atime))

        lines = [
            f"Path: {safe_path}",
            f"Type: {file_type}",
            f"Size: {st.st_size} bytes",
            f"Permissions: {mode_str}",
            f"Created: {created}",
            f"Modified: {modified}",
            f"Accessed: {accessed}",
        ]
        return ("\n".join(lines), False)
    except FileNotFoundError:
        return (f"Error: File not found: {safe_path}", True)
    except Exception as e:
        return (f"Error getting file info: {e}", True)


if __name__ == "__main__":
    server.run()
