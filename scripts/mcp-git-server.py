#!/usr/bin/env python3
"""
Git MCP Server for OmniAgent.

Provides git and GitHub operations as MCP tools.
Communicates via JSON-RPC 2.0 over stdio transport.

Tools:
  - create_github_repo: Create a repository under nexuslbs org on GitHub
  - clone_repo: Clone a git repository to local filesystem
  - commit_and_push: Stage, commit, and push changes to GitHub
  - git_status: Get git status of a repository

Environment requirements (read from /opt/data/.env or env vars):
  - GITHUB_APP_ID: GitHub App ID
  - GITHUB_INSTALLATION_ID: GitHub App Installation ID
"""

import json
import sys
import subprocess
import os
import base64
import time
import urllib.request
import urllib.error

# ── Config ──────────────────────────────────────────────────────────────

PRIVATE_KEY_PATH = "/opt/data/credentials/nexuslbs-app.2026-06-04.private-key.pem"
DOT_ENV_PATH = "/opt/data/.env"
GITHUB_ORG = "nexuslbs"
GITHUB_API = "https://api.github.com"

# ── Helpers ─────────────────────────────────────────────────────────────


def base64url_encode(data):
    """Base64url encode without padding."""
    if isinstance(data, str):
        data = data.encode()
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def load_env_vars():
    """Load GITHUB_APP_ID and GITHUB_INSTALLATION_ID from env or .env file."""
    app_id = os.environ.get("GITHUB_APP_ID")
    inst_id = os.environ.get("GITHUB_INSTALLATION_ID")

    if app_id and inst_id:
        return app_id, inst_id

    # Try reading from .env file
    if os.path.exists(DOT_ENV_PATH):
        with open(DOT_ENV_PATH) as f:
            for line in f:
                line = line.strip()
                if "=" in line and not line.startswith("#"):
                    k, v = line.split("=", 1)
                    v = v.strip().strip("'\"")
                    if k == "GITHUB_APP_ID" and not app_id:
                        app_id = v
                    elif k == "GITHUB_INSTALLATION_ID" and not inst_id:
                        inst_id = v

    if not app_id or not inst_id:
        sys.stderr.write(f"ERROR: GITHUB_APP_ID or GITHUB_INSTALLATION_ID not found\n")
        sys.stderr.flush()

    return app_id, inst_id


def create_jwt(app_id):
    """Create a JWT using RS256 with openssl subprocess."""
    header = base64url_encode(json.dumps({"alg": "RS256", "typ": "JWT"}))
    now = int(time.time())
    payload = base64url_encode(json.dumps({
        "iat": now - 60,
        "exp": now + 600,
        "iss": app_id
    }))

    signing_input = f"{header}.{payload}"

    result = subprocess.run(
        ["openssl", "dgst", "-sha256", "-sign", PRIVATE_KEY_PATH],
        input=signing_input.encode(),
        capture_output=True,
        timeout=15
    )

    if result.returncode != 0:
        sys.stderr.write(f"ERROR: openssl signing failed: {result.stderr.decode()}\n")
        sys.stderr.flush()
        return None

    signature = base64url_encode(result.stdout)
    return f"{signing_input}.{signature}"


def get_installation_token(app_id, inst_id):
    """Exchange a JWT for a GitHub App installation access token."""
    jwt_token = create_jwt(app_id)
    if not jwt_token:
        return None

    url = f"{GITHUB_API}/app/installations/{inst_id}/access_tokens"
    req = urllib.request.Request(url, data=b"", headers={
        "Authorization": f"Bearer {jwt_token}",
        "Accept": "application/vnd.github+json",
        "User-Agent": "omniagent-git-mcp",
    }, method="POST")

    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            data = json.loads(resp.read().decode())
            return data["token"]
    except urllib.error.HTTPError as e:
        sys.stderr.write(f"HTTP {e.code}: {e.read().decode()}\n")
        sys.stderr.flush()
        return None
    except urllib.error.URLError as e:
        sys.stderr.write(f"Network error: {e.reason}\n")
        sys.stderr.flush()
        return None


