#!/usr/bin/env python3
"""Core/Platform boundary lint for the omniagent delivery path.

Guardrail for code-plan C6 (Phase B) and defect class A4: platform-specific
behavior must never leak back into core delivery. Two incidents (threads
518/519, commits a501507/9e883d9/d3660bd) shipped telegram-specific delivery
logic in core: a first/last-only message collapse and an is_internal_telemetry
suppression, both driven by reading the telegram platform plugin's config from
src/agent/helpers.rs. Core delivery must stay platform-generic: every platform
plugin receives the full message stream and decides its own rendering.

Rules enforced here (all scoped to the core delivery path: src/agent +
src/platform; src/server plugin-management API is out of scope):

  R1. No non-comment, non-test code line in the delivery path may contain a
      platform-specific delivery identifier or platform name:
      `first_last_only`, `is_internal_telemetry`, `telegram` (case-insensitive).
      Comment/doc mentions that explain behavior are fine; code that branches
      on them is not. If a platform needs collapse/suppression it does it in
      its own plugin, never in core.

  R2. No `plugins_yaml::get_plugin(...)` config read may appear in the delivery
      path EXCEPT the documented LLM-provider api-key fallback in
      src/agent/executor.rs (which resolves PluginYamlType::Provider, i.e. the
      LLM provider, NOT a platform). A Platform-type config read in core
      delivery is exactly the A4 leak shape; any other un-typed read is
      treated as a violation too.

Exit code 0 = clean, 1 = violations found. Run from the repo root:
    python3 scripts/lint-core-platform-boundary.py
Optional: pass explicit files/dirs to scan instead of the defaults.

The same invariants are asserted at build time by the non-ignored guard tests
at the bottom of tests/plugin_tests.rs (core_delivery_*). Keep this script and
those tests in sync.
"""

import pathlib
import re
import sys

BANNED_DELIVERY_TOKENS = ("first_last_only", "is_internal_telemetry")
PLATFORM_NAME_RE = re.compile(r"telegram", re.IGNORECASE)
GET_PLUGIN = "plugins_yaml::get_plugin"
ALLOWED_GET_PLUGIN_FILE = "src/agent/executor.rs"  # LLM-provider api-key fallback
WINDOW_CHARS = 800

DEFAULT_ROOTS = ("src/agent", "src/platform")


def skipped_test_lines(text: str) -> set:
    """Line numbers (1-based) that lie inside a `#[cfg(test)] mod ... { }` block.

    Test code is allowed to mention platform names freely (fixtures, mock
    handshakes), so the lint only inspects production code lines.
    """
    skipped = set()
    in_test = False
    depth = 0
    for i, raw in enumerate(text.splitlines(), 1):
        if not in_test and "#[cfg(test)]" in raw:
            in_test = True
            depth = 0
        if in_test:
            depth += raw.count("{") - raw.count("}")
            if depth <= 0 and "}" in raw:
                in_test = False
            skipped.add(i)
    return skipped


def code_part(raw: str) -> str:
    """Return the code portion of a line (comment/doc stripped)."""
    return raw.split("//", 1)[0].strip()


def scan_file(path: pathlib.Path, violations: list) -> None:
    text = path.read_text(encoding="utf-8", errors="replace")
    skipped = skipped_test_lines(text)
    lines = text.splitlines()
    for i, raw in enumerate(lines, 1):
        if i in skipped:
            continue
        code = code_part(raw)
        if not code:
            continue
        low = code.lower()
        for token in BANNED_DELIVERY_TOKENS:
            if token in low:
                violations.append(
                    f"{path}:{i}: banned platform-specific delivery token "
                    f"'{token}' in delivery code (R1: core delivery must be "
                    f"platform-generic; collapse/suppression lives in the "
                    f"platform plugin, see code-plan C6 / threads 518-519)"
                )
        if PLATFORM_NAME_RE.search(code):
            violations.append(
                f"{path}:{i}: platform name reference in core delivery code "
                f"(R1: core never references a platform by name to shape "
                f"delivery)"
            )

    # R2: config reads in the delivery path (test regions excluded).
    text_lines = text.splitlines()
    for i, raw in enumerate(text_lines, 1):
        if i in skipped:
            continue
        if GET_PLUGIN not in raw:
            continue
        window = "\n".join(text_lines[i - 1 : i + 12])
        window = window[:WINDOW_CHARS]
        if "PluginYamlType::Platform" in window:
            violations.append(
                f"{path}:{i}: plugins_yaml::get_plugin resolves a PLATFORM "
                f"plugin config inside the core delivery path (R2: the A4 "
                f"leak shape - core must not read platform config to shape "
                f"delivery)"
            )
        elif not (
            str(path).endswith(ALLOWED_GET_PLUGIN_FILE)
            and "PluginYamlType::Provider" in window
        ):
            violations.append(
                f"{path}:{i}: plugins_yaml::get_plugin config read in the "
                f"core delivery path (R2: only the documented LLM-provider "
                f"api-key fallback in {ALLOWED_GET_PLUGIN_FILE} is allowed)"
            )


def collect_rs(paths) -> list:
    files = []
    for p in paths:
        p = pathlib.Path(p)
        if p.is_dir():
            for child in sorted(p.rglob("*.rs")):
                files.append(child)
        elif p.is_file() and p.suffix == ".rs":
            files.append(p)
    return files


def main(argv) -> int:
    repo_root = pathlib.Path(__file__).resolve().parent.parent
    if argv:
        targets = [pathlib.Path(a) if pathlib.Path(a).is_absolute()
                   else repo_root / a for a in argv]
    else:
        targets = [repo_root / d for d in DEFAULT_ROOTS]
    files = collect_rs(targets)
    violations = []
    for f in files:
        scan_file(f, violations)
    if violations:
        print(f"core-platform boundary lint FAILED ({len(violations)} violation(s)):")
        for v in violations:
            print(f"  - {v}")
        print(
            "Rule: core delivery (src/agent, src/platform) must never "
            "reference a platform by name/config or read platform plugin "
            "config to shape delivery. See AGENTS.md 'Core-Platform Boundary "
            "Rule' and code-plan C6."
        )
        return 1
    print(f"core-platform boundary lint OK ({len(files)} file(s) scanned)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
