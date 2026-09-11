#!/usr/bin/env python3
"""Self-test for scripts/lint-core-platform-boundary.py (phase 2, audit V-12).

Every rule class gets planted fixtures that reproduce the documented baseline
violations of the implicit-hardcoded-dependencies audit (wiki
Reference/Omniagent/Hardcoded-Dependency-Audit.md, sections 4-6):

  R1  delivery tokens / platform names in core delivery   (threads 518/519)
  R2  platform-plugin config read in core delivery         (defect class A4)
  R3  platform-name comparison or match arm                (B1/B2 mattermost,
                                                            B3 cli, B4/B5)
  R4  provider-name comparison / provider catch-all        (A1 anthropic,
                                                            A3 fallback openai)
  R5  hardcoded tool-name literal                          (C1 docker_compose .. C5)
  R6  hardcoded service endpoint                           (D1 qdrant, D2 localhost:8080)

The fixtures prove the lint FAILS on the pre-fix shapes (a tree without the
fixes) and stays silent on the capability-driven / parameterizable shapes the
fixes (V-2 .. V-9) introduced; the last tests assert the real checkout is
clean again and that the documented allowlist is shaped correctly.

Run (from the repo root or anywhere):
    python3 scripts/test_lint_core_platform_boundary.py [-v]
Exit code 0 = every rule behaves as documented.
"""

import importlib.util
import json
import os
import pathlib
import tempfile
import unittest

_SCRIPT = pathlib.Path(__file__).resolve().parent / "lint-core-platform-boundary.py"
_spec = importlib.util.spec_from_file_location("core_platform_boundary_lint", _SCRIPT)
lint = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(lint)

_rust_id = iter(range(1, 10_000))


def _tmpdir():
    name = "lintfix%d" % next(_rust_id)
    return tempfile.TemporaryDirectory(prefix=name)


def scan_rules(name, text, rules):
    """Write a fixture named `name` and run only `rules` against it."""
    with _tmpdir() as d:
        path = pathlib.Path(d) / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        violations = []
        lint.scan_file(path, tuple(rules), violations)
        return violations


def scan_all(name, text):
    """Unknown tree -> the dispatcher would exercise every rule."""
    return scan_rules(name, text, lint.RULES_EVERY)


def rules_of(violations):
    return sorted({v.rule for v in violations})


class TestR1DeliveryTokens(unittest.TestCase):
    """Phase 1 (code-plan C6): core delivery must stay platform-generic."""

    def test_delivery_key_token_is_flagged(self):
        v = scan_rules("src/agent/helpers.rs", 'let mode = "first_last_only";\n', ("R1",))
        self.assertEqual(len(v), 1, v)
        self.assertIn("first_last_only", v[0].message)

    def test_telemetry_suppression_token_is_flagged(self):
        v = scan_rules("src/agent/helpers.rs",
                       'let skip = is_internal_telemetry;\n', ("R1",))
        self.assertEqual(len(v), 1, v)

    def test_platform_name_in_delivery_is_flagged(self):
        for name in ("telegram", "mattermost"):
            v = scan_rules("src/platform/external/client.rs",
                           'let p = "%s";\n' % name, ("R1",))
            self.assertEqual(len(v), 1, name)
            self.assertEqual(v[0].rule, "R1")

    def test_comment_mention_is_allowed(self):
        v = scan_rules("src/agent/helpers.rs",
                       "// telegram collapses the stream, mattermost does not\n"
                       "let x = 1;\n", ("R1",))
        self.assertEqual(v, [], v)


class TestR2PlatformConfigRead(unittest.TestCase):
    """Defect class A4: core must not read platform plugin config."""

    def _src(self, body):
        return ("async fn deliver(pm: &PluginManager) {\n"
                "    let cfg = plugins_yaml::get_plugin(pm, name%s).await;\n"
                "}\n" % body)

    def test_untyped_config_read_is_flagged(self):
        v = scan_rules("src/agent/foo.rs", self._src(""), ("R2",))
        self.assertEqual(len(v), 1, v)

    def test_platform_typed_config_read_is_flagged(self):
        v = scan_rules("src/agent/foo.rs",
                       self._src(", PluginYamlType::Platform"), ("R2",))
        self.assertEqual(len(v), 1, v)
        self.assertIn("PLATFORM", v[0].message)

    def test_provider_fallback_in_executor_is_allowed(self):
        v = scan_rules("src/agent/executor.rs",
                       self._src(", PluginYamlType::Provider"), ("R2",))
        self.assertEqual(v, [], v)

    def test_provider_read_elsewhere_is_flagged(self):
        v = scan_rules("src/agent/other.rs",
                       self._src(", PluginYamlType::Provider"), ("R2",))
        self.assertEqual(len(v), 1, v)


