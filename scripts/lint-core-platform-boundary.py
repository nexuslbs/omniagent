#!/usr/bin/env python3
"""Core/Platform boundary lint for the omniagent delivery path.

Guardrail for code-plan C6 (Phase B) and defect class A4: platform-specific
behavior must never leak back into core delivery. Two incidents (threads
518/519, commits a501507/9e883d9/d3660bd) shipped telegram-specific delivery
logic in core: a first/last-only message collapse and an is_internal_telemetry
suppression, both driven by reading the telegram platform plugin's config from
src/agent/helpers.rs. Core delivery must stay platform-generic: every platform
plugin receives the full message stream and decides its own rendering.

Phase 1 rules (C6, all of them scoped to the core delivery path: src/agent +
src/platform):

  R1. No non-comment, non-test code line in the delivery path may contain a
      platform-specific delivery identifier or platform name:
      `first_last_only`, `is_internal_telemetry`, `telegram`, `mattermost`
      (case-insensitive). Comment/doc mentions that explain behavior are fine;
      code that branches on them is not. If a platform needs collapse or
      suppression it does it in its own plugin, never in core.

  R2. No `plugins_yaml::get_plugin(...)` config read may appear in the delivery
      path EXCEPT the documented LLM-provider api-key fallback in
      src/agent/executor.rs (which resolves PluginYamlType::Provider, i.e. the
      LLM provider, NOT a platform). A Platform-type config read in core
      delivery is exactly the A4 leak shape; any other un-typed read is
      treated as a violation too.

Phase 2 rules (audit V-12 of the implicit-hardcoded-dependencies audit, wiki
Reference/Omniagent/Hardcoded-Dependency-Audit.md sections 6 / 9 / 10). They
generalize the same principle to the whole "core knows a plugin, platform,
provider or tool BY NAME" class:

  R3. No platform-name comparison in core code: `<name> == "<platform>"`,
      `"<platform>" == <name>`, or a `match` arm keyed on a platform name
      (B1/B2 mattermost, B3 cli, B4/B5 platform hint tables). Core branches on
      the capabilities the plugin declares, never on its name.

  R4. No provider-name comparison (`provider.0 == "anthropic"`, A1) and no
      protocol catch-all that lands on a named provider (`_ => "openai"`, A3)
      in src/llm and src/vectorizer. A `match` arm on a protocol string that
      the plugin/config declared is fine; a silent fallback to a provider is
      not.

  R5. No first-party tool-name literal in core (`tool_name == "docker__compose"`
      C1, the hand-maintained read-only allowlists C2/C3, a literal
      prompt-tool fallback C5). Tool behavior comes from the plugin tool
      descriptors (audit V-2). Documented exception: a tool name used as the
      default of a configurable `*_tool` / `*_tool_name` settings key is the
      desired parameterizable pattern (C7) and stays allowed.

  R6. No hardcoded service endpoint in core: a loopback or wildcard URL (D2)
      or a docker-internal host:port (`http://qdrant:6333`, D1). Public FQDN
      URLs (docs, upstream provider APIs) are not flagged.

  R7. The same platform/provider/tool rules apply to the first-party tool
      plugins (`plugins/tools/*`) and to an `omni-plugins` checkout when one is
      present (B5, C3). A plugin's OWN declared tool ids are always allowed
      (they originate in the plugin, audit C8).

KNOWN_EXCEPTIONS is the documented per-case allowlist the V-12 plan calls for:
each entry carries the reason and names the follow-up fix that removes it.
Allowlisted findings are always reported (never silent) and an entry that
suppresses nothing while its file is scanned is reported as a stale warning so
it can be deleted as each fix lands.

Exit code 0 = clean, 1 = violations found. Run from the repo root:
    python3 scripts/lint-core-platform-boundary.py
Optional: pass explicit files/dirs to scan instead of the defaults.

The phase-1 invariants are asserted again at build time by the non-ignored
guard tests at the bottom of tests/plugin_tests.rs (core_delivery_*). The
phase-2 rules are covered by scripts/test_lint_core_platform_boundary.py,
whose fixtures reproduce every baseline violation of the audit (B1, B3, A1,
A3, C1, D2).
"""

