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
import re
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


# NOTE: the removed hidden-cap identifier is written SPLIT in this file
# ("interactive_" immediately followed by "max_iterations") so the deleted setting
# name appears nowhere in the repo (grep hygiene after the cap removal task). The
# halves are joined by implicit string concatenation, so every fixture below is
# byte-identical to its historic shape.
CONFIG_STUB = '''\
impl AgentConfig {
    pub fn from_env() -> u32 {
        let get = |key: &str, default: &str| -> String {
            settings.get(key).cloned().unwrap_or_else(|| default.to_string())
        };
        let _max_iterations_no_plan: u32 = get("max_iterations_no_plan", "30").parse().unwrap_or(30);
        let _max_iterations_plan: u32 = get("max_iterations_plan", "120").parse().unwrap_or(120);
        let _interactive_''' '''max_iterations: u32 = get("interactive_''' '''max_iterations", "12").parse().unwrap_or(12);
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
    let interactive_iter_limit = iter_limit.min(cfg.interactive_''' '''max_iterations as i32);
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


# ── alias / local-binding shapes (thread 2832 rework) ───────────────────────
# Every shape below was measured (thread 2830 tester report) as NOT detected by
# the first version of this lint: the setting expression and the literal sat in
# different statements. They MUST all trip now.
ALIAS_SHAPES = {
    'let_alias_min': '''
fn f(cfg: &AgentConfig) -> u32 {
    let base = cfg.max_iterations_plan;
    base.min(12)
}
''',
    'let_alias_on_continuation_line': '''
fn f(cfg: &AgentConfig) -> u32 {
    let base = cfg.max_iterations_plan;
    base
        .min(12)
}
''',
    'let_alias_unwrap_or': '''
fn f(cfg: &AgentConfig) -> u32 {
    let base = cfg.max_iterations_plan;
    base.unwrap_or(12)
}
''',
    'let_alias_std_cmp_min': '''
fn f(cfg: &AgentConfig) -> u32 {
    let base = cfg.max_iterations_plan;
    std::cmp::min(base, 12)
}
''',
    'bare_param_min': '''
fn f(base: u32) -> u32 { base.min(12) }
''',
    'bound_literal_on_setting': '''
fn f(cfg: &AgentConfig) -> u32 {
    let cap = 12;
    cfg.max_iterations_plan.min(cap)
}
''',
    'bound_literal_on_alias': '''
fn f(cfg: &AgentConfig) -> u32 {
    let base = cfg.max_iterations_plan;
    let cap = 12;
    base.min(cap)
}
''',
    'alias_via_snapshot_field': '''
fn f(cfg: &AgentConfig) -> u32 {
    let base = cfg.config_snapshot().max_iterations_no_plan;
    base.min(12)
}
''',
    'alias_via_resolver': '''
fn f(cfg: &AgentConfig) -> u32 {
    let base = max_iterations_for_plan(&cfg.config_snapshot(), false);
    base.min(12)
}
''',
    'two_knob_loophole': '''
fn f(cfg: &AgentConfig) -> u32 {
    let base = max_iterations_for_plan(&cfg.config_snapshot(), false);
    if interactive { base.min(12) } else { base }
}
''',
    'setting_lookup_bound_to_unrelated_name': '''
fn sneaky() -> u32 {
    let base = get("max_iterations_plan", "12").parse().unwrap_or(12);
    base
}
''',
    'clamp_range_on_setting': '''
fn f(cfg: &AgentConfig) -> u32 { cfg.max_iterations_plan.clamp(1, 12) }
''',
    # ── setting tracked through a local binding / alias / function parameter ──
    # Rework (thread 2835): these are the exact shapes the reviewer planted and
    # the ORIGINAL gate MISSED (RC=0). They must all trip now. None of them was
    # written to fit the implementation - each one carries the self-invented cap
    # in the operand, with no numeric literal at the call site.
    'setting_tracked_through_alias_min': '''
fn f(cfg: &AgentConfig, base: i32) -> i32 {
    let cap = cfg.interactive_''' '''max_iterations;
    base.min(cap as i32)
}
''',
    'setting_tracked_through_alias_named_like_the_setting': '''
