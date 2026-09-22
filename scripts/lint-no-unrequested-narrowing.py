#!/usr/bin/env python3
"""No-unrequested-narrowing lint for the omniagent delivery path.

Defect class A5 (root cause of the hidden `interactive_max_iterations` cap):
agent-authored code may NOT invent a default, cap, limit or fallback that
NARROWS or OVERRIDES an operator-configured setting. The historical instance was

    let iter_limit = max_iterations_for_plan(&cfg.config_snapshot(), plan) as i32;
    let interactive_iter_limit = iter_limit.min(cfg.interactive_max_iterations as i32);

backed by `interactive_max_iterations: get(..., "12")`: a self-invented cap that
truncated ordinary operator chats, was never requested, saved no money and was
invisible to the operator.

The rule (Dev-Task-Common-Rules S13 / Dev-Self-Invention-Check):
  * an operator-configured setting is USED AS GIVEN; it may not be silently
    narrowed by a numeric literal, by a literal bound to a local variable, by a
    second setting, or by a `unwrap_or(<non-zero>)` fallback;
  * a narrowing that is genuinely required must be requested by the operator and
    justified inline (`// narrowing-ok: <reason>`) or in the dated baseline file
    `scripts/narrowing-defaults-baseline.json`;
  * anything else fails this check.

Detection (mechanical, taint based, STATEMENT level):
  The unit of inspection is a Rust statement - the text between `;` / `{` / `}`
  separators at bracket depth 0 - NOT a physical line. A multi-line chain
  (`base\n    .min(12)`) is one unit, so moving the receiver to its own line
  cannot defuse the check.
    1. TAINT: a statement that binds an identifier to an operator setting - a
       struct field of the settings (`cfg.<key>`), a `get("<key>", ...)` lookup
       or a budget resolver (`max_iterations_for_plan(...)`) - marks that
       identifier. The fixed point runs over the whole file, so
       `let base = cfg.max_iterations_plan;` taints `base` and `base.min(12)`
       is flagged even though the setting and the literal are stated apart.
    2. LITERAL BINDINGS: `let cap = 12;` records `cap -> 12`; using `cap` as the
       min/clamp/unwrap_or argument is the same as writing the literal.
    3. BUDGET NAMES: an identifier whose NAME is a budget knob counts as
       setting-bound as well - `base`, `cap`, `limit`, `budget`,
       `max_iterations*`, `*_cap`, `*_budget`, any `*token_budget*`. Without
       type resolution this is what makes `fn f(base: u32) -> u32 { base.min(12) }`
       detectable. Unambiguous names (`max_iterations*`, `*token_budget*`,
       `iter_limit`, `iter_budget`, `*_cap`, `*_budget`) count anywhere; the
       ambiguous words `base`/`cap`/`limit`/`budget`/`iterations` count only as
       PLAIN locals/parameters, never as a member or index access
       (`params.limit`, `args["limit"]` are request parameters of a handler or
       plugin, not operator settings).
    4. FLAGGED: `.min/.max/.clamp(...)` on a setting-bound receiver where an
       argument is a literal or another setting-bound value; bare
       (rework, thread 2835) ALSO: `.min/.max/.clamp(...)` on a setting-bound
       receiver whose argument is a BUDGET-NAMED value with no literal and no
       `cfg.`/`get()` at the call site - `let cap = cfg.<key>; base.min(cap as
       i32)`, `let read_cap = cap; base.min(read_cap as i32)`, and
       `fn f(base: i32, interactive_cap: u32) { base.min(interactive_cap as
       i32) }` (the historic helper verbatim). The reviewer proved the previous
       version missed every one of these (RC=0). EXEMPT: both operands read an
       operator setting at the call site (`reduce_target.min(must_fit_target)`,
       the operator-visible stricter-of-two configured budgets).
       `min(...)`/`max(...)`/`std::cmp::min(...)` with a setting-bound argument
       and a literal; `.unwrap_or(<non-zero>)` on a setting-bound receiver.
    5. LIMITATION (documented, not hidden): a narrowing operand that carries
       NEITHER a literal NOR a budget-shaped name (`base.min(req_len)`) cannot
       be told apart from an ordinary request-parameter clamp without type or
       dataflow analysis, so it is NOT flagged; review must justify it.
    6. NOT flagged: arithmetic on an already-resolved budget
       (`let half = iter_limit / 2; half.max(3)`), `.max(0)` floors, and
       request-parameter handling in HTTP handlers / plugins.

Exemptions (all justified, never silent):
  * inline `// narrowing-ok: <reason>` on the flagged line or the line above;
  * a dated baseline entry with `justification` and `operator_request` fields;
  * the setting's OWN default-definition site: `get("<key>", "N")` followed by
    `.parse().unwrap_or(N)` with the SAME numeric value, bound to a name that
    matches the key (`<key>: get("<key>", "N")...` in the settings struct or
    `let <key> = get("<key>", "N")...`). That IS the documented default of the
    setting. A `get("<key>", "N")` bound to an unrelated name
    (`let base = get("max_iterations_plan", "12")...`) is NOT exempt: that is a
    new default the agent invented for the operator's knob.
  * `unwrap_or(N)` on a setting declared `Option<...>`: for an OPTIONAL setting
    the operator has two states (configured = Some, used as given; unconfigured
    = None), so `unwrap_or` is the documented default of the UNSET state and
    cannot narrow a configured value. `min`/`clamp` on the resolved value stay
    forbidden for both kinds.

Usage:
  python3 scripts/lint-no-unrequested-narrowing.py [--root DIR] [--baseline FILE]
Exit codes: 0 clean, 1 unjustified narrowing found, 2 usage/setup error.
"""