def run_git(cmd_args, cwd=None, env=None, timeout=60):
    """Run a git command and return (stdout, stderr, returncode)."""
    base_env = os.environ.copy()
    # Set safe directory for all paths
    base_env["GIT_TERMINAL_PROMPT"] = "0"
    if env:
        base_env.update(env)

    try:
        result = subprocess.run(
            cmd_args,
            capture_output=True,
            text=True,
            cwd=cwd,
            env=base_env,
            timeout=timeout
        )
        return result.stdout.strip(), result.stderr.strip(), result.returncode
    except subprocess.TimeoutExpired:
        return "", "Command timed out", -1
    except FileNotFoundError as e:
        return "", str(e), -1


# ── Tool Implementations ────────────────────────────────────────────────


def tool_create_github_repo(args):
    """Create a repository under nexuslbs org on GitHub."""
    repo_name = args.get("name", "").strip()
    if not repo_name:
        return error_result("Missing required parameter: name")
    
    description = args.get("description", "")
    private = args.get("private", False)
    
    app_id, inst_id = load_env_vars()
    if not app_id or not inst_id:
        return error_result("GitHub App credentials not configured")
    
    token = get_installation_token(app_id, inst_id)
    if not token:
        return error_result("Failed to obtain GitHub token")
    
    # Create repo via GitHub API
    url = f"{GITHUB_API}/orgs/{GITHUB_ORG}/repos"
    payload = json.dumps({
        "name": repo_name,
        "description": description,
        "private": private,
        "auto_init": False,
        "gitignore_template": "",
    }).encode()
    
    req = urllib.request.Request(url, data=payload, headers={
        "Authorization": f"Bearer {token}",
        "Accept": "application/vnd.github+json",
        "User-Agent": "omniagent-git-mcp",
        "Content-Type": "application/json",
    }, method="POST")
    
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            data = json.loads(resp.read().decode())
            clone_url = data.get("clone_url", "")
            ssh_url = data.get("ssh_url", "")
            html_url = data.get("html_url", "")
            
            return text_result(json.dumps({
                "success": True,
                "repo_name": repo_name,
                "clone_url": clone_url,
                "ssh_url": ssh_url,
                "html_url": html_url,
            }, indent=2))
    except urllib.error.HTTPError as e:
        body = e.read().decode()
        # 422 usually means repo already exists
        if e.code == 422:
            # Repo may already exist — try to get its URL
            clone_url = f"https://github.com/{GITHUB_ORG}/{repo_name}.git"
            return text_result(json.dumps({
                "success": True,
                "repo_name": repo_name,
                "clone_url": clone_url,
                "note": "Repository already exists"
            }, indent=2))
        return error_result(f"GitHub API error {e.code}: {body}")
    except urllib.error.URLError as e:
        return error_result(f"Network error: {e.reason}")


def tool_clone_repo(args):
    """Clone a git repository to local filesystem."""
    url = args.get("url", "").strip()
    if not url:
        return error_result("Missing required parameter: url")
    
    target_dir = args.get("dir", "").strip()
    
    # If no target dir specified, derive from repo name
    if not target_dir:
        repo_name = url.rstrip("/").split("/")[-1]
        if repo_name.endswith(".git"):
            repo_name = repo_name[:-4]
        target_dir = repo_name
    
    # Clone
    cmd = ["git", "clone", url, target_dir]
    stdout, stderr, rc = run_git(cmd, timeout=120)
    
    if rc != 0:
        # Check if directory already exists and is a git repo
        if os.path.isdir(os.path.join(target_dir, ".git")):
            return text_result(json.dumps({
                "success": True,
                "path": os.path.abspath(target_dir),
                "note": "Repository already exists locally"
            }, indent=2))
        return error_result(f"Clone failed: {stderr}")
    
    return text_result(json.dumps({
        "success": True,
        "path": os.path.abspath(target_dir),
        "note": "Repository cloned successfully"
    }, indent=2))