import json
import os
import pathlib
import re
import sys

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent

# ---------------------------------------------------------------------------
# Phase 1 (C6) constants
# ---------------------------------------------------------------------------
BANNED_DELIVERY_TOKENS = ("first_last_only", "is_internal_telemetry")
# Platform names that must never shape core delivery. `telegram` was phase 1;
# `mattermost` is the phase-2 gap 1 of the audit (B1/B2 used to slip through
# because the banned name was only `telegram`). Case-insensitive on purpose:
# the check below is a substring check, so `cli` is NOT a token here (it would
# match `client`); name comparisons on `cli` are caught by R3 instead.
PLATFORM_TOKEN_RE = re.compile(r"telegram|mattermost", re.IGNORECASE)
GET_PLUGIN = "plugins_yaml::get_plugin"
ALLOWED_GET_PLUGIN_FILE = "src/agent/executor.rs"  # LLM-provider api-key fallback
WINDOW_CHARS = 800
DELIVERY_ROOTS = ("src/agent/", "src/platform/")

# ---------------------------------------------------------------------------
# Phase 2 (V-12) constants
# ---------------------------------------------------------------------------
PLATFORM_NAMES = frozenset({
    "telegram", "mattermost", "cli", "slack", "signal", "discord", "whatsapp",
    "irc", "matrix", "rocketchat", "teams", "xmpp", "zulip", "wechat", "viber",
    "line", "imessage", "email", "sms", "webchat", "voice",
})

PROVIDER_NAMES = frozenset({
    "openai", "anthropic", "deepseek", "gemini", "google", "mistral", "groq",
    "openrouter", "ollama", "azure", "bedrock", "xai", "qwen", "moonshot",
    "cohere", "jina", "glm", "kimi", "zhipu", "together", "fireworks",
    "perplexity", "opencode-go",
})

# First-party tool ids named by the audit (C1-C5). Extend as the toolset grows;
# a tool id that is not listed here is simply not guarded yet.
TOOL_NAMES = frozenset({
    "docker__compose",
    "filesystem__read", "filesystem__list", "filesystem__search",
    "filesystem__info", "filesystem__grep",
    "search__messages", "search__wiki", "search__database",
    "search__channel_prompts", "search__thread_messages",
    "note_read", "notes__note_read", "notes__note_list", "notes_note-write",
    "memory__list_memories", "memory_manage-memory",
    "skills__list_skills", "skills__view_skill",
    "manage_subtasks", "subtasks__manage_subtasks", "subtasks__list_subtasks",
    "subtasks__add_subtask", "prompt_generate", "prompt_compact-messages",
    "prompt__generate", "prompt__compact_messages",
    "git__status", "git__run_command",
})

PLATFORM_LHS = r"\b(?:plugin_name|platform|platform_name|channel_platform|plat)\b"
PROVIDER_LHS = r"\b(?:provider|provider_id|provider_name|provider\.0|pid)\b"
_LIT = r'"(?P<lit>[A-Za-z0-9_.\-]+)"'

PLATFORM_CMP_RE = re.compile(PLATFORM_LHS + r"[^=<>!\n]{0,40}(?:==|!=)\s*" + _LIT)
PLATFORM_CMP_RE_REV = re.compile(_LIT + r"\s*(?:==|!=)[^=<>!\n]{0,40}" + PLATFORM_LHS)
PLATFORM_ARM_RE = re.compile(r"^\s*" + _LIT + r"\s*(?:\||=>)")