class TestR3PlatformNames(unittest.TestCase):
    """Gaps 1-2 of the audit: mattermost/cli comparisons and match arms."""

    def test_mattermost_comparison_B1B2(self):
        v = scan_rules("src/platform/external/client.rs",
                       'if plugin_name == "mattermost" {\n    return None;\n}\n',
                       ("R3",))
        self.assertEqual(len(v), 1, v)
        self.assertIn("mattermost", v[0].message)

    def test_cli_comparison_B3(self):
        for code in ('if platform == "cli" {', 'if "cli" == platform {',
                     'let mine = platform != "cli";'):
            v = scan_rules("src/agent/helpers.rs", code + "\n", ("R3",))
            self.assertEqual(len(v), 1, code)
            self.assertIn("cli", v[0].message)

    def test_platform_keyed_match_arm_B4B5(self):
        text = ('let hint = match platform {\n'
                '    "telegram" => "MarkdownV2",\n'
                '    "mattermost" => "GFM",\n'
                '    _ => "",\n'
                '};\n')
        v = scan_rules("plugins/tools/prompt/src/prompt_builder.rs", text, ("R3",))
        self.assertEqual(len(v), 2, v)
        self.assertTrue(all(x.rule == "R3" for x in v))

    def test_capability_driven_code_is_clean(self):
        text = ("let hint = platform_capabilities\n"
                "    .map(|c| c.prompt_hint.clone())\n"
                "    .unwrap_or_default();\n"
                "match protocol {\n"
                '    "json" => 1,\n'
                "    _ => 0,\n"
                "}\n")
        self.assertEqual(scan_rules("src/agent/foo.rs", text, ("R3",)), [])

    def test_test_region_is_skipped(self):
        text = ('fn real() {}\n'
                '#[cfg(test)]\n'
                'mod tests {\n'
                '    fn t() {\n'
                '        let caps = match name {\n'
                '            "cli" => 1,\n'
                '            _ => 0,\n'
                '        };\n'
                '    }\n'
                '}\n')
        self.assertEqual(scan_rules("src/platform/mod.rs", text, ("R3",)), [])


class TestR4ProviderNames(unittest.TestCase):
    """Audit A1 (provider-name comparison) and A3 (named catch-all)."""

    def test_provider_name_comparison_A1(self):
        for code in ('if provider.0 == "anthropic" {',
                     'let h = provider_name == "deepseek";'):
            v = scan_rules("src/llm/mod.rs", code + "\n", ("R4",))
            self.assertEqual(len(v), 1, code)
            self.assertEqual(v[0].rule, "R4")

    def test_provider_catchall_A3(self):
        for code in ('_ => "openai".to_string(),',
                     '_ => ProviderId::new("anthropic"),'):
            v = scan_rules("src/vectorizer/mod.rs", code + "\n", ("R4",))
            self.assertEqual(len(v), 1, code)

    def test_declared_protocol_arm_and_error_are_clean(self):
        text = ('match protocol {\n'
                '    "openai_compat" => Ok(ApiMode::ChatCompletions),\n'
                '    other => Err(anyhow!("unknown protocol {other}")),\n'
                '}\n'
                '_ => Err(AppError::Config("unset")),\n')
        self.assertEqual(scan_rules("src/llm/mod.rs", text, ("R4",)), [])


class TestR5ToolNameLiterals(unittest.TestCase):
    """Audit C1-C5: tool behaviour comes from plugin descriptors."""

    def test_tool_literals_in_core_are_flagged(self):
        for name, code in (
            ("docker_compose", 'guard("docker_compose");'),
            ("filesystem_read", 'if tc.function.name == "filesystem_read" {'),
            ("subtasks_manage-subtasks",
             'tc.function.name == "subtasks_manage-subtasks"'),
        ):
            v = scan_rules("src/agent/main_loop.rs", code + "\n", ("R5",))
            self.assertEqual(len(v), 1, name)
            self.assertIn(name, v[0].message)

    def test_python_twin_literals_are_flagged(self):
        v = scan_rules("tools/prompt/server.py",
                       'READ_TOOL_PREFIXES = ("filesystem_read", "search_wiki")\n',
                       ("R5",))
        self.assertEqual(len(v), 2, v)

    def test_configurable_tool_key_is_allowed_C7(self):
        text = ('let t = get("prompt_generate_tool", "prompt_generate");\n'
                'let c = get("prompt_compact_messages_tool",\n'
                '            "prompt_compact-messages");\n'
                'let r = get("redaction_tool", "");\n')
        self.assertEqual(scan_rules("src/agent/config.rs", text, ("R5",)), [])

    def test_unknown_tool_name_is_not_guarded(self):
        v = scan_rules("src/agent/foo.rs", 'let x = "compose_v2";\n', ("R5",))
        self.assertEqual(v, [], v)

    def test_test_region_is_skipped(self):
        text = ('fn real() {}\n'
                '#[cfg(test)]\n'
                'mod tests {\n'
                '    fn t() { let n = "docker_compose"; }\n'
                '}\n')
        self.assertEqual(scan_rules("src/agent/main_loop.rs", text, ("R5",)), [])


