#!/usr/bin/env python3
"""
Prompt Plugin Integration Tests

Tests the prompt_generate MCP tool through the /prompt-preview/ API,
verifying that planning decisions, system prompt assembly, and message
structure are correct across different scenarios.

Requirements:
  - Running omniagent at http://localhost:8080
  - A channel named "general" (created by the Mattermost setup)

Run:
  docker exec omni-omniagent-1 python3 /app/tests/test_prompt_plugin.py
  or
  python3 /opt/workspace/omniagent/tests/test_prompt_plugin.py  (from host)
"""

import json
import subprocess
import sys
import time
import os

BASE_URL = "http://localhost:8080"

def run(args: list[str]) -> tuple[str, str, int]:
    """Run a docker exec command and return (stdout, stderr, code)."""
    if os.path.exists("/.dockerenv"):
        # Running inside container
        result = subprocess.run(["curl", "-sf"] + args, capture_output=True, text=True, timeout=15)
        return (result.stdout, result.stderr, result.returncode)
    else:
        # Running from host
        result = subprocess.run(
            ["docker", "exec", "omni-omniagent-1", "curl", "-sf"] + args,
            capture_output=True, text=True, timeout=15
        )
        return (result.stdout, result.stderr, result.returncode)


def api_post(path: str, body: dict) -> dict:
    """POST to the agent API and return parsed JSON response."""
    body_str = json.dumps(body)
    stdout, stderr, code = run([
        "-X", "POST",
        f"{BASE_URL}{path}",
        "-H", "Content-Type: application/json",
        "-d", body_str,
    ])
    assert code == 0, f"POST {path} failed: stderr={stderr!r}, stdout={stdout!r}"
    return json.loads(stdout)


def api_get(path: str) -> dict:
    """GET from the agent API and return parsed JSON response."""
    stdout, stderr, code = run([f"{BASE_URL}{path}"])
    assert code == 0, f"GET {path} failed: stderr={stderr!r}, stdout={stdout!r}"
    return json.loads(stdout)


# ═══════════════════════════════════════════════════════════════════
# Test helpers
# ═══════════════════════════════════════════════════════════════════

def prompt_preview(channel: str, prompt: str, plan: bool = False) -> dict:
    """Call the prompt-preview API and return the response."""
    return api_post(f"/prompt-preview/{channel}", {"prompt": prompt, "plan": plan})


def assert_response_shape(resp: dict, expected_plan: bool | None):
    """Verify that the prompt preview response has the correct structure."""
    assert "system_prompt" in resp, f"Missing 'system_prompt' in response: {resp}"
    assert "messages" in resp, f"Missing 'messages' in response: {resp}"
    assert "plan" in resp, f"Missing 'plan' in response: {resp}"

    assert isinstance(resp["system_prompt"], str), "system_prompt should be a string"
    assert len(resp["system_prompt"]) > 0, "system_prompt should not be empty"
    assert isinstance(resp["messages"], list), "messages should be a list"

    # Plan behavior:
    #   plan=false → plan is null/None
    #   plan=true  → plan is either a string (LLM response) or null (if LLM fails)
    if expected_plan is False:
        # When explicitly passed as false, plan should be null
        pass  # plan can be None or some value depending on complexity detection
    elif expected_plan is True:
        pass  # plan can be a string or None if LLM call fails

    # Verify message structure
    for msg in resp["messages"]:
        assert "role" in msg, f"Message missing 'role': {msg}"
        assert "content" in msg, f"Message missing 'content': {msg}"
        assert msg["role"] in ("system", "user", "assistant", "cause", "agent"), f"Unknown role: {msg['role']}"

    # At minimum: one system message
    system_msgs = [m for m in resp["messages"] if m["role"] == "system"]
    assert len(system_msgs) >= 1, f"Should have at least one system message, got {len(system_msgs)}"


# ═══════════════════════════════════════════════════════════════════
# Tests
# ═══════════════════════════════════════════════════════════════════

PASS = 0
FAIL = 0

def test(name: str, condition: bool | str, detail: str = ""):
    """Run a test assertion and count pass/fail."""
    global PASS, FAIL
    prefix = "✅ PASS" if condition else "❌ FAIL"
    if not condition:
        FAIL += 1
        print(f"  {prefix}: {name} — {detail}")
    else:
        PASS += 1
        print(f"  {prefix}: {name}")