PROVIDER_CMP_RE = re.compile(PROVIDER_LHS + r"[^=<>!\n]{0,40}(?:==|!=)\s*" + _LIT)
PROVIDER_CMP_RE_REV = re.compile(_LIT + r"\s*(?:==|!=)[^=<>!\n]{0,40}" + PROVIDER_LHS)
PROVIDER_CATCHALL_RE = re.compile(r"_\s*=>\s*" + _LIT)
PROVIDER_ID_CATCHALL_RE = re.compile(
    r'_\s*=>\s*ProviderId::new\("(?P<lit>[A-Za-z0-9_.\-]+)"\)'
)

RUST_STRING_RE = re.compile(r'"(?P<lit>[^"\\\n]+)"')
PY_STRING_RE = re.compile(r"""(?P<q>["'])(?P<lit>[^"'\\\n]+)(?P=q)""")
# The desired parameterizable pattern (audit C7): a `*_tool` / `*_tool_name`
# settings key right before (or on) the line that names the tool id.
CONFIGURABLE_TOOL_KEY_RE = re.compile(r'"[A-Za-z0-9_]*_tool(?:_name)?"')

# Hardcoded service endpoints: loopback / wildcard / docker-internal host:port.
# Public FQDNs (api.openai.com, github.com, docs URLs) are intentionally not
# flagged: they are not deployment-specific.
ENDPOINT_RE = re.compile(
    r"https?://(?:localhost|127\.\d{1,3}\.\d{1,3}\.\d{1,3}|0\.0\.0\.0|\[::1\])"
    r"(?::\d+)?"
    r"|https?://[A-Za-z0-9_-]+:\d+"
)

# ---------------------------------------------------------------------------
# Documented allowlist (V-12: "start with per-case allowlist entries that are
# removed as each fix lands"). A violation is suppressed only when its rule AND
# a substring of its path match an entry here. Entries are reported on every
# run and a stale entry (file scanned, nothing suppressed) is warned about.
# ---------------------------------------------------------------------------
KNOWN_EXCEPTIONS = (
    {
        "rule": "R3",
        "path": "src/platform/mod.rs",
        "reason": (
            "core-BUILTIN transport capability table (audit V-4): the CLI "
            "transport has no plugin process, so core declares its own "
            "capabilities (quote_seq0) here - the core-side counterpart of a "
            "plugin initialize answer, not a delivery branch. The V-4 "
            "delivery branch (the old platform==cli comparison in "
            "src/agent/helpers.rs) is gone. Remove only if the CLI transport "
            "becomes a plugin"
        ),
    },
    {
        "rule": "R5",
        "path": "src/server/settings.rs",
        "reason": (
            "settings-metadata default for the configurable "
            "`prompt_generate_tool` key (audit C7 desired pattern): the "
            "literal documents the parameterizable default, it is not a name "
            "branch"
        ),
    },
    {
        "rule": "R5",
        "path": "src/server/mod.rs",
        "reason": (
            "last-resort default when NO global config is loaded "
            "(audit C5 residual): the configured `prompt_generate_tool` is "
            "honoured whenever a config exists. Remove once the server "
            "resolves the tool through the config helper"
        ),
    },
    {
        "rule": "R5",
        "path": "plugins/tools/prompt/src/compact.rs",
        "reason": (
            "legacy standalone fallback (audit V-2): used only when an older "
            "core passes no tool descriptors; the descriptor set decides "
            "whenever it is supplied. Remove together with legacy_read_type_tool"
        ),
    },
    {
        "rule": "R5",
        "path": "tools/prompt/server.py",
        "reason": (
            "Python twin of the same legacy fallback (audit B5/C3): "
            "the file lives in the omni-plugins checkout "
            "(tools/prompt/server.py) and is only scanned when present. "
            "Remove with the Rust legacy fallback"
        ),
    },
    {
        "rule": "R6",
        "path": "src/mcp/mod.rs",
        "reason": (
            "documented last-resort default of the core-API base URL "
            "(audit V-8): CORE_API_BASE_URL and HOST/PORT win when set. "
            "Remove if the fallback is dropped"
        ),
    },
    {
        "rule": "R6",
        "path": "src/mcp/external/client.rs",
        "reason": (
            "default external MCP server URL used when the plugin config "
            "carries none (the config value wins). Remove if the default "
            "becomes mandatory config"
        ),
    },
)

