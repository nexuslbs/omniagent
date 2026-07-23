#!/usr/bin/env python3
"""
prepare.py — Format source code, then generate offline sqlx query cache for all workspace members.

Usage (inside omniagent container where DATABASE_URL is already set):
    python3 scripts/prepare.py

What it does:
    1. Runs `cargo fmt` to format all source code.
    2. Scans every workspace member for `sql_forge!` macro usage.
    3. Runs `cargo sqlx prepare -- -p <pkg>` for each detected package.
    4. Runs `cargo fmt` a final pass.

Note: `cargo sqlx prepare --workspace` does NOT capture queries from all members.
Each must be prepared explicitly via `-- -p <pkg>`. This script auto-detects which
packages need it, so adding new plugin crates with sql_forge! requires no changes here.
"""

import os
import re
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


def find_sqlx_packages() -> list[str]:
    """
    Scan all workspace members for sql_forge! macro usage.
    Returns sorted unique package names (main crate + any plugin that uses sql_forge!).
    """
    cargo_toml = REPO_ROOT / "Cargo.toml"
    text = cargo_toml.read_text()

    # Parse workspace members
    m = re.search(r"members\s*=\s*\[(.*?)\]", text, re.DOTALL)
    if not m:
        print("Warning: could not parse workspace members from Cargo.toml")
        return []

    member_paths = [REPO_ROOT]  # main crate is at root
    seen_paths = {REPO_ROOT.resolve()}
    for entry in re.findall(r'"([^"]+)"', m.group(1)):
        p = (REPO_ROOT / entry).resolve()
        if p not in seen_paths:
            seen_paths.add(p)
            member_paths.append(p)

    packages = set()
    for path in member_paths:
        if not path.is_dir():
            continue

        # Get package name from Cargo.toml
        cargo_path = path / "Cargo.toml"
        if not cargo_path.exists():
            continue
        pkg_name = None
        with open(cargo_path) as f:
            for line in f:
                line = line.strip()
                if line.startswith("name = "):
                    pkg_name = line.split("=", 1)[1].strip().strip('"')
                    break
        if not pkg_name:
            continue

        # Scan for sql_forge! in .rs files
        has_forge = False
        for rs_file in sorted(path.rglob("*.rs")):
            if rs_file.read_text().find("sql_forge!") >= 0:
                has_forge = True
                break

        if has_forge:
            packages.add(pkg_name)
            print(f"  Detected: {pkg_name}")

    return sorted(packages)


def main() -> None:
    if not os.environ.get("DATABASE_URL"):
        print("❌ DATABASE_URL not set — run inside the omniagent container", file=sys.stderr)
        sys.exit(1)

    print("Step 1: Scanning workspace for sql_forge! usage")
    packages = find_sqlx_packages()
    if not packages:
        print("No packages with sql_forge! found — nothing to prepare")
        return

    print(f"\n  → {len(packages)} package(s) need preparation: {', '.join(packages)}")

    print("\nStep 2: cargo fmt --all")
    run(["cargo", "fmt", "--all"])

    print("\nStep 3: cargo sqlx prepare for each package")
    for pkg in packages:
        print(f"\n  Preparing {pkg} ...")
        run(["cargo", "sqlx", "prepare", "--", "-p", pkg])

    print("\nStep 4: cargo fmt --all (final pass)")
    run(["cargo", "fmt", "--all"])

    print(f"\n✅ prepare complete — offline data in .sqlx/")


if __name__ == "__main__":
    main()