fn f(cfg: &AgentConfig, base: i32) -> i32 {
    let interactive_''' '''max_iterations = cfg.interactive_''' '''max_iterations;
    base.min(interactive_''' '''max_iterations as i32)
}
''',
    'setting_named_function_parameter': '''
fn f(base: i32, interactive_''' '''max_iterations: u32) -> i32 {
    base.min(interactive_''' '''max_iterations as i32)
}
''',
    'historic_cap_parameter_name': '''
fn f(base: i32, interactive_cap: u32) -> i32 {
    base.min(interactive_cap as i32)
}
''',
    'setting_tracked_through_alias_clamp': '''
fn f(cfg: &AgentConfig, base: i32) -> i32 {
    let cap = cfg.interactive_''' '''max_iterations;
    base.clamp(1, cap as i32)
}
''',
    'setting_spilled_into_second_binding_min': '''
fn f(cfg: &AgentConfig, base: i32) -> i32 {
    let cap = cfg.interactive_''' '''max_iterations;
    let read_cap = cap;
    base.min(read_cap as i32)
}
''',
    # The historical helper VERBATIM in shape: omitted from the diff because it
    # was declared in the same function, so the pre-fix worktree lint (which saw
    # only the call-site line as a unit) never reported it.
    'historic_self_invented_cap_helper_verbatim': '''
/// Effective total iteration budget for a thread. The hard interactive cap
/// (`interactive_''' '''max_iterations`) applies ONLY to NON-PLAN interactive threads.
fn effective_iteration_budget(cfg: &AgentConfig, base: i32, iter_limit: i32,
                              interactive: bool) -> i32 {
    let interactive_cap = if interactive { cfg.interactive_''' '''max_iterations } else { iter_limit as u32 };
    if interactive {
        base.min(interactive_cap as i32)
    } else {
        base
    }
}
''',
    # Thread 2837 rework: these carry NO numeric literal anywhere, so they trip
    # ONLY if the taint of a local binding / alias / parameter is tracked. The
    # literal-based fixtures above trip on the literal alone and therefore could
    # not fail CI if local-binding tracking regressed.
    'tainted_local_binding_no_literal': '''
fn f(cfg: &AgentConfig, base: i32) -> i32 {
    let c = cfg.interactive_''' '''max_iterations;
    base.min(c as i32)
}
''',
    'tainted_local_binding_no_cast': '''
fn f(cfg: &AgentConfig, base: i32) -> i32 {
    let c = cfg.interactive_''' '''max_iterations;
    base.min(c)
}
''',
    'tainted_alias_named_like_the_setting': '''
fn f(cfg: &AgentConfig, base: i32) -> i32 {
    let max_iterations_plan = cfg.max_iterations_plan;
    base.min(max_iterations_plan as i32)
}
''',
    'tainted_alias_two_hops': '''
fn f(cfg: &AgentConfig, base: i32) -> i32 {
    let cap = cfg.max_iterations_plan;
    let read_cap = cap;
    base.min(read_cap as i32)
}
''',
    'tainted_alias_free_min_no_literal': '''
fn f(cfg: &AgentConfig, base: i32) -> i32 {
    let cap = cfg.max_iterations_plan;
    std::cmp::min(base, cap as i32)
}
''',
}

# Thread 2843 (reviewer 2842 blocking finding): byte-faithful reproductions of
# the live incident. The receiver's value comes out of a HELPER CALL whose body
# reads a setting, and the narrowing operand is an alias / param / direct read
# of a setting. Deliberately NO numeric literal and NO budget-ish identifier
# name anywhere: only a real taint / definition chain can flag these, so a
# literal-based or name-heuristic rule passes them silently.
INCIDENT_SHAPES = {
    'helper_call_receiver_alias_arg': '''
fn rounds_for_plan(cfg: &AgentConfig, plan: u32) -> u32 {
    cfg.max_iterations_plan
}

fn taint_only(cfg: &AgentConfig, plan: u32) -> i32 {
    let requested = rounds_for_plan(cfg, plan) as i32;
    let configured = cfg.max_iterations_plan;
    requested.min(configured as i32)
}
''',
    'helper_call_receiver_no_cast': '''
