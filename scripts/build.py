#!/usr/bin/env python3
"""
build.py - Build omniagent and all builtin plugin binaries.

Automatically discovers all workspace member packages from Cargo.toml
and builds them with cargo build --release. No hardcoded package lists.

When adding a new plugin (platform or tool) to the workspace, it is
automatically included - no changes needed to this script, the Dockerfile,
or deploy.py.

Usage (from repo root):
    SQLX_OFFLINE=true python3 scripts/build.py

Exit code: 0 on success, non-zero on any failure.
"""
import json
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


def run(cmd: list[str], description: str) -> None:
    """Run a command, print output, fail on non-zero exit."""
    print(f"  $ {' '.join(cmd)}")
    t0 = time.time()
    result = subprocess.run(
        cmd,
        cwd=str(REPO_ROOT),
        capture_output=True,
        text=True,
        timeout=1800,
    )
    elapsed = time.time() - t0
    if result.stdout:
        lines = result.stdout.splitlines()
        out = "\n".join(lines[-30:])
        if len(lines) > 30:
            print(f"  (last 30 of {len(lines)} lines of stdout)")
        print(out)
    if result.stderr:
        lines = result.stderr.splitlines()
        for line in lines[-10:]:
            print(line, file=sys.stderr)
    if result.returncode != 0:
        print(f"\n  ❌ {description} - FAILED (exit {result.returncode}) [{elapsed:.0f}s]")
        sys.exit(result.returncode)
    print(f"  ✅ {description} [{elapsed:.0f}s]")


def get_workspace_packages() -> list[dict]:
    """Return all workspace member packages from cargo metadata."""
    r = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=str(REPO_ROOT),
        capture_output=True,
        text=True,
        timeout=60,
    )
    if r.returncode != 0:
        print("Failed to get cargo metadata", file=sys.stderr)
        print(r.stderr[-1000:], file=sys.stderr)
        sys.exit(1)
    return json.loads(r.stdout)["packages"]


def main() -> None:
    packages = get_workspace_packages()
    names = sorted(p["name"] for p in packages)

    print(f"  Workspace packages ({len(names)}): {', '.join(names)}")
    print()

    # Build the entire workspace in one invocation.
    # This compiles omniagent, db-migrations, and every plugin binary
    # (platforms + tools) - everything in [workspace].members.
    # Without --workspace, cargo only builds the root package and its deps,
    # skipping standalone member crates like the MCP servers.
    # Single-invocation builds are faster than per-package (-p) invocations
    # because cargo parallelizes across all crates.
    run(["cargo", "build", "--release", "--workspace"], "Build workspace (release)")

    print(f"\n✅ All {len(names)} packages built successfully")


if __name__ == "__main__":
    main()