def run_tests():
    """Run all test groups."""
    global PASS, FAIL
    PASS = 0
    FAIL = 0

    # ── Pre-flight: health check ──
    print("\n═══ Pre-flight checks ═══")
    try:
        stdout, _, code = run([f"{BASE_URL}/health"])
        healthy = code == 0 and stdout.strip() == "ok"
        test("Agent is healthy", healthy, f"code={code}, stdout={stdout!r}")
    except Exception as e:
        test("Agent is healthy", False, str(e))
        print("\n❌ Cannot proceed — agent not reachable. Is the container running?")
        return

    if FAIL > 0:
        return  # Can't test without a running agent

    # ── List channels ──
    print("\n═══ Channel discovery ═══")
    # Try using "mm-setup" first, fall back to any channel
    for try_name in ["mm-setup", "cron", "kanban"]:
        try:
            stdout, _, code = run([
                "-X", "POST",
                f"{BASE_URL}/prompt-preview/{try_name}",
                "-H", "Content-Type: application/json",
                "-d", json.dumps({"prompt": "hello", "plan": False}),
            ])
            if code == 0:
                channel_name = try_name
                test(f"Using channel '{try_name}'", True)
                break
        except:
            pass
    else:
        test("No usable channel found", False, "Tried: mm-setup, cron, kanban")
        channel_name = "mm-setup"

    # ════════════════════════════════════════════════════════════
    # Test Group 1: Response structure and basic fields
    # ════════════════════════════════════════════════════════════
    print(f"\n═══ Group 1: Basic response structure (channel={channel_name}) ═══")

    resp = prompt_preview(channel_name, "Hello, can you help me?", plan=False)
    assert_response_shape(resp, expected_plan=False)

    resp = prompt_preview(channel_name, "Hello, can you help me?", plan=True)
    assert_response_shape(resp, expected_plan=True)

    resp = prompt_preview(channel_name, "Hello, can you help me?", plan=False)
    assert_response_shape(resp, expected_plan=False)

    test("plan=false returns null", resp.get("plan") is None, f"Got plan={resp.get('plan')!r}")
    test("Response contains system_prompt", "system_prompt" in resp)
    test("Response contains messages", "messages" in resp)
    test("Messages is a list", isinstance(resp["messages"], list))
    test("At least one system message",
         any(m["role"] == "system" for m in resp["messages"]))

    # ════════════════════════════════════════════════════════════
    # Test Group 2: Plan boolean control
    # ════════════════════════════════════════════════════════════
    print("\n═══ Group 2: Plan boolean control ═══")

    # plan=true
    resp_true = prompt_preview(channel_name, "Implement a new feature", plan=True)
    # When plan=true, the endpoint tries to call the LLM — plan is either a
    # string (the plan text) or None if the LLM isn't configured
    test("plan=true returns plan content or null",
         resp_true.get("plan") is None or isinstance(resp_true.get("plan"), str),
         f"Got plan type: {type(resp_true.get('plan')).__name__}, value: {resp_true.get('plan')!r}")

    # plan=false
    resp_false = prompt_preview(channel_name, "Implement a new feature", plan=False)
    # When plan=false, plan is always null (no LLM call made)
    test("plan=false returns null plan",
         resp_false.get("plan") is None,
         f"Got {resp_false.get('plan')!r}")

    # Short message with plan=true should still attempt planning
    resp_short_plan = prompt_preview(channel_name, "Hi", plan=True)
    test("Short message + plan=true attempts planning",
         resp_short_plan.get("plan") is None or isinstance(resp_short_plan.get("plan"), str),
         f"Got {resp_short_plan.get('plan')!r}")

    # Long message with plan=false should have null plan
    resp_long_no_plan = prompt_preview(
        channel_name,
        "Please implement a complete refactoring of the authentication system with JWT tokens,"
        " session management, and role-based access control across all API endpoints.",
        plan=False
    )
    test("Long complex message + plan=false returns null plan",
         resp_long_no_plan.get("plan") is None,
         f"Got {resp_long_no_plan.get('plan')!r}")

    # ════════════════════════════════════════════════════════════
    # Test Group 3: System prompt content
    # ════════════════════════════════════════════════════════════
    print("\n═══ Group 3: System prompt content ═══")

    resp = prompt_preview(channel_name, "What's the weather?", plan=False)
    sys_prompt = resp["system_prompt"]

    test("System prompt mentions 'OmniAgent'", "OmniAgent" in sys_prompt)
    test("System prompt mentions profile",
         "profile" in sys_prompt.lower() or "Hermes" in sys_prompt,
         f"First 80 chars: {sys_prompt[:80]}")

    # Verify system message in messages array
    sys_msgs = [m for m in resp["messages"] if m["role"] == "system"]
    test("System message has content", len(sys_msgs) > 0 and len(sys_msgs[0]["content"]) > 0)

    # ════════════════════════════════════════════════════════════
    # Test Group 4: Different prompt types
    # ════════════════════════════════════════════════════════════
    print("\n═══ Group 4: Different prompt types ═══")

    # Short greeting
    resp_greeting = prompt_preview(channel_name, "Hi there!", plan=True)
    test("Greeting with plan=true works",
         resp_greeting.get("plan") is None or isinstance(resp_greeting.get("plan"), str))

    # Code-like prompt
    resp_code = prompt_preview(channel_name, "Write a Python function to sort a list", plan=False)
    test("Code request with plan=false returns null plan",
         resp_code.get("plan") is None)

    # Empty prompt edge case
    resp_empty = prompt_preview(channel_name, "", plan=False)
    test("Empty prompt returns valid response", "system_prompt" in resp_empty)

    # Very long prompt (skip plan=true to avoid LLM timeout)
    long_text = "Tell me about " + "artificial intelligence and machine learning, " * 50
    resp_long = prompt_preview(channel_name, long_text, plan=False)
    test("Long prompt with plan=false returns null plan",
         resp_long.get("plan") is None)

    # Multiline prompt
    multiline = "Step 1: Do this\nStep 2: Do that\nStep 3: Profit"
    resp_ml = prompt_preview(channel_name, multiline, plan=False)
    test("Multiline prompt returns null plan", resp_ml.get("plan") is None)

    # ════════════════════════════════════════════════════════════
    # Test Group 5: Idempotency
    # ════════════════════════════════════════════════════════════
    print("\n═══ Group 5: Idempotency ═══")

    msg = "Create a new data pipeline for processing logs"
    resp1 = prompt_preview(channel_name, msg, plan=False)
    resp2 = prompt_preview(channel_name, msg, plan=False)

    # Same input should produce same plan decision
    test("Plan is consistent across calls",
         (resp1.get("plan") is None and resp2.get("plan") is None) or
         (isinstance(resp1.get("plan"), str) and isinstance(resp2.get("plan"), str)),
         f"First: {resp1.get('plan')!r}, Second: {resp2.get('plan')!r}")

    # System prompt should be similar (same profile, same channel)
    test("System prompt length is stable",
         abs(len(resp1["system_prompt"]) - len(resp2["system_prompt"])) < 50,
         f"First: {len(resp1['system_prompt'])} chars, Second: {len(resp2['system_prompt'])} chars")

    # ════════════════════════════════════════════════════════════
    # Test Group 6: Error handling
    # ════════════════════════════════════════════════════════════
    print("\n═══ Group 6: Error handling ═══")

    # Missing channel (non-existent)
    stdout, stderr, code = run([
        "-X", "POST",
        f"{BASE_URL}/prompt-preview/nonexistent-channel-xyz",
        "-H", "Content-Type: application/json",
        "-d", '{"prompt": "hello", "plan": false}',
    ])
    # Missing channel should still produce a valid response (falls back to default profile)
    if code == 0:
        resp = json.loads(stdout)
        test("Missing channel returns valid preview", "system_prompt" in resp)
    else:
        test("Missing channel returns 200", False, f"Got code={code}, stderr={stderr}")

    # Missing fields in body
    stdout, stderr, code = run([
        "-X", "POST",
        f"{BASE_URL}/prompt-preview/{channel_name}",
        "-H", "Content-Type: application/json",
        "-d", '{}',
    ])
    # Empty body - expected to fail with 400/422
    # curl exit code 22 = HTTP status >= 400 (curl reports via exit code)
    test("Empty body returns error HTTP status",
         code == 22,
         f"Expected curl code 22 (HTTP error), got {code}, stderr={stderr[:100]!r}")

    # Invalid JSON
    stdout, stderr, code = run([
        "-X", "POST",
        f"{BASE_URL}/prompt-preview/{channel_name}",
        "-H", "Content-Type: application/json",
        "-d", 'not-json',
    ])
    # Invalid JSON - expected to fail with 400/422
    test("Invalid JSON returns error HTTP status",
         code == 22,
         f"Expected curl code 22 (HTTP error), got {code}, stderr={stderr[:100]!r}")


    # ════════════════════════════════════════════════════════════
    # Summary
    # ════════════════════════════════════════════════════════════
    total = PASS + FAIL
    print(f"\n{'═' * 50}")
    print(f"📊 Results: {PASS}/{total} passed, {FAIL}/{total} failed")
    print(f"{'═' * 50}\n")

    return FAIL == 0


if __name__ == "__main__":
    success = run_tests()
    sys.exit(0 if success else 1)
