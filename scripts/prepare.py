#!/usr/bin/env python3
"""
prepare.py — Generate offline sqlx query cache for the entire workspace.

Usage (inside omniagent container where DATABASE_URL is already set):
    python3 scripts/prepare.py

What it does:
    1. Runs `cargo fmt` to format all source code.
    2. Runs `cargo sqlx prepare --workspace` to generate offline cache
       for the root crate AND all workspace member crates in one pass.
    3. Runs `cargo fmt` a final pass.

The offline cache in .sqlx/ must be committed to version control so
that SQLX_OFFLINE=true builds (production and CI) don't need a live
database at compile time.
"""

import os
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


def run(cmd: list[str]) -> None:
    """Run a command, print output, fail on non-zero exit."""
    result = subprocess.run(
        cmd,
        cwd=str(REPO_ROOT),
        capture_output=True,
        text=True,
        timeout=300,
    )
    if result.stdout:
        print(result.stdout, end="")
    if result.stderr:
        print(result.stderr, end="", file=sys.stderr)
    if result.returncode != 0:
        sys.exit(result.returncode)


def main() -> None:
    if not os.environ.get("DATABASE_URL"):
        print("❌ DATABASE_URL not set — run inside the omniagent container", file=sys.stderr)
        sys.exit(1)

    print("Step 1: cargo fmt --all")
    run(["cargo", "fmt", "--all"])

    print("Step 2: cargo sqlx prepare --workspace")
    run(["cargo", "sqlx", "prepare", "--workspace"])

    print("Step 3: cargo fmt --all (final pass)")
    run(["cargo", "fmt", "--all"])

    print(f"\n✅ prepare complete — offline data in .sqlx/")


if __name__ == "__main__":
    main()
