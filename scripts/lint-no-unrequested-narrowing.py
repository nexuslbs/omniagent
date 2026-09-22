#!/usr/bin/env python3
"""No-unrequested-narrowing lint for the omniagent delivery path.

Defect class A5 (root cause of the hidden `interactive_max_iterations` cap):
agent-authored code may NOT invent a default, cap, limit or fallback that
NARROWS or OVERRIDES an operator-configured setting. The historical instance
was `iter_limit = base.min(interactive_cap)` backed by
`interactive_max_iterations: get(..., "12")`: a self-invented cap that
truncated ordinary operator chats, was never requested, saved no money and was
invisible to the operator.

The rule (Dev-Task-Common-Rules / Dev-Self-Invention-Check):
  * an operator-configured setting is USED AS GIVEN; it may not be silently
    narrowed by a numeric literal, by a second setting, or by a
    `unwrap_or(<non-zero>)` fallback that substitutes a different value;
  * a narrowing that is genuinely required must be requested by the operator
    and justified inline (`// narrowing-ok: <reason>`) or in the dated
    baseline file `scripts/narrowing-defaults-baseline.json`;
  * anything else fails this check.

Detection (mechanical, taint based):
  1. collect the operator-configurable setting keys from the code itself
     (`get("<key>", ...)` call sites in src/agent/config.rs);
  2. mark identifiers DIRECTLY bound to such a setting:
     `cfg.<key>` field access, `get("<key>", ...)` lookups and
     `max_iterations_for_plan(...)`-style budget resolvers;
  3. flag `.min(...)`, `.max(...)`, `.clamp(...)`, bare `min(...)`/`max(...)`
     and `.unwrap_or(<non-zero>)` calls where a DIRECTLY setting-bound operand
     is narrowed by a numeric literal or by a second setting-bound operand.
     Derived scalars (`let half = iter_limit / 2;`) are NOT flagged: they are
     arithmetic on an already-resolved budget, not a narrowing of the setting.

Exemptions (both must be justified, never silent):
  * inline `// narrowing-ok: <reason>` on the flagged line or the line above;
  * an entry in the dated baseline JSON with `justification` and
    `operator_request` fields;
  * the setting's own default-definition site, i.e. `get("<key>", "N")`
    followed by `.parse().unwrap_or(N)` with the SAME numeric value - that is
    where the documented default of the setting is declared, not a narrowing
    of a configured value.

Usage:
  python3 scripts/lint-no-unrequested-narrowing.py [--root DIR] [--baseline FILE]
Exit code 0 = clean, 1 = unjustified narrowing found (or bad justification),
2 = usage/lint error.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys

# ── patterns ────────────────────────────────────────────────────────────────
SETTING_GET_RE = re.compile(r'get\(\s*"([a-z0-9_]+)"')
SETTING_KEY_DEFAULT_RE = re.compile(r'get\(\s*"([a-z0-9_]+)"\s*,\s*"([^"]*)"')
FIELD_ACCESS_RE = re.compile(
    r'\b(?:cfg|config|ctx|self|agent_config|settings|[A-Za-z0-9_]*_cfg|[A-Za-z0-9_]*_settings)'
    r'\.([a-z0-9_]+)'
)
# budget/limit resolvers: the value they return IS the operator budget
RESOLVER_RE = re.compile(r'\b(?:[A-Za-z_][A-Za-z0-9_]*::)*([a-z0-9_]*max_iterations[a-z0-9_]*)\s*\(')
SETTING_NAMEISH_RE = re.compile(r'(?i)^(?:max_)?(?:iterations|iter_limit|iter_budget)[a-z0-9_]*$')
IDENT_RE = re.compile(r'[A-Za-z_][A-Za-z0-9_]*')
NUMBER_RE = re.compile(r'(?<![A-Za-z0-9_.])(\d+)(?![A-Za-z0-9_])')
JUSTIFY_RE = re.compile(r'narrowing-ok\s*:?\s*(.+)')
ASSIGN_RE = re.compile(
    r'^\s*(?:let\s+(?:mut\s+)?)?([A-Za-z_][A-Za-z0-9_]*)'
    r'\s*(?::[^=;]*?)?=\s*(.+?);?\s*$'
)
RUST_KEYWORDS = frozenset({
    'let', 'mut', 'if', 'else', 'match', 'for', 'while', 'loop', 'return',
    'fn', 'pub', 'as', 'in', 'move', 'true', 'false', 'self', 'Some', 'None',
    'Ok', 'Err', 'ref', 'break', 'continue', 'impl', 'struct', 'enum', 'use',
})
COMMENT_STRIP = re.compile(r'//.*$')
OPTION_FIELD_RE = re.compile(r'pub\s+([a-z0-9_]+)\s*:\s*Option\s*<')


def strip_comment(line: str) -> str:
    return COMMENT_STRIP.sub('', line)


def find_calls(code: str, names=('min', 'max', 'clamp')) -> list[tuple[str, str, str]]:
    """Return [(receiver_expr, name, args_text)] for `.name(...)` call sites."""
    out = []
    for m in re.finditer(r'\.([A-Za-z_][A-Za-z0-9_]*)\s*\(', code):
        name = m.group(1)
        if name not in names:
            continue
        start = m.end()  # after '('
        depth = 1
        i = start
        while i < len(code) and depth:
            if code[i] == '(':
                depth += 1
            elif code[i] == ')':
                depth -= 1
                if depth == 0:
                    break
            i += 1
        args = code[start:i]
        # receiver: walk back over a balanced expression ending just before '.'
        j = m.start() - 1
        depth = 0
        while j >= 0:
            ch = code[j]
            if ch == ')':
                depth += 1
            elif ch == '(':
                if depth == 0:
                    break
                depth -= 1
            elif depth == 0 and ch in ' ;,=+-*/&|!{':
                break
            j -= 1
        recv = code[j + 1:m.start()]
        out.append((recv, name, args))
    return out


def find_bare_calls(code: str, names=('min', 'max')) -> list[tuple[str, str]]:
    """Return [(name, args_text)] for bare `min(a, b)` / `max(a, b)` calls."""
    out = []
    for m in re.finditer(r'(?<![A-Za-z0-9_.])(min|max)\s*\(', code):
        if m.group(1) not in names:
            continue
        start = m.end()
        depth = 1
        i = start
        while i < len(code) and depth:
            if code[i] == '(':
                depth += 1
            elif code[i] == ')':
                depth -= 1
                if depth == 0:
                    break
            i += 1
        out.append((m.group(1), code[start:i]))
    return out


def ident_set(text: str) -> set[str]:
    return {t for t in IDENT_RE.findall(text) if t not in RUST_KEYWORDS}


def numbers_in(text: str) -> list[int]:
    return [int(n) for n in NUMBER_RE.findall(text)]


CORE_SETTINGS_FILES = (
    os.path.join('src', 'agent', 'config.rs'),
    os.path.join('src', 'server', 'settings.rs'),
)


def collect_optional_setting_fields(root: str, keys: set[str]) -> set[str]:
    """Setting keys declared `Option<...>` in AgentConfig.

    For an OPTIONAL setting the operator has two states: configured (Some, used
    as given) and unconfigured (None). `unwrap_or(N)` on such a field is the
    documented default for the UNSET state, not a narrowing of a configured
    value - narrowing a configured value would require `Some(v)` to be silently
    replaced, which `unwrap_or` cannot do. `min`/`max`/`clamp` on the resolved
    value stay forbidden for both kinds.
    """
    path = os.path.join(root, 'src', 'agent', 'config.rs')
    if not os.path.isfile(path):
        return set()
    with open(path, encoding='utf-8', errors='replace') as fh:
        text = fh.read()
    fields = {m.group(1) for m in OPTION_FIELD_RE.finditer(text)}
    return fields & keys


def collect_setting_keys(root: str) -> set[str]:
    """Operator-configurable setting keys, read out of the code itself.

    Only CORE config resolution counts as an operator-configurable setting: a
    plugin's own tool-argument parsing (`args["limit"]`) is a request
    parameter, not an operator setting, and must not be reported.
    """
    keys: set[str] = set()
    for rel in CORE_SETTINGS_FILES:
        path = os.path.join(root, rel)
        if not os.path.isfile(path):
            continue
        with open(path, encoding='utf-8', errors='replace') as fh:
            for line in fh:
                line = strip_comment(line)
                keys.update(SETTING_GET_RE.findall(line))
    return keys


def rust_files(root: str) -> list[str]:
    out = []
    for base in ('src', 'tests', 'plugins'):
        top = os.path.join(root, base)
        for dirpath, _dirs, files in os.walk(top):
            for fn in files:
                if fn.endswith('.rs'):
                    out.append(os.path.join(dirpath, fn))
    return sorted(out)


def has_setting_access(text: str, keys: set[str]) -> bool:
    """True when `text` reads an operator setting (cfg.<key> / get("<key>") / resolver)."""
    for m in FIELD_ACCESS_RE.finditer(text):
        if m.group(1) in keys:
            return True
    if any(f'"{k}"' in text for k in keys):
        return True
    return bool(RESOLVER_RE.search(text))


def is_direct_rhs(rhs: str, direct: set[str], keys: set[str]) -> bool:
    if has_setting_access(rhs, keys):
        return True
    # single identifier (optionally through lossless wrappers) already DIRECT
    simplified = re.sub(r'\.(parse|clone|to_string|as_str)\(\)', '', rhs)
    simplified = re.sub(r'\bas\s+[A-Za-z0-9_:<>]+', '', simplified).strip()
    if IDENT_RE.fullmatch(simplified) and simplified in direct:
        return True
    return False


def build_direct_set(files: list[str], keys: set[str]) -> dict[str, set[str]]:
    """file -> DIRECT identifiers (fixed point over simple assignments)."""
    direct: dict[str, set[str]] = {f: set() for f in files}
    lines_by_file: dict[str, list[str]] = {}
    for f in files:
        try:
            with open(f, encoding='utf-8', errors='replace') as fh:
                lines_by_file[f] = fh.read().splitlines()
        except OSError:
            lines_by_file[f] = []
        direct[f]  # local identifiers only: a struct field that merely shares a
        # setting's name (request.max_tokens) is NOT the operator setting.
    for _ in range(4):
        changed = False
        for f in files:
            for line in lines_by_file[f]:
                code = strip_comment(line)
                m = ASSIGN_RE.match(code)
                if not m:
                    continue
                name, rhs = m.group(1), m.group(2)
                if name in direct[f]:
                    continue
                if is_direct_rhs(rhs, direct[f], keys):
                    direct[f].add(name)
                    changed = True
        if not changed:
            break
    return direct


def setting_default_pair(recv: str) -> tuple[str, str] | None:
    m = SETTING_KEY_DEFAULT_RE.search(recv)
    if m:
        return m.group(1), m.group(2)
    return None


def is_definition_site(recv: str, arg_text: str, keys: set[str]) -> bool:
    """`get("<key>", "N").parse().unwrap_or(N)` - the setting's own default."""
    pair = setting_default_pair(recv)
    if not pair:
        return False
    key, default_str = pair
    if key not in keys:
        return False
    nums = numbers_in(arg_text)
    try:
        default_num = int(default_str)
    except (TypeError, ValueError):
        return False
    return bool(nums) and all(n == default_num for n in nums)


