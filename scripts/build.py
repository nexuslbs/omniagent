#!/usr/bin/env python3
"""
build.py — Strict workspace build with zero-tolerance for warnings.

Usage:
    # Production build (offline — uses .sqlx/ cache):
    SQLX_OFFLINE=true python3 scripts/build.py

    # Dev build (live DB — for use inside the container):
    DATABASE_URL=postgres://user:***@host:5432/db python3 scripts/build.py

What it does:
    1. cargo check --workspace --all-targets  (fails on ANY warning)
    2. cargo clippy --workspace --all-targets (fails on ANY clippy warning)
    3. cargo fmt --check                       (fails on formatting issues)
    4. cargo build --release -p omniagent      (release build — this is the main binary)
        - Uses SQLX_OFFLINE=true if set in env
        - Or verifies queries against live DATABASE_URL if set

Exit code: 0 on success, non-zero on any failure.
"""

import os
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


def run(cmd: list[str], description: str) -> None:
    """Run a command, print output, fail on non-zero exit."""
    print(f"\n{'='*60}")
    print(f"  {description}")
    print(f"  $ {' '.join(cmd)}")
    print(f"{'='*60}")

    result = subprocess.run(
        cmd,
        cwd=str(REPO_ROOT),
        capture_output=True,
        text=True,
        timeout=600,
    )

    if result.stdout:
        print(result.stdout)
    if result.stderr:
        print(result.stderr, file=sys.stderr)

    if result.returncode != 0:
        print(f"\n❌ {description} — FAILED (exit {result.returncode})")
        sys.exit(result.returncode)

    print(f"✅ {description} — OK")


def main() -> None:
    steps = [
        (
            # cargo check with -D warnings makes ANY warning a hard error
            ["cargo", "check", "--workspace", "--all-targets", "--", "-D", "warnings"],
            "Check: cargo check --workspace --all-targets (-D warnings)",
        ),
        (
            # cargo clippy with -D warnings
            ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"],
            "Lint: cargo clippy --workspace --all-targets (-D warnings)",
        ),
        (
            # cargo fmt --check
            ["cargo", "fmt", "--all", "--check"],
            "Format: cargo fmt --all --check",
        ),
    ]

    for cmd, desc in steps:
        run(cmd, desc)

    # Release build of the main binary
    build_cmd = ["cargo", "build", "--release", "-p", "omniagent"]
    run(build_cmd, "Build: cargo build --release -p omniagent")

    print(f"\n{'='*60}")
    print(f"  ✅ ALL CHECKS PASSED")
    print(f"{'='*60}")


if __name__ == "__main__":
    main()