from __future__ import annotations

import argparse
import bisect
import json
import os
import re
import sys

# ── patterns ────────────────────────────────────────────────────────────────
SETTING_GET_RE = re.compile(r'\bget\(\s*"([a-z0-9_]+)"')
SETTING_KEY_DEFAULT_RE = re.compile(r'\bget\(\s*"([a-z0-9_]+)"\s*,\s*"([^"]*)"')
FIELD_ACCESS_RE = re.compile(
    r'\b(?:cfg|config|ctx|self|agent_config|agent_cfg|settings|resolved|snapshot|snap'
    r'|opts|options|[A-Za-z0-9_]*_cfg|[A-Za-z0-9_]*_settings)\.([a-z0-9_]+)')
# budget/limit resolvers: the value they return IS the operator budget
RESOLVER_RE = re.compile(
    r'\b(?:[A-Za-z_][A-Za-z0-9_]*::)*([A-Za-z0-9_]*max_iterations[A-Za-z0-9_]*)\s*\(')
IDENT_RE = re.compile(r'[A-Za-z_][A-Za-z0-9_]*')
NUMBER_RE = re.compile(r'(?<![A-Za-z0-9_.])(\d+)(?![A-Za-z0-9_])')
NUM_SUFFIX_RE = re.compile(r'\b(\d+)(?:u|i)(?:8|16|32|64|128|size)\b')
JUSTIFY_RE = re.compile(r'narrowing-ok\s*:?\s*(.+)')
UNWRAP_OR_RE = re.compile(r'\.unwrap_or\s*\(')
LET_RE = re.compile(
    r'\b(?:let|const)\s+(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)'
    r'\s*(?::[^=;{}()]*?)?=\s*([^;]*?)\s*$', re.S)
OPTION_FIELD_RE = re.compile(r'pub\s+([a-z0-9_]+)\s*:\s*Option\s*<')
RUST_KEYWORDS = frozenset({
    'let', 'mut', 'if', 'else', 'match', 'for', 'while', 'loop', 'return',
    'fn', 'pub', 'as', 'in', 'move', 'true', 'false', 'self', 'Some', 'None',
    'Ok', 'Err', 'ref', 'break', 'continue', 'impl', 'struct', 'enum', 'use',
})
# unambiguous budget knobs: count as setting-bound wherever they appear
NAMEISH_STRONG = ('max_iterations', 'token_budget', 'iter_limit', 'iter_budget')
# ambiguous budget words: only as a plain local/parameter whose binding is NOT
# known-computed, and never as a member/index access (`params.limit`).
# `limit` is deliberately absent: in this codebase it is overwhelmingly a
# request parameter name (`read_logs(.., limit: Option<usize>)`, `args["limit"]`),
# and flagging it produces false positives on non-setting code.
NAMEISH_WEAK = frozenset({
    'base', 'cap', 'budget', 'iterations', 'iteration_cap', 'interactive_cap',
})
CALL_NAMES = ('min', 'max', 'clamp')
CORE_SETTINGS_FILES = (
    os.path.join('src', 'agent', 'config.rs'),
    os.path.join('src', 'server', 'settings.rs'),
)