class TestR6Endpoints(unittest.TestCase):
    """Audit D1/D2: loopback / wildcard / docker-internal endpoints."""

    def test_loopback_and_docker_endpoints_are_flagged(self):
        for url in ("http://localhost:8080", "http://127.0.0.1:9999/kanban",
                    "http://0.0.0.0:3000", "http://qdrant:6333"):
            v = scan_rules("src/mcp/mod.rs", 'let u = "%s";\n' % url, ("R6",))
            self.assertEqual(len(v), 1, url)

    def test_public_fqdn_urls_are_clean(self):
        text = ('let a = "https://api.openai.com/v1/chat/completions";\n'
                'let b = "https://github.com/nexuslbs/omniagent";\n')
        self.assertEqual(scan_rules("src/llm/mod.rs", text, ("R6",)), [])


class TestDispatch(unittest.TestCase):
    """The rule set is scoped per tree (no needless noise, no blind spots)."""

    def rules(self, rel):
        return set(lint.rules_for(pathlib.Path(lint.REPO_ROOT) / rel))

    def test_core_delivery_path(self):
        r = self.rules("src/agent/helpers.rs")
        self.assertEqual(r, {"R1", "R2", "R3", "R5", "R6"})

    def test_platform_path(self):
        r = self.rules("src/platform/external/client.rs")
        self.assertIn("R3", r)
        self.assertIn("R1", r)

    def test_llm_and_vectorizer_scan_providers(self):
        self.assertIn("R4", self.rules("src/llm/mod.rs"))
        self.assertIn("R4", self.rules("src/vectorizer/mod.rs"))
        self.assertNotIn("R4", self.rules("src/server/mod.rs"))

    def test_plugin_trees_scan_platform_provider_tools(self):
        r = self.rules("plugins/tools/prompt/src/compact.rs")
        self.assertEqual(r, {"R3", "R4", "R5"})


class TestPluginOwnToolIds(unittest.TestCase):
    """Audit C8: tool ids originate in the plugin manifest."""

    def test_own_ids_allowed_foreign_id_flagged(self):
        with _tmpdir() as d:
            root = pathlib.Path(d) / "repo"
            fs = root / "plugins" / "tools" / "filesystem"
            (fs / "src").mkdir(parents=True)
            (fs / "plugin.json").write_text(json.dumps({
                "name": "filesystem",
                "tools": [{"name": "filesystem_read"}],
            }), encoding="utf-8")
            (fs / "src" / "main.rs").write_text(
                'let a = "filesystem_read";\nlet b = "filesystem_list";\n',
                encoding="utf-8")
            notes = root / "plugins" / "tools" / "notes"
            (notes / "src").mkdir(parents=True)
            (notes / "src" / "main.rs").write_text(
                'let c = "docker_compose";\n', encoding="utf-8")

            old = os.environ.pop("OMNI_PLUGINS_DIR", None)
            try:
                violations, _used, _scanned = lint.lint_repo(root)
            finally:
                if old is not None:
                    os.environ["OMNI_PLUGINS_DIR"] = old

            msgs = [v.display() for v in violations]
            self.assertEqual(len(msgs), 1, msgs)
            self.assertIn("docker_compose", msgs[0])
            self.assertIn("notes", msgs[0])


class TestAllowlist(unittest.TestCase):
    """The documented per-case allowlist never hides a violation silently."""

    def test_matching_entry_suppresses_and_is_recorded(self):
        v = [lint.Violation("R5", "/repo/plugins/tools/prompt/src/compact.rs",
                            65, "legacy fallback")]
        kept, used = lint.apply_allowlist(v)
        self.assertEqual(kept, [])
        self.assertEqual(len(used), 1)

    def test_non_matching_violation_is_kept(self):
        v = [lint.Violation("R5", "/repo/src/agent/main_loop.rs", 12, "new leak")]
        kept, used = lint.apply_allowlist(v)
        self.assertEqual(len(kept), 1)
        self.assertEqual(used, set())

    def test_entries_are_well_formed(self):
        for entry in lint.KNOWN_EXCEPTIONS:
            self.assertIn(entry["rule"], lint.RULES_EVERY, entry)
            self.assertTrue(entry["path"], entry)
            self.assertGreater(len(entry["reason"]), 20, entry)

    def test_every_rule_has_a_documented_exception_or_none_needed(self):
        # R3's only structural exception is the core-builtin transport table.
        r3 = [e for e in lint.KNOWN_EXCEPTIONS if e["rule"] == "R3"]
        self.assertEqual([e["path"] for e in r3], ["src/platform/mod.rs"])


class TestRealCheckout(unittest.TestCase):
    """The lint must PASS on the fixed checkout (verification of audit V-12)."""

    def test_real_repo_is_clean(self):
        if not (pathlib.Path(lint.REPO_ROOT) / "src").is_dir():
            self.skipTest("not run from an omniagent checkout")
        violations, _used, scanned = lint.lint_repo(lint.REPO_ROOT)
        self.assertGreater(len(scanned), 50)
        self.assertEqual([v.display() for v in violations], [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
