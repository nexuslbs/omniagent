#!/usr/bin/env python3
"""
prepare.py — Generate offline sqlx query cache for the entire workspace.

Usage (inside omniagent container where DATABASE_URL is already set):
    python3 scripts/prepare.py

What it does:
    1. Runs `cargo fmt` to format all source code.
    2. Runs `cargo sqlx prepare --workspace` to generate offline cache
       for the root crate (includes all workspace member crates).
    3. For each plugin (plugins/tools/*/) that uses `sql_forge!()` or
       `sqlx::query!()`, runs `cargo sqlx prepare` in its crate directory
       so production builds (which compile each plugin as -p <name>) can
       resolve queries with SQLX_OFFLINE=true.
    4. Runs `cargo fmt` a final pass.

The offline cache files (*.sqlx/*.json) must be committed to version
control so that SQLX_OFFLINE=true builds (production and CI) don't need
a live database at compile time.
"""

import os
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
PLUGINS_DIR = REPO_ROOT / "plugins" / "tools"


def run(cmd: list[str], cwd: Path | None = None) -> None:
    """Run a command, print output, fail on non-zero exit."""
    result = subprocess.run(
        cmd,
        cwd=cwd or str(REPO_ROOT),
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


def needs_offline_cache(dir_path: Path) -> bool:
    """Check if a plugin crate uses sql_forge!() or sqlx::query!() macros.

    These macros resolve SQL at compile time and require the offline cache.
    Plain sqlx::query(\"...\") does NOT need offline data — the inline SQL
    string is parsed directly.
    """
    src_dir = dir_path / "src"
    if not src_dir.is_dir():
        return False
    # Only check .rs files — skip target/, .sqlx/ etc.
    for rs_file in src_dir.rglob("*.rs"):
        try:
            text = rs_file.read_text(encoding="utf-8", errors="replace")
            if "sql_forge!" in text or "sqlx::query!" in text:
                return True
        except Exception:
            continue
    return False


def main() -> None:
    if not os.environ.get("DATABASE_URL"):
        print("❌ DATABASE_URL not set — run inside the omniagent container", file=sys.stderr)
        sys.exit(1)

    print("Step 1: cargo fmt --all")
    run(["cargo", "fmt", "--all"])

    print("Step 2: cargo sqlx prepare --workspace")
    run(["cargo", "sqlx", "prepare", "--workspace"])

    # Step 3: prepare plugin crates that use sql_forge! / sqlx::query!
    if PLUGINS_DIR.is_dir():
        print("\nStep 3: Preparing plugin-local .sqlx caches ...")
        plugin_dirs = sorted(p for p in PLUGINS_DIR.iterdir() if p.is_dir())
        found = 0
        for pdir in plugin_dirs:
            if needs_offline_cache(pdir):
                found += 1
                print(f"  → {pdir.name} (sql_forge!/sqlx::query! detected)")
                run(["cargo", "sqlx", "prepare"], cwd=pdir)
        if found == 0:
            print("  (no plugins with sql_forge!/sqlx::query! found)")
        else:
            print(f"  ✓ {found} plugin crate(s) prepared")

    print("Step 4: cargo fmt --all (final pass)")
    run(["cargo", "fmt", "--all"])

    # Summary
    root_files = list((REPO_ROOT / ".sqlx").glob("*.json")) if (REPO_ROOT / ".sqlx").is_dir() else []
    plugin_files = []
    if PLUGINS_DIR.is_dir():
        for pdir in PLUGINS_DIR.iterdir():
            sqlx_dir = pdir / ".sqlx"
            if sqlx_dir.is_dir():
                plugin_files.extend(sqlx_dir.glob("*.json"))
    print(f"\n✅ prepare complete")
    print(f"   Root cache: {len(root_files)} queries")
    print(f"   Plugin caches: {len(plugin_files)} queries across {len(set(f.parent.parent.name for f in plugin_files))} plugin(s)")


if __name__ == "__main__":
    main()