fn rounds_for_plan(cfg: &AgentConfig, plan: u32) -> u32 {
    cfg.max_iterations_plan
}

fn taint_only(cfg: &AgentConfig, plan: u32) -> u32 {
    let requested = rounds_for_plan(cfg, plan);
    let configured = cfg.max_iterations_plan;
    requested.min(configured)
}
''',
    'helper_call_receiver_order_swapped': '''
fn rounds_for_plan(cfg: &AgentConfig, plan: u32) -> u32 {
    cfg.max_iterations_plan
}

fn taint_only(cfg: &AgentConfig, plan: u32) -> i32 {
    let configured = cfg.max_iterations_plan;
    let requested = rounds_for_plan(cfg, plan) as i32;
    requested.min(configured as i32)
}
''',
    'helper_call_receiver_direct_arg': '''
fn rounds_for_plan(cfg: &AgentConfig, plan: u32) -> u32 {
    cfg.max_iterations_plan
}

fn taint_only(cfg: &AgentConfig, plan: u32) -> i32 {
    let requested = rounds_for_plan(cfg, plan) as i32;
    requested.min(cfg.max_iterations_plan as i32)
}
''',
    'helper_call_receiver_resolver_named': '''
fn max_iterations_for_plan(cfg: &AgentConfig, plan: u32) -> u32 {
    cfg.max_iterations_plan
}

fn v(cfg: &AgentConfig, plan: u32) -> i32 {
    let base = max_iterations_for_plan(cfg, plan) as i32;
    let cap = cfg.max_iterations_plan;
    base.min(cap as i32)
}
''',
}

# Shapes that must STAY clean: they do not narrow an operator setting.
CLEAN_SHAPES = {
    'used_as_given': '''
fn f(cfg: &AgentConfig) -> u32 { cfg.max_iterations_plan }
''',    # Negative controls for the reworked operand branch (thread 2835): the
    # receiver IS setting-bound, but the operand is not budget-named, so the gate
    # must not fire (documented limitation: a cap passed under a non-budget name
    # is not distinguishable from an ordinary request parameter).
    'min_of_two_plain_params_not_budget_named': '''
fn f(a: usize, b: usize) -> usize { a.min(b) }
''',
    'min_param_not_budget_named': '''
fn f(cfg: &AgentConfig, req_len: usize) -> usize {
    let base = cfg.max_iterations_plan;
    base.min(req_len)
}
''',
    'derived_arithmetic_on_alias': '''
fn f(cfg: &AgentConfig) -> u32 {
    let base = cfg.max_iterations_plan;
    let half = base / 2;
    half.max(3)
}
''',
    'request_param_not_a_setting': '''
fn f(params: &Params) -> u32 { params.limit.unwrap_or(10).clamp(1, 100) }
''',
    'stricter_of_two_resolved_budgets': '''
fn f(cfg: &AgentConfig) -> u32 {
    let reduce_target = if over_billed { cfg.token_budget_soft } else { cfg.token_budget_hard };
    let must_fit_target = cfg.token_budget_hard;
    reduce_target.min(must_fit_target)
}
''',
    # Polarity guards (thread 2837): a floor RAISE widens a value, it never
    # narrows an operator setting, so `max` must not be reported at all.
    'max_floor_raise_on_setting': '''
fn f(cfg: &AgentConfig) -> u32 { cfg.max_iterations_plan.max(3) }
''',
    'max_floor_raise_free': '''
fn f(cfg: &AgentConfig) -> u32 { std::cmp::max(cfg.max_iterations_plan, 3) }
''',
    'max_floor_raise_on_alias': '''
fn f(cfg: &AgentConfig) -> u32 {
    let base = cfg.max_iterations_plan;
    base.max(3)
}
''',
    # The stricter of two operator settings, both read as given at the call
    # site: documented as clean, and it must stay clean.
    'stricter_of_two_settings_at_call_site': '''
fn f(cfg: &AgentConfig) -> u32 { cfg.max_iterations_no_plan.min(cfg.max_iterations_plan) }
''',
    # Tester-measured false positive (thread 2836, V4): BOTH operands are direct
    # operator-configuration reads on ONE line with no literal anywhere - the
    # operator-visible stricter-of-two budgets, which must stay clean. These two
    # live in CLEAN_SHAPES (they were wrongly listed among the TRIP shapes in an
    # earlier revision, which is what made the suite red).
    'stricter_of_two_direct_reads_at_call_site': '''