def tool_commit_and_push(args):
    """Stage, commit, and push changes."""
    repo_dir = args.get("repo_dir", "").strip()
    if not repo_dir:
        return error_result("Missing required parameter: repo_dir")
    
    message = args.get("message", "").strip()
    if not message:
        return error_result("Missing required parameter: message")
    
    files = args.get("files", [])
    
    # Check repo exists
    git_dir = os.path.join(repo_dir, ".git")
    if not os.path.isdir(git_dir):
        return error_result(f"Not a git repository: {repo_dir}")
    
    # Stage files
    if files and len(files) > 0:
        cmd = ["git", "add"] + files
    else:
        cmd = ["git", "add", "-A"]
    
    stdout, stderr, rc = run_git(cmd, cwd=repo_dir)
    if rc != 0:
        return error_result(f"git add failed: {stderr}")
    
    # Commit
    stdout, stderr, rc = run_git(["git", "commit", "-m", message], cwd=repo_dir)
    if rc != 0:
        if "nothing to commit" in stderr or "nothing to commit" in stdout:
            return text_result(json.dumps({
                "success": True,
                "note": "Nothing to commit — working tree clean"
            }, indent=2))
        return error_result(f"git commit failed: {stderr}")
    
    # Push — use GitHub App token for auth
    app_id, inst_id = load_env_vars()
    push_note = "committed locally"
    
    if app_id and inst_id:
        token = get_installation_token(app_id, inst_id)
        if token:
            # Get current remote URL
            stdout, _, _ = run_git(["git", "remote", "get-url", "origin"], cwd=repo_dir)
            remote_url = stdout.strip()
            
            if remote_url:
                # Build push URL with token
                if remote_url.startswith("https://"):
                    push_url = f"https://x-access-token:{token}@{remote_url.split('://', 1)[1]}"
                elif remote_url.startswith("git@"):
                    # SSH — just push without token
                    push_url = remote_url
                else:
                    push_url = remote_url
                
                # Get current branch
                stdout, _, _ = run_git(["git", "rev-parse", "--abbrev-ref", "HEAD"], cwd=repo_dir)
                branch = stdout.strip() or "main"
                
                stdout, stderr, rc = run_git(
                    ["git", "push", push_url, f"HEAD:{branch}"],
                    cwd=repo_dir, timeout=120
                )
                
                if rc == 0:
                    push_note = "committed and pushed"
                else:
                    # Push failed — still committed locally
                    push_note = f"committed locally (push failed: {stderr[:200]})"
    
    return text_result(json.dumps({
        "success": True,
        "repo_dir": os.path.abspath(repo_dir),
        "note": push_note
    }, indent=2))


def tool_git_status(args):
    """Get git status of a repository."""
    repo_dir = args.get("repo_dir", "").strip()
    if not repo_dir:
        return error_result("Missing required parameter: repo_dir")
    
    git_dir = os.path.join(repo_dir, ".git")
    if not os.path.isdir(git_dir):
        return error_result(f"Not a git repository: {repo_dir}")
    
    stdout, stderr, rc = run_git(["git", "status"], cwd=repo_dir)
    if rc != 0:
        return error_result(f"git status failed: {stderr}")
    
    # Also get branch info
    branch_out, _, _ = run_git(["git", "rev-parse", "--abbrev-ref", "HEAD"], cwd=repo_dir)
    
    return text_result(json.dumps({
        "success": True,
        "repo_dir": os.path.abspath(repo_dir),
        "branch": branch_out.strip(),
        "status": stdout
    }, indent=2))


# ── MCP Protocol Handlers ──────────────────────────────────────────────


def text_result(text):
    """Create a successful MCP tool result with text content."""
    return {
        "content": [{"type": "text", "text": text}]
    }


def error_result(message):
    """Create an error MCP tool result."""
    return {
        "content": [{"type": "text", "text": f"ERROR: {message}"}],
        "is_error": True
    }


