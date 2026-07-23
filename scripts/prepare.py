#!/usr/bin/env python3
"""
prepare.py — Format source code, then generate offline sqlx query cache for all workspace members.

Usage (inside omniagent container where DATABASE_URL is already set):
    python3 scripts/prepare.py

What it does:
    1. Runs `cargo fmt` to format all source code.
    2. Finds workspace members that depend on `sqlx` (by inspecting each Cargo.toml).
    3. Runs `cargo sqlx prepare -- -p <pkg>` for each detected package.
    4. Runs `cargo fmt` a final pass.

Note: `cargo sqlx prepare --workspace` does NOT capture queries from all members.
Each must be prepared explicitly via `-- -p <pkg>`. This script auto-detects which
packages need it (by looking at Cargo.toml, not source files).
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
    Find workspace members that depend on the `sqlx` crate by inspecting
    each member's Cargo.toml. Returns sorted unique package names.
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

        cargo_path = path / "Cargo.toml"
        if not cargo_path.exists():
            continue

        # Read Cargo.toml to get package name and check for sqlx dependency
        cargo_text = cargo_path.read_text()
        pkg_name = _parse_pkg_name(cargo_text)
        if not pkg_name:
            continue

        if _depends_on_sqlx(cargo_text):
            packages.add(pkg_name)
            print(f"  Detected: {pkg_name}")

    return sorted(packages)


def _parse_pkg_name(cargo_text: str) -> str | None:
    """Extract the package name from a Cargo.toml string."""
    for line in cargo_text.splitlines():
        line = line.strip()
        if line.startswith("name = "):
            return line.split("=", 1)[1].strip().strip('"')
    return None


def _depends_on_sqlx(cargo_text: str) -> bool:
    """
    Check whether a Cargo.toml depends on `sqlx` (as a direct dependency,
    not transitive). The sqlx crate typically appears as:
        sqlx = { version = "...", features = [...] }
    or simply:
        sqlx = "0.8"
    We also check that there isn't a [target.'cfg(...)'.dependencies]
    section masking it, but the simple regex catches the common case.
    """
    # Strip [target.'cfg(…)'.dependencies] and [dev-dependencies] to avoid false positives
    # We only want [dependencies] and workspace.dependencies
    # Simple approach: search for `sqlx` in the dependency sections.
    # Look for `sqlx = ` in a line that isn't inside a dev-dependencies block.
    in_dev = False
    in_workspace = False
    for line in cargo_text.splitlines():
        stripped = line.strip()

        if stripped.startswith("[dev-dependencies"):
            in_dev = True
            continue
        if stripped.startswith("[dependencies"):
            in_dev = False
            in_workspace = False
            continue
        if stripped.startswith("[workspace.dependencies"):
            in_workspace = True
            in_dev = False
            continue
        if stripped.startswith("[") and stripped.endswith("]"):
            # Entering another section — reset flags if not already in one we care about
            if not in_dev and not in_workspace:
                continue
            in_dev = False
            in_workspace = False
            continue

        if in_dev:
            continue
        if re.match(r'^sqlx\s*=', stripped):
            return True

    return False


def main() -> None:
    if not os.environ.get("DATABASE_URL"):
        print("❌ DATABASE_URL not set — run inside the omniagent container", file=sys.stderr)
        sys.exit(1)

    print("Step 1: Scanning workspace members for sqlx dependency")
    packages = find_sqlx_packages()
    if not packages:
        print("No sqlx-dependent packages found — nothing to prepare")
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