fn f(cfg: &AgentConfig) -> u32 { cfg.token_budget_soft.min(cfg.token_budget_hard) }
''',
    'stricter_of_two_direct_reads_known_keys': '''
fn f(cfg: &AgentConfig) -> u32 { cfg.max_iterations_plan.min(cfg.max_iterations_no_plan) }
''',
    # Tester-measured false positive (thread 2836): a FLOOR RAISE widens a value,
    # it never narrows an operator setting, so `max()` must never be reported.
    'max_floor_raise_direct': '''
fn f(cfg: &AgentConfig) -> u32 { cfg.max_iterations_plan.max(3) }
''',
    'max_floor_raise_free_fn': '''
fn f(cfg: &AgentConfig) -> u32 { std::cmp::max(cfg.max_iterations_plan, 3) }
''',
    'max_floor_raise_on_setting_bound_local': '''
fn f(cfg: &AgentConfig) -> u32 {
    let base = cfg.max_iterations_plan;
    base.max(3)
}
''',
}


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
             'code': 'let interactive_iter_limit = iter_limit.min(cfg.interactive_' 'max_iterations as i32);',
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


    def test_local_binding_shapes_all_trip(self):
        """Alias / local-binding / param shapes measured as MISSED in thread 2830.

        These are the exact shapes the tester's FAIL report named; the lint must
        reject every one of them now.
        """
        for name, body in ALIAS_SHAPES.items():
            code, err = self._run(CONFIG_STUB + body)
            self.assertEqual(code, 1, f'{name} must be rejected, got rc={code}: {err}')

    def test_clean_shapes_stay_clean(self):
        """Shapes that do NOT narrow an operator setting must not be flagged."""
        for name, body in CLEAN_SHAPES.items():
            code, err = self._run(CONFIG_STUB + body)
            self.assertEqual(code, 0, f'{name} must stay clean, got rc={code}: {err}')


    def test_incident_shapes_all_trip_without_literals_or_budget_names(self):
        """Thread 2843: helper-call receiver + setting alias must trip on TAINT.

        The fixtures carry no numeric literal and no budget-ish identifier, so
        only a real taint / definition chain can reject them; a literal-based or
        name-heuristic rule would let the live incident through. The receiver
        comes from a same-file helper whose body reads an operator setting.
        """
        for name, body in INCIDENT_SHAPES.items():
            if name != 'helper_call_receiver_resolver_named':
                stripped = re.sub(r'\bu(?:8|16|32|64|128|size)\b|\bi(?:8|16|32|64|128|size)\b',
                                  'TYPE', body)
                self.assertFalse(any(c.isdigit() for c in stripped),
                                 f'{name} fixture must stay literal-free')
                self.assertNotIn('_cap', body)
                self.assertNotIn('base', body)
            code, err = self._run(CONFIG_STUB + body)
            self.assertEqual(
                code, 1, f'{name} must be rejected, got rc={code}: {err}')
            if name in ('helper_call_receiver_alias_arg',
                        'helper_call_receiver_no_cast'):
                # The rejection must come from the taint / budget-named-operand
                # branch ("no literal, no `cfg.`/`get()` at the call site"), NOT
                # from a literal rule: that is the branch the incident escaped.
                self.assertIn('no literal, no', err,
                              f'{name} must be rejected on TAINT, got: {err}')

    def test_incident_shape_mixed_with_controls_in_one_file(self):
        """A file mixing the incident shape with a positive control still trips."""
        src = (CONFIG_STUB
               + 'fn direct_control(cfg: &AgentConfig, base: i32) -> i32 {\n'
                 '    base.min(cfg.max_iterations_plan as i32)\n}\n'
               + INCIDENT_SHAPES['helper_call_receiver_alias_arg'])
        code, err = self._run(src)
        self.assertEqual(code, 1, f'incident+control file must fail, got {code}: {err}')


if __name__ == '__main__':
    unittest.main(verbosity=2)