TOOLS = [
    {
        "name": "create_github_repo",
        "description": "Create a new repository under the nexuslbs organization on GitHub",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Repository name (e.g., 'ping-pong')"
                },
                "description": {
                    "type": "string",
                    "description": "Repository description (optional)"
                },
                "private": {
                    "type": "boolean",
                    "description": "Whether the repository should be private (default: false)"
                }
            },
            "required": ["name"]
        }
    },
    {
        "name": "clone_repo",
        "description": "Clone a git repository to the local filesystem",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Git clone URL (HTTPS or SSH)"
                },
                "dir": {
                    "type": "string",
                    "description": "Target directory for the clone (optional, defaults to repo name)"
                }
            },
            "required": ["url"]
        }
    },
    {
        "name": "commit_and_push",
        "description": "Stage, commit, and push changes to GitHub. Generates auth token internally.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repo_dir": {
                    "type": "string",
                    "description": "Path to the git repository"
                },
                "message": {
                    "type": "string",
                    "description": "Commit message"
                },
                "files": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Specific files to stage (optional, defaults to all changes)"
                }
            },
            "required": ["repo_dir", "message"]
        }
    },
    {
        "name": "status",
        "description": "Get the git status of a repository (branch, changes, etc.)",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repo_dir": {
                    "type": "string",
                    "description": "Path to the git repository"
                }
            },
            "required": ["repo_dir"]
        }
    }
]


def handle_initialize(msg_id):
    """Handle MCP initialize request."""
    return {
        "jsonrpc": "2.0",
        "id": msg_id,
        "result": {
            "protocolVersion": "2025-03-26",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "mcp-git-server",
                "version": "1.0.0"
            }
        }
    }


def handle_list_tools(msg_id):
    """Handle MCP tools/list request."""
    return {
        "jsonrpc": "2.0",
        "id": msg_id,
        "result": {
            "tools": TOOLS
        }
    }


def handle_call_tool(msg_id, params):
    """Handle MCP tools/call request."""
    tool_name = params.get("name", "")
    arguments = params.get("arguments", {})

    tool_map = {
        "create_github_repo": tool_create_github_repo,
        "clone_repo": tool_clone_repo,
        "commit_and_push": tool_commit_and_push,
        "status": tool_git_status,
    }

    handler = tool_map.get(tool_name)
    if not handler:
        return {
            "jsonrpc": "2.0",
            "id": msg_id,
            "error": {
                "code": -32601,
                "message": f"Tool not found: {tool_name}"
            }
        }

    try:
        result = handler(arguments)
        return {
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": result
        }
    except Exception as e:
        import traceback
        sys.stderr.write(f"ERROR in tool {tool_name}: {traceback.format_exc()}\n")
        sys.stderr.flush()
        return {
            "jsonrpc": "2.0",
            "id": msg_id,
            "error": {
                "code": -32603,
                "message": f"Internal error: {str(e)}"
            }
        }


# ── Main Loop ───────────────────────────────────────────────────────────


def main():
    """Main MCP server loop — reads JSON-RPC from stdin, writes to stdout."""
    # Log startup info to stderr (visible in container logs)
    sys.stderr.write(f"[mcp-git-server] Starting with Python {sys.version}\n")
    sys.stderr.write(f"[mcp-git-server] Private key exists: {os.path.exists(PRIVATE_KEY_PATH)}\n")
    sys.stderr.write(f"[mcp-git-server] .env exists: {os.path.exists(DOT_ENV_PATH)}\n")
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
            # No response needed for notifications
            continue
        elif method == "tools/list":
            response = handle_list_tools(msg_id)
        elif method == "tools/call":
            params = msg.get("params", {})
            response = handle_call_tool(msg_id, params)
        elif method == "shutdown":
            sys.exit(0)
        else:
            if msg_id:
                response = {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "error": {
                        "code": -32601,
                        "message": f"Method not found: {method}"
                    }
                }
            else:
                continue

        sys.stdout.write(json.dumps(response) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