# ── primitives ──────────────────────────────────────────────────────────────
def nameish(ident: str) -> bool:
    """True when an identifier name is unambiguously a budget/limit knob."""
    n = ident.lstrip('_').lower()
    if not n:
        return False
    if any(f in n for f in NAMEISH_STRONG):
        return True
    return n.endswith('_cap') or n.endswith('_budget')


def weak_nameish(expr: str) -> bool:
    """A WEAK budget word used as a plain local/parameter (not `a.limit`)."""
    for m in IDENT_RE.finditer(expr):
        i = m.start() - 1
        while i >= 0 and expr[i] in ' \t\n':
            i -= 1
        if i >= 0 and expr[i] in '.[':
            continue
        if m.group(0).lstrip('_').lower() in NAMEISH_WEAK:
            return True
    return False


def same_knob(name: str, key: str) -> bool:
    """True when a binding name names the same knob as the setting key."""
    n = name.lstrip('_').lower()
    if n == key or n.startswith(key) or key.startswith(n):
        return True
    noise = {'secs', 'ms', 'kb', 'mb', 'bytes', 'b', 'percent', 'pct', 'count'}
    return bool((set(n.split('_')) & set(key.split('_'))) - noise)


def ident_set(text: str) -> set[str]:
    return {t for t in IDENT_RE.findall(text) if t not in RUST_KEYWORDS}


def numbers_in(text: str) -> list[int]:
    return [int(n) for n in NUMBER_RE.findall(NUM_SUFFIX_RE.sub(r'\1', text))]


def numeric_literals(text: str, consts: dict[str, int]) -> list[int]:
    """Numeric literals in `text` plus the values of literal-bound identifiers."""
    lits = numbers_in(text)
    for ident in ident_set(text):
        if ident in consts:
            lits.append(consts[ident])
    return lits


# string literal (Rust strings may span lines, incl. `\` continuation) | char
# literal (single line, so lifetimes are not swallowed) | line comment.
MASK_RE = re.compile(
    r'r#*"(?:.|\n)*?"#*'
    r'|"(?:\\.|[^"\\])*"'
    r"|'(?:\\.|[^'\\\n])'"
    r'|//[^\n]*')


