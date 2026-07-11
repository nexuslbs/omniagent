#!/usr/bin/env python3
"""Run only GROUP 11 prompt plugin tests from tests.py"""
import sys, json, urllib.request, urllib.error
sys.path.insert(0, '/opt/omni/scripts')
import importlib.util as u
spec = u.spec_from_file_location('t', '/opt/omni/scripts/tests.py')
mod = u.module_from_spec(spec)
spec.loader.exec_module(mod)

mod._resolve_prompt_channel()
print(f'Channel: {mod.PROMPT_CHANNEL}')

failed = []
for fn in [
    'test_p7_no_compaction_needed', 'test_p7_compaction_reduces_count', 'test_p7_keep_recent_1',
    'test_p7_zero_tool_calls', 'test_p7_tool_names_preserved', 'test_p7_compact_multiple_tools',
    'test_p7_missing_messages_field', 'test_p7_empty_messages', 'test_p7_idempotent',
    'test_p1_basic_response_structure', 'test_p2_plan_true_attempts_llm',
    'test_p2_plan_false_returns_null', 'test_p2_short_message_with_plan',
    'test_p2_long_complex_no_plan', 'test_p3_system_prompt_content',
    'test_p3_system_message_exists', 'test_p4_greeting_with_plan',
    'test_p4_code_request_no_plan', 'test_p4_empty_prompt',
    'test_p4_long_prompt_no_plan', 'test_p4_multiline_prompt',
    'test_p5_idempotent_plan_null', 'test_p5_stable_system_prompt_length',
    'test_p6_missing_fallback',
]:
    try:
        getattr(mod, fn)()
        print(f'  PASS {fn}')
    except Exception as e:
        print(f'  FAIL {fn}: {e}')
        failed.append(fn)

n = len(failed)
total = 24 - n
print(f'\nResults: {total}/24 passed, {n} failed')
sys.exit(1 if failed else 0)