def justification_for(line_no: int, lines: list[str]) -> str | None:
    for idx in (line_no - 1, line_no - 2):
        if idx < 0 or idx >= len(lines):
            continue
        m = JUSTIFY_RE.search(lines[idx])
        if m:
            return m.group(1).strip()
    return None


def load_baseline(path: str) -> tuple[dict, list[str]]:
    if not os.path.isfile(path):
        return {'entries': []}, []
    with open(path, encoding='utf-8') as fh:
        data = json.load(fh)
    problems = []
    entries = data.get('entries', [])
    for i, e in enumerate(entries):
        if not str(e.get('justification', '')).strip():
            problems.append(f'baseline entry #{i} ({e.get("file", "?")}) has no justification')
        if not str(e.get('operator_request', '')).strip():
            problems.append(
                f'baseline entry #{i} ({e.get("file", "?")}) has no operator_request reference')
    return {'entries': entries}, problems


def baseline_match(entries: list[dict], rel: str, code: str) -> dict | None:
    needle = ' '.join(code.split())
    for e in entries:
        if e.get('file') != rel:
            continue
        if ' '.join(str(e.get('code', '')).split()) == needle:
            return e
    return None


def lint(root: str, baseline_path: str) -> tuple[int, list[str]]:
    keys = collect_setting_keys(root)
    optional_fields = collect_optional_setting_fields(root, keys)
    files = rust_files(root)
    direct = build_direct_set(files, keys)
    baseline, base_problems = load_baseline(baseline_path)

    findings: list[str] = []
    for f in files:
        rel = os.path.relpath(f, root)
        lines = []
        try:
            with open(f, encoding='utf-8', errors='replace') as fh:
                lines = fh.read().splitlines()
        except OSError:
            continue
        dset = direct.get(f, set())
        for idx, raw in enumerate(lines, start=1):
            code = strip_comment(raw)
            if not code.strip():
                continue
            hits: list[str] = []
            for recv, name, args in find_calls(code):
                recv_idents = ident_set(recv)
                direct_recv = bool(recv_idents & dset) or has_setting_access(recv, keys)
                direct_args = bool(ident_set(args) & dset) or has_setting_access(args, keys)
                lits = numbers_in(args)
                # `.max(0)` is a non-negative floor on a difference, not a
                # narrowing of the configured budget.
                if name == 'max' and lits and all(n == 0 for n in lits):
                    continue
                if direct_recv and (lits or direct_args):
                    hits.append(
                        f'{name}() narrows a setting-bound value with '
                        f'{"literal " + str(lits) if lits else "another setting"}')
            for name, args in find_bare_calls(code):
                direct_args = bool(ident_set(args) & dset) or has_setting_access(args, keys)
                lits = numbers_in(args)
                if name == 'min' and direct_args and lits:
                    hits.append(
                        f'{name}() clamps a setting-bound argument with literal {lits}')
            # unwrap_or fallback on a setting value
            for m in re.finditer(r'\.unwrap_or\s*\(', code):
                start = m.end()
                depth = 1
                i = start
                while i < len(code) and depth:
                    if code[i] == '(':
                        depth += 1
                    elif code[i] == ')':
                        depth -= 1
                        if depth == 0:
                            break
                    i += 1
                args = code[start:i]
                recv = code[:m.start()]
                # receiver expression: from the last statement boundary
                j = len(recv) - 1
                depth = 0
                while j >= 0:
                    ch = recv[j]
                    if ch == ')':
                        depth += 1
                    elif ch == '(':
                        if depth == 0:
                            break
                        depth -= 1
                    elif depth == 0 and ch in ' ;,={':
                        break
                    j -= 1
                recv = recv[j + 1:]
                lits = numbers_in(args)
                if not lits or all(n == 0 for n in lits):
                    continue
                if is_definition_site(recv, args, keys):
                    continue
                recv_tainted = bool(ident_set(recv) & dset) or has_setting_access(recv, keys)
                optional_unset = any(
                    m.group(1) in optional_fields for m in FIELD_ACCESS_RE.finditer(recv))
                if recv_tainted and not optional_unset:
                    hits.append(
                        f'unwrap_or({lits}) silently substitutes a non-zero fallback '
                        f'for a setting value')
            if not hits:
                continue
            just = justification_for(idx, lines)
            entry = baseline_match(baseline['entries'], rel, code)
            if just:
                continue
            if entry:
                continue
            for h in hits:
                findings.append(
                    f'{rel}:{idx}: {h}\n    {code.strip()}\n'
                    f'    remediation: use the configured value as given, or require the '
                    f'narrowing from the operator (task body) and justify it with '
                    f'`// narrowing-ok: <reason>` or the baseline file')
    for p in base_problems:
        findings.append(f'baseline: {p}')
    return (1 if findings else 0), findings


def main(argv=None) -> int:
    here = os.path.dirname(os.path.abspath(__file__))
    default_root = os.path.dirname(here)
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument('--root', default=default_root,
                    help='repository root to scan (default: repo containing this script)')
    ap.add_argument('--baseline', default=os.path.join(
        here, 'narrowing-defaults-baseline.json'))
    args = ap.parse_args(argv)

    root = os.path.abspath(args.root)
    if not os.path.isdir(os.path.join(root, 'src')):
        print(f'narrowing-lint: no src/ under {root}', file=sys.stderr)
        return 2
    code, findings = lint(root, args.baseline)
    if code == 0:
        print('narrowing-lint: OK - no unjustified narrowing default on an '
              'operator-configured setting')
        return 0
    print('narrowing-lint: FAIL - unrequested narrowing default(s) detected '
          '(defect class A5)', file=sys.stderr)
    for f in findings:
        print(f'  - {f}', file=sys.stderr)
    print(f'narrowing-lint: {len(findings)} finding(s). A cap/limit that is not '
          f'requested by the operator must not be shipped; raise it as a proposal '
          f'task instead.', file=sys.stderr)
    return 1


if __name__ == '__main__':
    sys.exit(main())