def mask_code(text: str) -> str:
    """Blank out comments and literal CONTENTS, keeping every byte offset.

    Structure-only view: a log message, doc string, raw-string test fixture or
    doc comment that merely mentions `.min(` / `unwrap_or(` is not a call site.
    Hand-written scanner (not one regex): only this way can a raw string whose
    content contains `"` be closed on its real `"#` terminator.
    """
    out = list(text)
    n = len(text)
    i = 0

    def blank(a: int, b: int) -> None:
        for k in range(a, min(b, n)):
            if out[k] != '\n':
                out[k] = ' '

    while i < n:
        ch = text[i]
        if ch == '/' and i + 1 < n and text[i + 1] == '/':        # line comment
            j = text.find('\n', i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
            continue
        if ch == 'r' and re.match(r'r#*"', text[i:i + 12]):       # raw string
            m = re.match(r'r(#*)"', text[i:i + 12])
            hashes = m.group(1)
            close = '"' + hashes
            j = text.find(close, i + len(m.group(0)))
            j = n if j < 0 else j + len(close)
            blank(i, j)
            i = j
            continue
        if ch == '"':                                            # string literal
            j = i + 1
            while j < n:
                if text[j] == '\\':
                    j += 2
                    continue
                if text[j] == '"':
                    j += 1
                    break
                j += 1
            blank(i, j)
            i = j
            continue
        if ch == "'":                                            # char literal
            if i + 2 < n and text[i + 2] == "'":
                blank(i, i + 3)
                i += 3
                continue
            if i + 1 < n and text[i + 1] == '\\':
                j = text.find("'", i + 2)
                j = n if j < 0 else j + 1
                blank(i, j)
                i = j
                continue
        i += 1
    return ''.join(out)


def blank_comments(text: str) -> str:
    """Blank out `//` comments keeping byte offsets stable."""
    return re.sub(r'//[^\n]*', lambda m: ' ' * len(m.group(0)), text)


def has_setting_access(text: str, keys: set[str]) -> bool:
    """True when `text` reads an operator setting (cfg.<key> / get("<key>")/resolver)."""
    for m in FIELD_ACCESS_RE.finditer(text):
        if m.group(1) in keys:
            return True
    if any(f'"{k}"' in text for k in keys):
        return True
    return bool(RESOLVER_RE.search(text))


def receiver_before(code: str, pos: int, stop: str = ';,={}') -> str:
    """Balanced receiver expression ending just before `pos` (a `.` or `(`).

    Whitespace is NOT a separator: the receiver of `base\n    .min(12)` is
    `base` and the chain may legitimately be wrapped onto the next line.
    Leading whitespace is trimmed off the returned expression.
    """
    j = pos - 1
    depth = 0
    while j >= 0:
        ch = code[j]
        if ch == ')':
            depth += 1
        elif ch == '(':
            if depth == 0:
                break
            depth -= 1
        elif depth == 0 and ch in stop:
            break
        j -= 1
    return code[j + 1:pos].strip()


def balanced_args(code: str, pos: int) -> str:
    """Text inside the parentheses of a call whose `(` is at `pos`."""
    start = pos + 1
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
    return code[start:i]


def find_method_calls(code: str) -> list[tuple[int, str, str, str]]:
    """[(pos, receiver, name, args)] for `.min(..)`/`.max(..)`/`.clamp(..)`."""
    out = []
    for m in re.finditer(r'\.([A-Za-z_][A-Za-z0-9_]*)\s*\(', code):
        name = m.group(1)
        if name not in CALL_NAMES:
            continue
        out.append((m.start(), receiver_before(code, m.start()),
                    name, balanced_args(code, m.end() - 1)))
    return out


def find_free_calls(code: str) -> list[tuple[int, str, str]]:
    """[(pos, name, args)] for bare/qualified `min(a, b)` / `std::cmp::min(a, b)`."""
    out = []
    for m in re.finditer(
            r'(?<![A-Za-z0-9_.])(?:[A-Za-z_][A-Za-z0-9_]*::)*([A-Za-z_][A-Za-z0-9_]*)\s*\(', code):
        name = m.group(1)
        if name not in CALL_NAMES:
            continue
        out.append((m.start(), name, balanced_args(code, m.end() - 1)))
    return out


def split_units(masked: str) -> list[tuple[int, int, str]]:
    """Statement units: [(char_offset, 1-based line, unit_text)].

    Units end at `;` / `{` / `}` at bracket depth 0, so a multi-line
    `base\\n    .min(12)` chain and a one-line `let base = <setting>;` are each
    inspected as a whole. Input is masked code, so literal contents never
    influence the split.
    """
    line_starts = [0] + [m.end() for m in re.finditer(r'\n', masked)]
    out: list[tuple[int, int, str]] = []
    n = len(masked)
    i = 0
    start = 0
    pdepth = 0
    while i < n:
        ch = masked[i]
        if ch in '([':
            pdepth += 1
        elif ch in ')]':
            pdepth = max(0, pdepth - 1)
        elif pdepth == 0 and ch in ';{}':
            out.append((start, bisect.bisect_right(line_starts, start), masked[start:i]))
            start = i + 1
        i += 1
    if masked[start:].strip():
        out.append((start, bisect.bisect_right(line_starts, start), masked[start:]))
    return out


# ── setting inventory ───────────────────────────────────────────────────────
def collect_setting_keys(root: str) -> set[str]:
    """Operator-configurable setting keys, read out of the code itself.

    Only CORE config resolution counts: a plugin's own tool-argument parsing
    (`args["limit"]`, `params.limit`) is a request parameter, not an operator
    setting, and must not be reported.
    """
    keys: set[str] = set()
    for rel in CORE_SETTINGS_FILES:
        path = os.path.join(root, rel)
        if not os.path.isfile(path):
            continue
        with open(path, encoding='utf-8', errors='replace') as fh:
            for line in fh:
                keys.update(SETTING_GET_RE.findall(blank_comments(line)))
    return keys


def collect_optional_setting_fields(root: str, keys: set[str]) -> set[str]:
    """Setting keys declared `Option<...>` in the agent config struct."""
    path = os.path.join(root, 'src', 'agent', 'config.rs')
    if not os.path.isfile(path):
        return set()
    with open(path, encoding='utf-8', errors='replace') as fh:
        text = fh.read()
    fields = {m.group(1) for m in OPTION_FIELD_RE.finditer(text)}
    return fields & keys


def rust_files(root: str) -> list[str]:
    out = []
    for base in ('src', 'tests', 'plugins'):
        top = os.path.join(root, base)
        for dirpath, _dirs, files in os.walk(top):
            for fn in files:
                if fn.endswith('.rs'):
                    out.append(os.path.join(dirpath, fn))
    return sorted(out)


# ── taint ───────────────────────────────────────────────────────────────────
def setting_bound(expr_masked: str, expr_raw: str, tainted: set[str],
                  keys: set[str], computed: frozenset[str] | set[str] = frozenset()) -> bool:
    """True when an expression reads an operator-configured value.

    `computed` holds identifiers bound to a value that is provably NOT a
    setting (`let base = 1u64 << n;`): such a name must not be re-read as a
    budget knob just because of its name.
    """
    ids = ident_set(expr_masked) - set(computed)
    if ids & tainted:
        return True
    if has_setting_access(expr_raw or expr_masked, keys):
        return True
    if any(nameish(i) for i in ids):
        return True
    return weak_nameish(expr_masked) and not (ident_set(expr_masked) <= set(computed))


def rhs_is_direct(rhs: str, tainted: set[str], keys: set[str]) -> bool:
    """True when an RHS reads a setting *as given* (no arithmetic on it)."""
    simplified = re.sub(r'\.(parse|clone|to_string|trim|as_str|unwrap|unwrap_or|expect)'
                        r'\s*(?:::?<[^>]*>)?\s*\([^()]*\)', '', rhs)
    simplified = re.sub(r'\bas\s+[A-Za-z0-9_:<>]+', '', simplified).strip()
    # a resolver call or a direct setting access is always "as given"
    if has_setting_access(simplified, keys):
        return True
    # derived arithmetic is NOT a setting binding
    if re.search(r'[+\-*/%]', simplified):
        return False
    if simplified in tainted:
        return True
    if re.fullmatch(r'[A-Za-z0-9_.:\[\]()<> ]+', simplified):
        return any(nameish(i) or i in tainted for i in ident_set(simplified))
    return False


def bind_from_unit(unit_masked: str, unit_raw: str, tainted: set[str],
                   consts: dict[str, int], keys: set[str],
                   computed: set[str]) -> None:
    """Update taint / literal / computed bindings from a `let NAME = RHS` unit."""
    m = LET_RE.search(unit_masked)
    if not m:
        return
    name, rhs_masked = m.group(1), m.group(2).strip().rstrip(',').strip()
    if not rhs_masked:
        return
    cleaned = NUM_SUFFIX_RE.sub(r'\1', rhs_masked)
    lits = numbers_in(cleaned)
    if len(lits) == 1 and not ident_set(cleaned):
        consts[name] = lits[0]
        return
    if rhs_is_direct(rhs_masked, tainted, keys) or has_setting_access(unit_raw, keys):
        tainted.add(name)
        computed.discard(name)
    else:
        computed.add(name)


# ── detection ───────────────────────────────────────────────────────────────
def binding_name(prefix: str) -> str | None:
    """The nearest `let NAME =` / `<field>:` binding before a call site."""
    best: tuple[int, str] | None = None
    for m in re.finditer(r'\b(?:let|const)\s+(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)'
                         r'\s*(?::[^=;{}()]*?)?=', prefix):
        best = (m.end(), m.group(1))
    for m in re.finditer(r'(?:^|[,\n;{])\s*([a-z_][a-z0-9_]*)\s*:', prefix):
        if best is None or m.end() > best[0]:
            best = (m.end(), m.group(1))
    return best[1] if best else None


def definition_ok(unit_raw: str, call_pos: int, args: str) -> bool:
    """`get("<key>", "N")` ... `unwrap_or(N)` - the knob's own documented default."""
    lits = numbers_in(args)
    if not lits:
        return False
    bind = binding_name(unit_raw[:call_pos])
    for m in SETTING_KEY_DEFAULT_RE.finditer(unit_raw):
        if m.start() >= call_pos:
            break
        key, default = m.group(1), m.group(2)
        try:
            value = int(default)
        except ValueError:
            continue
        if not all(n == value for n in lits):
            continue
        if bind and same_knob(bind, key):
            return True
        if not bind and re.search(
                rf'(?<![A-Za-z0-9_]){re.escape(key)}\s*:\s*$', unit_raw[:m.start()]):
            return True
    return False


def definition_chain_ok(unit_raw: str, call_pos: int, keys: set[str]) -> bool:
    """`get("<key>", ...)...min(N)` bound to `<key>` in a settings file.

    At the setting's OWN definition site the field initializer states the whole
    resolution of the knob: documented default, parse, and its unit bound
    (`token_usage_budget: get("token_usage_budget", "0").parse().unwrap_or(0).min(100)`
    - a percentage cannot exceed 100). That chain IS the definition of the
    setting; the audited defect class is code that re-narrows the RESOLVED
    setting somewhere else. The exemption needs all three: a core settings file,
    a `get("<key>", ...)` in the same statement, and the nearest binding naming
    that same key.
    """
    bind = binding_name(unit_raw[:call_pos])
    if not bind:
        return False
    for m in SETTING_GET_RE.finditer(unit_raw):
        if m.group(1) in keys and same_knob(bind, m.group(1)):
            return True
    return False


def scan_unit(unit: str, unit_raw: str, tainted: set[str],
              consts: dict[str, int], keys: set[str],
              optional_fields: set[str],
              computed: set[str] = frozenset(),
              is_settings_file: bool = False) -> list[tuple[int, str]]:
    """Narrowing findings in one statement unit: [(char_offset, message)]."""
    hits: list[tuple[int, str]] = []
    for pos, recv, name, args in find_method_calls(unit):
        lits = numeric_literals(args, consts)
        # `.max(0)` is a non-negative floor on a difference, not a narrowing.
        if name == 'max' and lits and all(n == 0 for n in lits):
            continue
        recv_raw = unit_raw[max(0, pos - len(recv)):pos]
        if is_settings_file and definition_chain_ok(unit_raw, pos, keys):
            continue
        if not setting_bound(recv, recv_raw, tainted, keys, computed):
            continue
        args_raw = unit_raw[pos:pos + len(args) + 1]
        # An argument counts as a narrowing operand when it is a literal (or a
        # literal-bound identifier) or when the argument ITSELF reads a setting
        # (`cfg.interactive_max_iterations`, `get("..","12")`, a resolver call).
        # Two ALREADY-RESOLVED values being combined (`reduce_target.min(
        # must_fit_target)`, e.g. the stricter of two operator budgets) is not a
        # narrowing of either setting and must not be reported.
        explicit_setting_arg = has_setting_access(args_raw, keys)
        if lits or explicit_setting_arg:
            what = 'literal ' + str(lits) if lits else 'another setting'
            hits.append((pos, f'{name}() narrows a setting-bound value with {what}'))
    for pos, name, args in find_free_calls(unit):
        args_raw = unit_raw[pos:pos + len(args) + 1]
        if not setting_bound(args, args_raw, tainted, keys, computed):
            continue
        lits = numeric_literals(args, consts)
        if name == 'max' and lits and all(n == 0 for n in lits):
            continue
        if lits:
            hits.append((
                pos, f'{name}() clamps a setting-bound argument with literal {lits}'))
    # ── arg-side narrowing: the cap arrives as a bare local / alias / param ──
    # Added in the defect-class-A5 rework (thread 2835). The two branches above
    # only fire when the narrowing operand carries a numeric literal or names a
    # setting through `cfg.`/`get(...)`. The historical self-invented cap passed
    # its value in differently:
    #     let cap = cfg.interactive_max_iterations; base.min(cap as i32)
    #     fn f(base: i32, interactive_cap: u32) -> i32 { base.min(interactive_cap as i32) }
    # Neither operand carries a literal, so the exact root-cause shape used to
    # sail through. Rule: `min`/`max`/`clamp` on a budget-ish receiver with a
    # budget-ish operand is a narrowing - UNLESS the call combines two values
    # that BOTH read an operator setting right here (`reduce_target.min(
    # must_fit_target)`, the operator-visible "stricter of the two configured
    # budgets"), which stays clean.
    for pos, recv, name, args in find_method_calls(unit):
        if name not in CALL_NAMES:
            continue
        recv_raw = unit_raw[max(0, pos - len(recv)):pos]
        if not setting_bound(recv, recv_raw, tainted, keys, computed):
            continue
        args_raw = unit_raw[pos:pos + len(args) + 1]
        if not args.strip():
            continue
        arg_ids = ident_set(args)
        if not (any(nameish(a) for a in arg_ids) or weak_nameish(args)):
            continue
        lits = numeric_literals(args, consts)
        if lits or has_setting_access(args_raw, keys):
            continue  # reported by the literal / explicit-setting branches above
        recv_explicit = bool(ident_set(recv) & tainted) or has_setting_access(recv_raw, keys)
        arg_explicit = bool(arg_ids & tainted) or has_setting_access(args_raw, keys)
        if recv_explicit and arg_explicit:
            continue  # stricter-of-two-operator-settings, both sides as given
        hits.append((
            pos,
            f'{name}() narrows a setting-bound value with budget-named operand '
            f'{sorted(arg_ids) or args.strip()!r} (no literal, no `cfg.`/`get()` '
            f'at the call site): if this cap is not operator-requested it is '
            f'defect class A5'))
    for m in UNWRAP_OR_RE.finditer(unit):
        args = args_raw = None
        args = unit[m.end():]
        depth = 1
        i = 0
        while i < len(args) and depth:
            if args[i] == '(':
                depth += 1
            elif args[i] == ')':
                depth -= 1
                if depth == 0:
                    break
            i += 1
        args = args[:i]
        args_raw = unit_raw[m.end():m.end() + i]
        lits = [n for n in numbers_in(args_raw or args) if n != 0]
        if not lits:
            continue
        if definition_ok(unit_raw, m.start(), args_raw or args):
            continue
        recv = receiver_before(unit, m.start())
        recv_raw = unit_raw[max(0, m.start() - len(recv)):m.start()]
        if not setting_bound(recv, recv_raw, tainted, keys, computed):
            continue
        if any(f.group(1) in optional_fields
               for f in FIELD_ACCESS_RE.finditer(recv_raw)):
            continue
        hits.append((
            m.start(),
            f'unwrap_or({lits}) silently substitutes a non-zero fallback '
            f'for a setting value'))
    return hits


REDEMPTION = (
    'use the configured value as given, or require the narrowing from the '
    'operator (task body) and justify it with `// narrowing-ok: <reason>` or a '
    'baseline entry')


def lint(root: str, baseline_path: str) -> tuple[int, list[str]]:
    keys = collect_setting_keys(root)
    optional_fields = collect_optional_setting_fields(root, keys)
    files = rust_files(root)
    baseline, base_problems = load_baseline(baseline_path)

    findings: list[str] = []
    for f in files:
        rel = os.path.relpath(f, root)
        try:
            with open(f, encoding='utf-8', errors='replace') as fh:
                raw = fh.read()
        except OSError:
            continue
        lines = raw.splitlines()
        masked = mask_code(raw)
        rawc = blank_comments(raw)
        tainted: set[str] = set()
        consts: dict[str, int] = {}
        computed: set[str] = set()
        is_settings_file = rel in CORE_SETTINGS_FILES
        for offset, line_no, unit in split_units(masked):
            if not unit.strip():
                continue
            unit_raw = rawc[offset:offset + len(unit)]
            for pos, msg in scan_unit(unit, unit_raw, tainted, consts,
                                      keys, optional_fields, computed,
                                      is_settings_file):
                call_line = line_no + unit[:pos].count('\n')
                src_line = lines[call_line - 1] if 0 <= call_line - 1 < len(lines) else ''
                if justification_for(call_line, lines):
                    continue
                if baseline_match(baseline['entries'], rel, src_line):
                    continue
                findings.append(
                    f'{rel}:{call_line}: {msg}\n    {src_line.strip()}\n'
                    f'    remediation: {REDEMPTION}')
            bind_from_unit(unit, unit_raw, tainted, consts, keys, computed)
    for p in base_problems:
        findings.append(f'baseline: {p}')
    return (1 if findings else 0), findings


# ── baseline / justification exemptions ─────────────────────────────────────
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
            problems.append(f'entry #{i} ({e.get("file", "?")}) has no justification')
        if not str(e.get('operator_request', '')).strip():
            problems.append(
                f'entry #{i} ({e.get("file", "?")}) has no operator_request reference')
    return {'entries': entries}, problems


def baseline_match(entries: list[dict], rel: str, code: str) -> bool:
    needle = ' '.join(code.split())
    for e in entries:
        if e.get('file') != rel:
            continue
        if ' '.join(str(e.get('code', '')).split()) == needle:
            return True
    return False


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
    print(f'narrowing-lint: {len(findings)} finding(s). A cap/limit that the '
          f'operator did not request must not be shipped; raise it as a proposal '
          f'task instead.', file=sys.stderr)
    return 1


if __name__ == '__main__':
    sys.exit(main())
