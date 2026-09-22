#!/usr/bin/env python3
"""Self-test for scripts/lint-no-unrequested-narrowing.py (defect class A5).

Planted fixtures MUST trip the lint (the historical hidden cap instance and its
variants); fixed/justified code MUST stay clean. `--self-test-fixture` tells the
lint to scan a fixture root as if it were the repo (used by CI to prove the gate
is live, and by the reviewer to reproduce the firing evidence).
"""

from __future__ import annotations

import io
import json
import os
import shutil
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import importlib.util

_spec = importlib.util.spec_from_file_location(
    'narrowing_lint',
    os.path.join(os.path.dirname(os.path.abspath(__file__)),
                 'lint-no-unrequested-narrowing.py'))
narrowing_lint = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(narrowing_lint)


CONFIG_STUB = '''\
impl AgentConfig {
    pub fn from_env() -> u32 {
        let get = |key: &str, default: &str| -> String {
            settings.get(key).cloned().unwrap_or_else(|| default.to_string())
        };
        let _max_iterations_no_plan: u32 = get("max_iterations_no_plan", "30").parse().unwrap_or(30);
        let _max_iterations_plan: u32 = get("max_iterations_plan", "120").parse().unwrap_or(120);
        let _interactive_max_iterations: u32 = get("interactive_max_iterations", "12").parse().unwrap_or(12);
        0
    }
}
'''

# The historical instance, verbatim in shape: a self-invented cap applied with
# `min` to the operator-configured iteration budget.
PLANTED_BAD = CONFIG_STUB + '''
pub struct InteractiveBudget;

impl InteractiveBudget {
    pub fn resolve(
        base: u32,
        interactive_cap: u32,
    ) -> u32 {
        // Hidden behaviour: cap every ordinary operator chat at 12 rounds.
        base.min(interactive_cap)
    }
}

fn planted_direct(cfg: &AgentConfig) -> i32 {
    let iter_limit = queries::max_iterations_for_plan(&cfg.config_snapshot(), plan) as i32;
    let interactive_iter_limit = iter_limit.min(cfg.interactive_max_iterations as i32);
    interactive_iter_limit
}

fn planted_unwrap(cfg: &AgentConfig) -> u32 {
    cfg.max_iterations_no_plan.checked_div(1).unwrap_or(12)
}
'''

CLEAN_FIXED = CONFIG_STUB + '''
fn fixed(cfg: &AgentConfig) -> i32 {
    let iter_limit = queries::max_iterations_for_plan(&cfg.config_snapshot(), plan) as i32;
    iter_limit
}

fn fixed_definition_site(cfg: &AgentConfig) -> u32 {
    cfg.max_iterations_no_plan
}
'''

CLEAN_JUSTIFIED = CONFIG_STUB + '''
fn justified(cfg: &AgentConfig) -> i32 {
    let iter_limit = queries::max_iterations_for_plan(&cfg.config_snapshot(), plan) as i32;
    // narrowing-ok: operator asked for a hard 12-round safety net (task body quote X)
    iter_limit.min(12)
}
'''

# Arithmetic on an already-resolved budget is NOT a narrowing of the setting.
CLEAN_DERIVED = CONFIG_STUB + '''
fn derived(cfg: &AgentConfig) -> i32 {
    let iter_limit = queries::max_iterations_for_plan(&cfg.config_snapshot(), plan) as i32;
    let half = iter_limit / 2;
    let at = half.max(3).min(iter_limit - 1);
    at
}
'''


class LintNarrowingTest(unittest.TestCase):
    def _run(self, source: str, baseline: dict | None = None) -> tuple[int, str]:
        tmp = tempfile.mkdtemp(prefix='narrowing-lint-')
        try:
            agent_dir = os.path.join(tmp, 'src', 'agent')
            os.makedirs(agent_dir)
            with open(os.path.join(agent_dir, 'main_loop.rs'), 'w', encoding='utf-8') as fh:
                fh.write(source)
            with open(os.path.join(agent_dir, 'config.rs'), 'w', encoding='utf-8') as fh:
                fh.write(CONFIG_STUB)
            base_path = os.path.join(tmp, 'baseline.json')
            with open(base_path, 'w', encoding='utf-8') as fh:
                json.dump(baseline or {'entries': []}, fh)
            err = io.StringIO()
            with redirect_stdout(io.StringIO()), redirect_stderr(err):
                code = narrowing_lint.main(['--root', tmp, '--baseline', base_path])
            return code, err.getvalue()
        finally:
            shutil.rmtree(tmp, ignore_errors=True)

    def test_planted_hidden_cap_trips(self):
        """The historical `min(base, 12)` hidden cap must fail the lint."""
        code, err = self._run(PLANTED_BAD)
        self.assertEqual(code, 1, f'expected FAIL, got {code}: {err}')
        self.assertIn('min() narrows a setting-bound value', err)
        self.assertIn('unrequested narrowing default', err)

    def test_fixed_code_clean(self):
        code, err = self._run(CLEAN_FIXED)
        self.assertEqual(code, 0, f'expected OK, got {code}: {err}')

    def test_justified_inline_clean(self):
        code, err = self._run(CLEAN_JUSTIFIED)
        self.assertEqual(code, 0, f'expected OK, got {code}: {err}')

    def test_derived_arithmetic_clean(self):
        code, err = self._run(CLEAN_DERIVED)
        self.assertEqual(code, 0, f'expected OK, got {code}: {err}')

    def test_baseline_requires_justification(self):
        code, err = self._run(CLEAN_FIXED, baseline={
            'entries': [{'file': 'src/agent/main_loop.rs', 'code': 'iter_limit.min(12)'}]})
        self.assertEqual(code, 1)
        self.assertIn('has no justification', err)
        self.assertIn('has no operator_request', err)

    def test_baseline_justified_clean(self):
        code, err = self._run(PLANTED_BAD, baseline={'entries': [
            {'file': 'src/agent/main_loop.rs',
             'code': 'let interactive_iter_limit = iter_limit.min(cfg.interactive_max_iterations as i32);',
             'justification': 'operator-requested safety net',
             'operator_request': 'telegram msg #1234'},
            {'file': 'src/agent/main_loop.rs', 'code': 'base.min(interactive_cap)',
             'justification': 'operator-requested safety net',
             'operator_request': 'telegram msg #1234'},
            {'file': 'src/agent/main_loop.rs',
             'code': 'cfg.max_iterations_no_plan.checked_div(1).unwrap_or(12)',
             'justification': 'operator-requested safety net',
             'operator_request': 'telegram msg #1234'},
        ]})
        self.assertEqual(code, 0, f'expected OK, got {code}: {err}')


if __name__ == '__main__':
    unittest.main(verbosity=2)