RULES_EVERY = ("R1", "R2", "R3", "R4", "R5", "R6")


class Violation:
    __slots__ = ("rule", "path", "line", "message")

    def __init__(self, rule, path, line, message):
        self.rule = rule
        self.path = pathlib.Path(path)
        self.line = line
        self.message = message

    def display(self):
        try:
            rel = self.path.relative_to(REPO_ROOT)
        except ValueError:
            rel = self.path
        return f"{rel}:{self.line}: {self.message}"

    def sort_key(self):
        return (str(self.path), self.line, self.rule)


# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------
def skipped_test_lines(text):
    """Line numbers (1-based) inside a `#[cfg(test)] mod ... { }` block.

    Test code is allowed to mention platform/provider/tool names freely
    (fixtures, mock handshakes), so the lint only inspects production lines.
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


def _strip_line_comment(line, marker):
    """Cut a line at `marker` unless it sits inside a string literal.

    A naive `line.split(marker)` truncates string literals containing the
    marker: `let u = "http://localhost:8080";` lost everything from `//` on,
    so R6 could never see the URL (found by the lint self-test fixtures).
    """
    out = []
    quote = None
    escaped = False
    i = 0
    while i < len(line):
        ch = line[i]
        if quote is not None:
            out.append(ch)
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == quote:
                quote = None
        elif ch in ("\"", "'"):
            quote = ch
            out.append(ch)
        elif line.startswith(marker, i):
            break
        else:
            out.append(ch)
        i += 1
    return "".join(out)


def code_part(raw, suffix):
    """Return the code portion of a line (comment stripped, strings kept)."""
    marker = "#" if suffix == ".py" else "//"
    return _strip_line_comment(raw, marker).strip()


def production_lines(path):
    """Yield (line_number, code) for every production code line of `path`."""
    text = path.read_text(encoding="utf-8", errors="replace")
    skipped = skipped_test_lines(text) if path.suffix == ".rs" else set()
    for i, raw in enumerate(text.splitlines(), 1):
        if i in skipped:
            continue
        code = code_part(raw, path.suffix)
        if code:
            yield i, code


def _rel_str(path):
    try:
        return str(path.relative_to(REPO_ROOT))
    except ValueError:
        return str(path)


def _string_literals(code, suffix):
    rx = PY_STRING_RE if suffix == ".py" else RUST_STRING_RE
    return [m.group("lit") for m in rx.finditer(code)]


def allowlist_entry(rule, path):
    text = str(path)
    for entry in KNOWN_EXCEPTIONS:
        if entry["rule"] == rule and entry["path"] in text:
            return entry
    return None


# ---------------------------------------------------------------------------
# R1 + R2 - platform-generic delivery path
# ---------------------------------------------------------------------------
def scan_delivery_path(path, violations):
    text = path.read_text(encoding="utf-8", errors="replace")
    lines = text.splitlines()
    skipped = skipped_test_lines(text)
    for i, raw in enumerate(lines, 1):
        if i in skipped:
            continue
        code = code_part(raw, ".rs")
        if not code:
            continue
        low = code.lower()
        for token in BANNED_DELIVERY_TOKENS:
            if token in low:
                violations.append(Violation(
                    "R1", path, i,
                    f"banned platform-specific delivery token '{token}' in "
                    f"delivery code (R1: core delivery must be platform-generic; "
                    f"collapse/suppression lives in the platform plugin, see "
                    f"code-plan C6 / threads 518-519)",
                ))
        if PLATFORM_TOKEN_RE.search(code):
            violations.append(Violation(
                "R1", path, i,
                "platform name reference in core delivery code (R1: core never "
                "references a platform by name to shape delivery)",
            ))

    # R2: config reads in the delivery path (test regions excluded).
    for i, raw in enumerate(lines, 1):
        if i in skipped:
            continue
        if GET_PLUGIN not in raw:
            continue
        window = "\n".join(lines[i - 1:i + 12])[:WINDOW_CHARS]
        if "PluginYamlType::Platform" in window:
            violations.append(Violation(
                "R2", path, i,
                "plugins_yaml::get_plugin resolves a PLATFORM plugin config "
                "inside the core delivery path (R2: the A4 leak shape - core "
                "must not read platform config to shape delivery)",
            ))
        elif not (
            str(path).endswith(ALLOWED_GET_PLUGIN_FILE)
            and "PluginYamlType::Provider" in window
        ):
            violations.append(Violation(
                "R2", path, i,
                "plugins_yaml::get_plugin config read in the core delivery path "
                f"(R2: only the documented LLM-provider api-key fallback in "
                f"{ALLOWED_GET_PLUGIN_FILE} is allowed)",
            ))


# ---------------------------------------------------------------------------
# R3 - no platform-name comparison / platform-keyed match arm in core
# ---------------------------------------------------------------------------
def scan_platform_names(path, violations):
    for ln, code in production_lines(path):
        hit = None
        for rx in (PLATFORM_CMP_RE, PLATFORM_CMP_RE_REV):
            m = rx.search(code)
            if m and m.group("lit").lower() in PLATFORM_NAMES:
                hit = (f"platform-name comparison with '{m.group('lit')}'",
                       "R3: core branches on the capabilities a platform plugin "
                       "declares, never on its name (audit B1/B2/B3)")
                break
        if hit is None:
            m = PLATFORM_ARM_RE.match(code)
            if m and m.group("lit").lower() in PLATFORM_NAMES:
                hit = (f"match arm keyed on platform name '{m.group('lit')}'",
                       "R3: platform-specific text/behavior belongs to the "
                       "platform plugin, not to a core (or tool-plugin) match "
                       "table (audit B4/B5)")
        if hit:
            violations.append(Violation("R3", path, ln, f"{hit[0]} ({hit[1]})"))


# ---------------------------------------------------------------------------
# R4 - no provider-name comparison / named-provider catch-all
# ---------------------------------------------------------------------------
def scan_provider_names(path, violations):
    for ln, code in production_lines(path):
        m = PROVIDER_CMP_RE.search(code) or PROVIDER_CMP_RE_REV.search(code)
        if m and m.group("lit").lower() in PROVIDER_NAMES:
            violations.append(Violation(
                "R4", path, ln,
                f"provider-name comparison with '{m.group('lit')}' (R4: the "
                f"provider's declared capability decides auth/request shape, "
                f"not its name - audit A1)",
            ))
        for rx in (PROVIDER_CATCHALL_RE, PROVIDER_ID_CATCHALL_RE):
            m = rx.search(code)
            if m and m.group("lit").lower() in PROVIDER_NAMES:
                violations.append(Violation(
                    "R4", path, ln,
                    f"catch-all arm landing on provider '{m.group('lit')}' "
                    f"(R4: an unknown protocol must fail loudly, never fall "
                    f"back to a named provider - audit A3 / V-9)",
                ))


# ---------------------------------------------------------------------------
# R5 - no hardcoded first-party tool-name literal
# ---------------------------------------------------------------------------
def scan_tool_name_literals(path, violations, own_tool_ids=frozenset(),
                            own_prefixes=frozenset()):
    entries = list(production_lines(path))
    for idx, (ln, code) in enumerate(entries):
        window = " ".join(c for _, c in entries[max(0, idx - 2):idx + 1])
        if CONFIGURABLE_TOOL_KEY_RE.search(window):
            # Documented parameterizable pattern: a tool name used as the
            # default of a `*_tool` / `*_tool_name` settings key (audit C7).
            continue
        for lit in dict.fromkeys(_string_literals(code, path.suffix)):
            if lit not in TOOL_NAMES:
                continue
            if _is_own_tool(lit, own_tool_ids, own_prefixes):
                continue
            violations.append(Violation(
                "R5", path, ln,
                f"hardcoded tool-name literal '{lit}' (R5: tool behavior comes "
                f"from the plugin's tool descriptors, audit V-2 / C1-C5)",
            ))


def _is_own_tool(lit, own_tool_ids, own_prefixes):
    if lit in own_tool_ids:
        return True
    for prefix in own_prefixes:
        if lit == prefix or lit.startswith(prefix + "_") or lit.startswith(prefix + "-"):
            return True
    return False


# ---------------------------------------------------------------------------
# R6 - no hardcoded service endpoint
# ---------------------------------------------------------------------------
def scan_endpoints(path, violations):
    for ln, code in production_lines(path):
        for m in ENDPOINT_RE.finditer(code):
            violations.append(Violation(
                "R6", path, ln,
                f"hardcoded service endpoint '{m.group(0)}' (R6: resolve the "
                f"endpoint from settings/env instead of a built-in host - "
                f"audit D1/D2)",
            ))


# ---------------------------------------------------------------------------
# Rule dispatch, plugin roots, allowlist
# ---------------------------------------------------------------------------
def rules_for(path, plugin_mode=None):
    if plugin_mode:
        return ("R3", "R4", "R5")
    rel = _rel_str(path)
    if rel.startswith("plugins/tools/"):
        return ("R3", "R4", "R5")
    if rel.startswith("src/"):
        rules = ["R3", "R6"]
        if rel.startswith(("src/llm/", "src/vectorizer/")):
            rules.append("R4")
        if rel.startswith(("src/agent/", "src/server/", "src/mcp/")):
            rules.append("R5")
        if rel.startswith(DELIVERY_ROOTS):
            rules.extend(("R1", "R2"))
        return tuple(rules)
    # Unknown tree (e.g. the lint's own fixtures): exercise every rule.
    return RULES_EVERY


def scan_file(path, rules, violations, own_tool_ids=frozenset(),
              own_prefixes=frozenset()):
    if "R1" in rules or "R2" in rules:
        scan_delivery_path(path, violations)
    if "R3" in rules:
        scan_platform_names(path, violations)
    if "R4" in rules:
        scan_provider_names(path, violations)
    if "R5" in rules:
        scan_tool_name_literals(path, violations, own_tool_ids, own_prefixes)
    if "R6" in rules:
        scan_endpoints(path, violations)


def collect_sources(paths, suffixes=(".rs", ".py")):
    files = []
    for raw in paths:
        p = pathlib.Path(raw)
        if p.is_dir():
            for child in sorted(p.rglob("*")):
                if child.is_file() and child.suffix in suffixes:
                    files.append(child)
        elif p.is_file() and p.suffix in suffixes:
            files.append(p)
    return files


def plugin_roots(repo_root):
    """Directories whose children are tool plugins, first-party first."""
    roots = []
    first_party = repo_root / "plugins" / "tools"
    if first_party.is_dir():
        roots.append(first_party)
    candidates = []
    env_dir = os.environ.get("OMNI_PLUGINS_DIR")
    if env_dir:
        candidates.append(pathlib.Path(env_dir))
    candidates.append(repo_root.parent / "omni-plugins")
    for cand in candidates:
        tools = cand / "tools"
        if tools.is_dir():
            if tools not in roots:
                roots.append(tools)
            break
    return roots


def plugin_dir_for(path, root):
    """Directory of the plugin owning `path` (nearest plugin.json ancestor)."""
    p = path.parent
    while True:
        if (p / "plugin.json").is_file():
            return p
        if p == root or p == p.parent:
            break
        p = p.parent
    try:
        rel = path.relative_to(root)
        if rel.parts:
            return root / rel.parts[0]
    except ValueError:
        pass
    return root


def plugin_own_tool_ids(plugin_dir):
    """Tool ids a plugin declares (audit C8: names originate in the plugin)."""
    ids = set()
    prefixes = {plugin_dir.name}
    manifest = plugin_dir / "plugin.json"
    if manifest.is_file():
        try:
            data = json.loads(manifest.read_text(encoding="utf-8"))
        except (ValueError, OSError):
            data = {}
        if isinstance(data, dict):
            for key in ("name", "id"):
                val = data.get(key)
                if isinstance(val, str) and val:
                    prefixes.add(val)
            tools = data.get("tools")
            if isinstance(tools, list):
                for tool in tools:
                    if isinstance(tool, dict) and isinstance(tool.get("name"), str):
                        ids.add(tool["name"])
    return frozenset(ids), frozenset(prefixes)


def apply_allowlist(violations):
    kept = []
    used = set()
    for v in violations:
        idx = None
        for i, entry in enumerate(KNOWN_EXCEPTIONS):
            if entry["rule"] == v.rule and entry["path"] in str(v.path):
                idx = i
                break
        if idx is None:
            kept.append(v)
        else:
            used.add(idx)
    return kept, used


def lint_repo(repo_root, scan_dirs=None):
    """Scan the default targets. Returns (violations, used, scanned_paths)."""
    violations = []
    used = set()
    scanned = []

    if scan_dirs:
        files = collect_sources(scan_dirs)
        for f in files:
            scan_file(f, rules_for(f), violations)
            scanned.append(str(f))
        return violations, used, scanned

    def scan_batch(files, plugin_mode=False):
        for f in files:
            own_ids, own_prefixes = frozenset(), frozenset()
            if plugin_mode:
                root = _plugin_root_for(f, repo_root)
                if root is not None:
                    own_ids, own_prefixes = plugin_own_tool_ids(
                        plugin_dir_for(f, root))
            scan_file(f, rules_for(f, plugin_mode=plugin_mode or None),
                      violations, own_ids, own_prefixes)
            scanned.append(str(f))

    scan_batch(collect_sources([repo_root / "src"]))
    for root in plugin_roots(repo_root):
        scan_batch(collect_sources([root]), plugin_mode=True)

    kept, used = apply_allowlist(violations)
    return kept, used, scanned


def _plugin_root_for(path, repo_root):
    for root in plugin_roots(repo_root):
        try:
            path.relative_to(root)
            return root
        except ValueError:
            continue
    return None


def main(argv):
    repo_root = REPO_ROOT
    if argv:
        targets = [
            pathlib.Path(a) if pathlib.Path(a).is_absolute() else repo_root / a
            for a in argv
        ]
    else:
        targets = None

    violations, used, scanned = lint_repo(repo_root, scan_dirs=targets)
    files = len(scanned)

    if violations:
        violations.sort(key=lambda v: v.sort_key())
        print(f"core-platform boundary lint FAILED "
              f"({len(violations)} violation(s)):")
        for v in violations:
            print(f"  - {v.display()}")
        print(
            "Rule: core code must never reference a platform/provider/tool by "
            "name or read platform plugin config to shape delivery. See "
            "AGENTS.md 'Core-Platform Boundary Rule', code-plan C6 and audit "
            "items V-2/V-5/V-9/V-12."
        )
        return 1

    print(f"core-platform boundary lint OK ({files} file(s) scanned)")
    if used:
        print(f"  {len(used)} allowlisted known exception(s):")
        for idx in sorted(used):
            entry = KNOWN_EXCEPTIONS[idx]
            print(f"    - {entry['rule']} {entry['path']}: {entry['reason']}")
    for idx, entry in enumerate(KNOWN_EXCEPTIONS):
        if idx in used:
            continue
        if any(entry["path"] in p for p in scanned):
            print(f"  WARNING: stale allowlist entry {entry['rule']} "
                  f"'{entry['path']}' suppressed nothing - remove it")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
