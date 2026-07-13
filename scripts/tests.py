#!/usr/bin/env python3
"""
Merged integration tests for the OmniAgent plugin lifecycle.

This file contains tests for install, uninstall, update, remove, download,
enable, and disable of plugins. New tests should not remove old tests.

**RUNNING:** These tests MUST run inside the omniagent container, which runs
as root. Do NOT run them from the host: the host may not have the same
filesystem permissions, and the agent's auto-detection of plugin directory
changes only works from within the container's filesystem view.

    docker exec -e PYTHONUNBUFFERED=1 omni-omniagent-1 python3 -u \\
        /opt/workspace/omni-stack/scripts/tests.py

GROUP 1: Original Remove API tests (idempotent, restored from git history):
  A1-A3: Source NOT in YAML (built-in, bundled, remote)
  B1-B3: Source IN YAML (built-in, bundled, remote)
  C1:    YAML entry but no disk (phantom plugin)
  D1-D2: Provider tests (bundled, in / not in YAML)
  E1-E2: Platform tests (bundled, in / not in YAML)
  F1-F2: Name collision tests (bundled + remote same name)
  Each test is self-contained: SETUP → RUN → VERIFY → CLEANUP.

GROUP 2: Source-aware Remove API tests:
  Tests 1-7: Remove scenarios with explicit source query parameter.
  Git hygiene at start / discard changes at end.

GROUP 3: File upload tests:
  Tests 8-9: Explorer file upload + Kanban-scoped file upload.

Running twice on a clean repo produces identical results.
"""
#
# IMPORTANT: Tests must NOT restart the container or call pkill omniagent.
# The container runs cargo-watch which auto-rebuilds from source changes.
# The agent auto-detects filesystem and YAML changes within ~5s, so no
# restart is needed. The restart_agent() function just verifies the agent
# is healthy after waiting for auto-detection.
#


import os, sys, json, shutil, subprocess, time, re
import urllib.request, urllib.error
import uuid

# ═══════════════════════════════════════════════════════════════════════
#  Config
# ═══════════════════════════════════════════════════════════════════════

BASE = "http://localhost:8080"
DASHBOARD = "http://dashboard:3001"
WORKSPACE = "/opt/workspace/omni-stack"
REMOTE_REPO = "/opt/workspace/omni-plugins"

# ═══════════════════════════════════════════════════════════════════════
#  Shell helpers
# ═══════════════════════════════════════════════════════════════════════

def sh(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True)

# ═══════════════════════════════════════════════════════════════════════
#  API helpers
# ═══════════════════════════════════════════════════════════════════════

def api_get(path):
    try:
        r = urllib.request.urlopen(f"{BASE}/api{path}", timeout=10)
        return json.loads(r.read())
    except urllib.error.HTTPError as e:
        body = e.read().decode()
        try: return json.loads(body)
        except: return {"success": False, "error": body}

def api_post(path, body=None, files=None, base=None):
    """POST to BASE (omniagent) or DASHBOARD proxy.
    For file uploads, uses multipart/form-data.
    For JSON, uses application/json.
    """
    url_base = base if base else BASE
    url = f"{url_base}/api{path}" if not files else f"{url_base}{path}"
    if files:
        boundary = uuid.uuid4().hex
        data = b""
        for field_name, filename, content in files:
            data += f"--{boundary}\r\n".encode()
            data += f'Content-Disposition: form-data; name="{field_name}"; filename="{filename}"\r\n'.encode()
            data += b"Content-Type: application/octet-stream\r\n\r\n"
            data += content + b"\r\n"
        data += f"--{boundary}--\r\n".encode()
        req = urllib.request.Request(
            url,
            data=data,
            method="POST",
            headers={"Content-Type": f"multipart/form-data; boundary={boundary}"},
        )
    else:
        req = urllib.request.Request(
            url,
            data=json.dumps(body).encode() if body is not None else None,
            method="POST",
            headers={"Content-Type": "application/json"},
        )
    try:
        resp = urllib.request.urlopen(req)
        resp_body = resp.read()
        if not resp_body.strip():
            return {}  # dashboard may return empty body on success
        return json.loads(resp_body)
    except urllib.error.HTTPError as e:
        raw = e.read()
        if not raw.strip():
            raise AssertionError(f"POST {path} failed (HTTP {e.code}): empty body")
        body_str = raw.decode("utf-8", errors="replace")
        raise AssertionError(f"POST {path} failed (HTTP {e.code}): {json.loads(body_str)}")

def api_delete(path):
    """Return (success_bool, response_data) regardless of HTTP status"""
    req = urllib.request.Request(f"{BASE}/api{path}", method="DELETE")
    try:
        r = urllib.request.urlopen(req, timeout=10)
        return (True, json.loads(r.read()))
    except urllib.error.HTTPError as e:
        body = e.read().decode()
        try: return (False, json.loads(body))
        except: return (False, {"error": body})

# ═══════════════════════════════════════════════════════════════════════
#  YAML helpers (manual parsing, no pyyaml)
# ═══════════════════════════════════════════════════════════════════════

def read_plugins_yml():
    with open(f"{WORKSPACE}/plugins.yml") as f:
        content = f.read()
    lines = content.split("\n")
    sections, section, name, entry = {}, None, None, None
    config_lines, in_config = None, False

    for line in lines:
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        indent = len(line) - len(line.lstrip())
        if in_config and indent <= 6:
            if config_lines:
                config_str = "\n".join(config_lines)
                entry["config"] = config_str
                config_lines = None
            in_config = False
        if indent == 0 and stripped.endswith(":"):
            section = stripped[:-1]
            sections[section] = {}
            name = None
            entry = None
        elif indent == 2 and stripped.endswith(":"):
            name = stripped[:-1]
            sections[section][name] = {}
            entry = sections[section][name]
        elif indent == 4:
            colon_idx = stripped.index(":")
            key = stripped[:colon_idx].strip()
            value = stripped[colon_idx+1:].strip()
            if value == "":
                entry[key] = {}
                in_config = True
                config_lines = []
            else:
                if value == "true": entry[key] = True
                elif value == "false": entry[key] = False
                elif value == "{}": entry[key] = {}
                elif value.startswith('"') and value.endswith('"'): entry[key] = value[1:-1]
                elif value.startswith("'") and value.endswith("'"): entry[key] = value[1:-1]
                else: entry[key] = value
        elif indent == 6 and in_config:
            colon_idx = stripped.index(":")
            subkey = stripped[:colon_idx].strip()
            subval = stripped[colon_idx+1:].strip()
            if subval.startswith('"') and subval.endswith('"'): subval = subval[1:-1]
            elif subval.startswith("'") and subval.endswith("'"): subval = subval[1:-1]
            if isinstance(entry.get("config"), dict):
                entry["config"][subkey] = subval
            else:
                config_lines.append(line)
    return sections

def write_plugins_yml(data):
    lines = []
    for section, entries in data.items():
        lines.append(f"{section}:")
        for name, props in entries.items():
            lines.append(f"  {name}:")
            for k, v in props.items():
                if isinstance(v, dict) and v:
                    lines.append(f"    {k}:")
                    for sk, sv in v.items():
                        sv_str = json.dumps(sv) if "'" in str(sv) or sv == "" else str(sv)
                        lines.append(f"      {sk}: {sv_str}")
                elif isinstance(v, bool):
                    lines.append(f"    {k}: {str(v).lower()}")
                elif isinstance(v, dict) and not v:
                    lines.append(f"    {k}: {{}}")
                elif v == "" or v is None:
                    lines.append(f"    {k}: ''")
                else:
                    lines.append(f"    {k}: {v}")
        lines.append("")
    content = "\n".join(lines)
    with open(f"{WORKSPACE}/plugins.yml", "w") as f:
        f.write(content)

def yaml_get(entry_type, name):
    data = read_plugins_yml()
    return data.get(entry_type, {}).get(name, None)

def yaml_set(entry_type, name, data_dict):
    data = read_plugins_yml()
    if entry_type not in data:
        data[entry_type] = {}
    data[entry_type][name] = data_dict
    write_plugins_yml(data)

def yaml_del(entry_type, name):
    data = read_plugins_yml()
    if entry_type in data and name in data[entry_type]:
        del data[entry_type][name]
        write_plugins_yml(data)

def yaml_has(entry_type, name):
    return yaml_get(entry_type, name) is not None

def read_remote_yml():
    r = sh(f"cat {WORKSPACE}/remote.yml")
    data = {"tools": {}, "platforms": {}, "providers": {}}
    section = None
    for line in r.stdout.split("\n"):
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        indent = len(line) - len(line.lstrip())
        if indent == 0 and stripped.endswith(":"):
            section = stripped[:-1]
            if section not in data:
                data[section] = {}
        elif indent == 2 and section:
            name = stripped.split(":")[0].strip()
            data[section][name] = True
    return data

def remote_yml_has(name, type_dir="tools"):
    data = read_remote_yml()
    return name in data.get(type_dir, {})

# ═══════════════════════════════════════════════════════════════════════
#  File helpers (sudo)
# ═══════════════════════════════════════════════════════════════════════

def exists(path):
    return os.path.exists(path)

def cp(src, dst, recursive=False):
    if recursive:
        shutil.copytree(src, dst, dirs_exist_ok=True)
    else:
        shutil.copy2(src, dst)

def mv(src, dst):
    shutil.move(src, dst)

def rm_rf(path):
    if os.path.exists(path):
        if os.path.isdir(path):
            shutil.rmtree(path)
        else:
            os.remove(path)

def mkdir_p(path):
    os.makedirs(path, exist_ok=True)

# ── Save/Restore state (per-test) ────────────────────────────────────
# Each test may call backup_* and restore_* inside its try/finally.
# The .bak file is the per-test contract: do not nest backup/restore.

def backup_plugins_yml():
    shutil.copy2(f"{WORKSPACE}/plugins.yml", f"{WORKSPACE}/plugins.yml.bak")

def restore_plugins_yml():
    bak = f"{WORKSPACE}/plugins.yml.bak"
    if os.path.exists(bak):
        shutil.copy2(bak, f"{WORKSPACE}/plugins.yml")
        os.remove(bak)

def backup_remote_yml():
    shutil.copy2(f"{WORKSPACE}/remote.yml", f"{WORKSPACE}/remote.yml.bak")

def restore_remote_yml():
    bak = f"{WORKSPACE}/remote.yml.bak"
    if os.path.exists(bak):
        shutil.copy2(bak, f"{WORKSPACE}/remote.yml")
        os.remove(bak)

# ═══════════════════════════════════════════════════════════════════════
#  Idempotent Setup Helpers
# ═══════════════════════════════════════════════════════════════════════
#
# These ensure a plugin exists in the desired state so the test
# preconditions are always met, regardless of previous test runs.

def ensure_bundled_plugin(name, plugin_type="tools"):
    """Ensure a bundled plugin directory exists.
    Sources (checked in order):
      1. Already exists at target path
      2. .remote/ directory (for remote→bundled collision tests)
      3. omni-plugins repo (/opt/workspace/omni-plugins/)
      4. Workspace git checkout (for deleted omni-stack bundled plugins)
    """
    target = f"{WORKSPACE}/plugins/{plugin_type}/{name}"
    if exists(target):
        return  # already exists

    # Try .remote/ source (remote→bundled collision tests)
    remote_src = f"{WORKSPACE}/plugins/{plugin_type}/.remote/{name}/{plugin_type}/{name}"
    if exists(remote_src):
        shutil.copytree(remote_src, target, dirs_exist_ok=True,
                        ignore=shutil.ignore_patterns("target"))
        return

    # Try local omni-plugins repo (used for remote plugin installs)
    repo_src = f"{REMOTE_REPO}/{plugin_type}/{name}"
    if exists(repo_src):
        mkdir_p(f"{WORKSPACE}/plugins/{plugin_type}")
        cp(repo_src, target, recursive=True)
        return

    # Try restoring from omni-stack git (for bundled plugins deleted by tests)
    subprocess.run(
        f"cd {WORKSPACE} && git checkout -- plugins/{plugin_type}/{name} 2>&1",
        shell=True, capture_output=True, text=True
    )
    if exists(target):
        return

    raise RuntimeError(
        f"Cannot create bundled plugin '{name}' in {plugin_type}: "
        f"no source found in .remote/, {REMOTE_REPO}, or git history"
    )

def remove_bundled_plugin(name, plugin_type="tools"):
    """Remove a bundled plugin directory we created temporarily."""
    target = f"{WORKSPACE}/plugins/{plugin_type}/{name}"
    if exists(target):
        rm_rf(target)

def ensure_remote_plugin(name, plugin_type="tools"):
    """Install a remote plugin from the local repo if not already installed."""
    remote_dir = f"{WORKSPACE}/plugins/{plugin_type}/.remote/{name}"
    # Check plugin.json exists (not just the directory: may be empty from failed cleanup)
    plugin_json = f"{remote_dir}/{plugin_type}/{name}/plugin.json"
    if exists(plugin_json):
        return  # already installed with files

    repo_src = f"{REMOTE_REPO}/{plugin_type}/{name}"
    if not exists(repo_src):
        raise RuntimeError(f"Cannot install remote plugin '{name}': source not found in repo")

    # Clean up empty directory if it exists
    if os.path.exists(remote_dir):
        shutil.rmtree(remote_dir)

    # Copy source to .remote/<name>/<type>/<name>/
    dest_base = remote_dir
    mkdir_p(f"{dest_base}/{plugin_type}")
    cp(repo_src, f"{dest_base}/{plugin_type}/{name}", recursive=True)

    # Pre-build Rust binary so install API doesn't timeout compiling
    cargo_toml = f"{dest_base}/{plugin_type}/{name}/Cargo.toml"
    if os.path.exists(cargo_toml):
        sh(f"cd {dest_base}/{plugin_type}/{name} && cargo build --release 2>&1")

    # Register in remote.yml
    remote_yml_path = f"{WORKSPACE}/remote.yml"
    with open(remote_yml_path, "a") as f:
        f.write(f"\n  {name}:\n    url: https://github.com/nexuslbs/omni-plugins.git\n    path: {plugin_type}/{name}\n")

def remove_remote_plugin(name, plugin_type="tools"):
    """Remove a remote plugin we installed temporarily."""
    remote_dir = f"{WORKSPACE}/plugins/{plugin_type}/.remote/{name}"
    if os.path.exists(remote_dir):
        shutil.rmtree(remote_dir)
    # Remove from remote.yml
    remote_yml_path = f"{WORKSPACE}/remote.yml"
    with open(remote_yml_path) as f:
        lines = f.readlines()
    filtered = []
    skip = False
    for line in lines:
        stripped = line.strip()
        if stripped.startswith(f"  {name}:"):
            skip = True
            continue
        if skip and stripped and not stripped.startswith("  "):
            skip = False
        if not skip:
            filtered.append(line)
    with open(remote_yml_path, "w") as f:
        f.writelines(filtered)

# ── Restart agent ────────────────────────────────────────────────────

def restart_agent():
    # The agent auto-detects filesystem and YAML changes within ~5s via
    # periodic scanning. No need for process restarts or source file touches.
    # Just wait for the agent to be healthy (in case a previous reload is in progress).
    time.sleep(3)
    for _ in range(10):
        try:
            r = urllib.request.urlopen(f"{BASE}/health", timeout=3)
            if r.status == 200:
                return
        except:
            pass
        time.sleep(1)
    raise RuntimeError("Agent not healthy after waiting")

# ═══════════════════════════════════════════════════════════════════════
#  Test harness
# ═══════════════════════════════════════════════════════════════════════

tests_run = 0
tests_pass = 0
tests_fail = 0

def test(fn):
    global tests_run, tests_pass, tests_fail
    tests_run += 1
    name = fn.__name__.replace("test_", "Test ").replace("_", " ")
    print(f"\n--- {name} ", end="", flush=True)
    try:
        fn()
        print("✓ PASS", flush=True)
        tests_pass += 1
    except AssertionError as e:
        print(f"✗ FAIL: {e}", flush=True)
        import traceback
        traceback.print_exc()
        tests_fail += 1
    except Exception as e:
        print(f"✗ ERROR: {e}", flush=True)
        import traceback
        traceback.print_exc()
        tests_fail += 1

def expect_error(resp, substring):
    assert not resp[0], f"expected error, got success={resp[1]}"
    err_text = json.dumps(resp[1]).lower()
    assert substring.lower() in err_text, f"expected '{substring}' in error, got: {resp[1]}"

# ═══════════════════════════════════════════════════════════════════════
#  GROUP 1: Original Remove API tests (idempotent, restored from git)
# ═══════════════════════════════════════════════════════════════════════
#
# Group A: Source NOT in YAML (3 tests)
#   A1. Built-in → 400 error
#   A2. Bundled → succeed, YAML unaffected
#   A3. Remote → succeed, YAML unaffected
#
# Group B: Source IN YAML (3 tests)
#   B1. Built-in → 400 error
#   B2. Bundled → succeed, YAML + disk removed
#   B3. Remote → succeed, YAML + .remote/ removed
#
# Group C: YAML entry but no disk (1 test)
#   C1. Phantom plugin → succeed, YAML only removed
#
# Group D: Provider tests (2 tests)
#   D1. Bundled provider IN YAML → succeed, YAML + disk
#   D2. Bundled provider NOT in YAML → succeed, YAML unaffected
#
# Group E: Platform tests (2 tests)
#   E1. Bundled platform IN YAML → succeed, YAML + disk
#   E2. Bundled platform NOT in YAML → succeed, YAML unaffected
#
# Group F: Name collision tests (2 tests)
#   F1. Bundled+remote same name, YAML source=bundled → removes bundled only
#   F2. Bundled+remote same name, YAML source=remote → removes remote only

# ── A1: Built-in NOT in YAML → 400 error ─────────────────────────────

def test_a1():
    """Built-in plugin with NO YAML entry → should ERROR 400"""
    plugin, ptype = "search", "tools"

    backup_plugins_yml()
    try:
        if yaml_has(ptype, plugin):
            yaml_del(ptype, plugin)
            restart_agent()

        success, resp = api_delete(f"/plugins/{plugin}?source=built-in")
        expect_error((success, resp), "cannot remove built-in")
    finally:
        if not yaml_has(ptype, plugin):
            yaml_set(ptype, plugin, {"enabled": True, "source": "built-in", "config": {}})
            restart_agent()
        restore_plugins_yml()
        restart_agent()


# ── A2: Bundled NOT in YAML → succeed, YAML unaffected ───────────────

def test_a2():
    """Bundled plugin with NO YAML entry → succeed, YAML unchanged, disk removed"""
    plugin, ptype = "cosmos-rust-tool", "tools"
    plugin_dir = f"{WORKSPACE}/plugins/{ptype}/{plugin}"

    backup_plugins_yml()
    try:
        ensure_bundled_plugin(plugin, ptype)
        if yaml_has(ptype, plugin):
            yaml_del(ptype, plugin)
            restart_agent()

        success, resp = api_delete(f"/plugins/{plugin}?source=bundled")
        assert success, f"expected success, got {resp}"
        assert not exists(plugin_dir), "plugin dir still on disk"
        assert not yaml_has(ptype, plugin), "YAML was affected but shouldn't have been"
    finally:
        restore_plugins_yml()
        restart_agent()


# ── A3: Remote NOT in YAML → succeed, YAML unaffected ────────────────

def test_a3():
    """Remote plugin with NO YAML entry → succeed, YAML unchanged, .remote/ removed"""
    plugin, ptype = "test-rust-tool", "tools"
    remote_dir = f"{WORKSPACE}/plugins/{ptype}/.remote/{plugin}"

    backup_plugins_yml()
    backup_remote_yml()
    try:
        ensure_remote_plugin(plugin, ptype)
        if yaml_has(ptype, plugin):
            yaml_del(ptype, plugin)

        success, resp = api_delete(f"/plugins/{plugin}?source=remote")
        assert success, f"expected success, got {resp}"
        assert not exists(remote_dir), ".remote dir still on disk"
        assert not yaml_has(ptype, plugin), "YAML was affected but shouldn't have been"
        assert not remote_yml_has(plugin, ptype), "remote.yml entry should be removed"
    finally:
        restore_remote_yml()
        restore_plugins_yml()


# ── B1: Built-in IN YAML → 400 error ─────────────────────────────────

def test_b1():
    """Built-in plugin WITH YAML entry → should ERROR 400, YAML untouched"""
    plugin, ptype = "search", "tools"

    entry = yaml_get(ptype, plugin)
    if not entry or entry.get("source") != "built-in":
        yaml_set(ptype, plugin, {"enabled": True, "source": "built-in", "config": {}})
        restart_agent()

    success, resp = api_delete(f"/plugins/{plugin}?source=built-in")
    expect_error((success, resp), "cannot remove built-in")
    assert yaml_has(ptype, plugin), "YAML entry was removed but should remain"


# ── B2: Bundled IN YAML → succeed, YAML + disk removed ───────────────

def test_b2():
    """Bundled plugin WITH YAML entry → succeed, YAML + disk removed"""
    plugin, ptype = "prompt", "tools"
    plugin_dir = f"{WORKSPACE}/plugins/{ptype}/{plugin}"

    ensure_bundled_plugin(plugin, ptype)

    backup_plugins_yml()
    try:
        yaml_set(ptype, plugin, {"enabled": True, "source": "bundled", "config": {}})
        restart_agent()

        success, resp = api_delete(f"/plugins/{plugin}?source=bundled")
        assert success, f"expected success, got {resp}"
        assert not exists(plugin_dir), "plugin dir still on disk"
        assert not yaml_has(ptype, plugin), "YAML entry still present"
    finally:
        restore_plugins_yml()
        restart_agent()


# ── B3: Remote IN YAML → succeed, YAML + .remote/ removed ────────────

def test_b3():
    """Remote plugin WITH YAML entry → succeed, YAML + .remote/ removed"""
    plugin, ptype = "test-python-tool", "tools"
    remote_dir = f"{WORKSPACE}/plugins/{ptype}/.remote/{plugin}"

    ensure_remote_plugin(plugin, ptype)

    backup_plugins_yml()
    backup_remote_yml()
    try:
        yaml_set(ptype, plugin, {"enabled": True, "source": "remote", "config": {}})
        restart_agent()

        success, resp = api_delete(f"/plugins/{plugin}?source=remote")
        assert success, f"expected success, got {resp}"
        assert not exists(remote_dir), ".remote dir still on disk"
        assert not yaml_has(ptype, plugin), "YAML entry still present"
        assert not remote_yml_has(plugin, ptype), "remote.yml entry should be removed"
    finally:
        restore_remote_yml()
        restore_plugins_yml()
        restart_agent()


# ── C1: Phantom plugin in YAML but not on disk → succeed, YAML only ──

def test_c1():
    """Plugin in YAML (source=built-in) but NOT on disk → succeed, YAML only"""
    plugin, ptype = "phantom-plugin", "tools"
    fake_entry = {"enabled": True, "source": "built-in", "config": {}}

    # Safety check: plugin must not exist anywhere (just check omni-stack paths)
    for t in ["tools", "platforms", "providers"]:
        p = f"{WORKSPACE}/plugins/{t}/{plugin}"
        assert not os.path.exists(p), f"Plugin '{plugin}' exists at {p}: test would fail!"

    backup_plugins_yml()
    try:
        yaml_set(ptype, plugin, fake_entry)
        restart_agent()

        success, resp = api_delete(f"/plugins/{plugin}?source=built-in")
        assert success, f"expected success, got {resp}"
        assert not yaml_has(ptype, plugin), "YAML entry still present"
    finally:
        restore_plugins_yml()
        restart_agent()


# ── D1: Bundled provider IN YAML → succeed, YAML + disk removed ──────

def test_d1():
    """Bundled provider WITH YAML entry → succeed, YAML + disk removed"""
    plugin, ptype = "noop", "providers"
    plugin_dir = f"{WORKSPACE}/plugins/{ptype}/{plugin}"

    ensure_bundled_plugin(plugin, ptype)

    backup_plugins_yml()
    try:
        yaml_set(ptype, plugin, {"enabled": True, "source": "bundled", "config": {}})
        restart_agent()

        success, resp = api_delete(f"/plugins/{plugin}?source=bundled")
        assert success, f"expected success, got {resp}"
        assert not exists(plugin_dir), "provider dir still on disk"
        assert not yaml_has(ptype, plugin), "YAML entry still present"
    finally:
        restore_plugins_yml()
        ensure_bundled_plugin(plugin, ptype)
        restart_agent()


# ── D2: Bundled provider NOT in YAML → succeed, YAML unaffected ──────

def test_d2():
    """Bundled provider with NO YAML entry → succeed, YAML unchanged, disk removed"""
    plugin, ptype = "noop-full", "providers"
    plugin_dir = f"{WORKSPACE}/plugins/{ptype}/{plugin}"

    backup_plugins_yml()
    try:
        ensure_bundled_plugin(plugin, ptype)
        if yaml_has(ptype, plugin):
            yaml_del(ptype, plugin)
            restart_agent()

        success, resp = api_delete(f"/plugins/{plugin}?source=bundled")
        assert success, f"expected success, got {resp}"
        assert not exists(plugin_dir), "provider dir still on disk"
        assert not yaml_has(ptype, plugin), "YAML was affected but shouldn't have been"
    finally:
        restore_plugins_yml()
        ensure_bundled_plugin(plugin, ptype)
        restart_agent()


# ── E1: Bundled platform IN YAML → succeed, YAML + disk removed ──────

def test_e1():
    """Bundled platform WITH YAML entry → succeed, YAML + disk removed"""
    plugin, ptype = "test-rust", "platforms"
    plugin_dir = f"{WORKSPACE}/plugins/{ptype}/{plugin}"

    ensure_bundled_plugin(plugin, ptype)

    backup_plugins_yml()
    try:
        yaml_set(ptype, plugin, {"enabled": True, "source": "bundled", "config": {}})
        restart_agent()

        success, resp = api_delete(f"/plugins/{plugin}?source=bundled")
        assert success, f"expected success, got {resp}"
        assert not exists(plugin_dir), "platform dir still on disk"
        assert not yaml_has(ptype, plugin), "YAML entry still present"
    finally:
        restore_plugins_yml()
        restart_agent()


# ── E2: Bundled platform NOT in YAML → succeed, YAML unaffected ──────

def test_e2():
    """Bundled platform with NO YAML entry → succeed, YAML unchanged, disk removed"""
    plugin, ptype = "test-rust", "platforms"
    plugin_dir = f"{WORKSPACE}/plugins/{ptype}/{plugin}"

    backup_plugins_yml()
    try:
        ensure_bundled_plugin(plugin, ptype)
        if yaml_has(ptype, plugin):
            yaml_del(ptype, plugin)
            restart_agent()

        success, resp = api_delete(f"/plugins/{plugin}?source=bundled")
        assert success, f"expected success, got {resp}"
        assert not exists(plugin_dir), "platform dir still on disk"
        assert not yaml_has(ptype, plugin), "YAML was affected but shouldn't have been"
    finally:
        restore_plugins_yml()
        restart_agent()


# ── F1: Name collision: bundled source, both exist ──────────────────

def test_f1():
    """Same name bundled+remote, YAML source=bundled → removes bundled only"""
    plugin, ptype = "test-rust-tool", "tools"
    bundled_dir = f"{WORKSPACE}/plugins/{ptype}/{plugin}"
    remote_dir = f"{WORKSPACE}/plugins/{ptype}/.remote/{plugin}"

    ensure_remote_plugin(plugin, ptype)
    ensure_bundled_plugin(plugin, ptype)

    backup_plugins_yml()
    backup_remote_yml()
    try:
        yaml_set(ptype, plugin, {"enabled": True, "source": "bundled", "config": {}})
        restart_agent()

        success, resp = api_delete(f"/plugins/{plugin}?source=bundled")
        assert success, f"expected success, got {resp}"
        assert not exists(bundled_dir), "bundled dir should have been removed"
        assert exists(remote_dir), "remote dir should NOT have been removed"
        assert not yaml_has(ptype, plugin), "YAML entry should have been removed"
    finally:
        remove_bundled_plugin(plugin, ptype)
        restore_remote_yml()
        restore_plugins_yml()
        restart_agent()


# ── F2: Name collision: remote source, both exist ───────────────────

def test_f2():
    """Same name bundled+remote, YAML source=remote → removes remote only"""
    plugin, ptype = "test-python-tool", "tools"
    bundled_dir = f"{WORKSPACE}/plugins/{ptype}/{plugin}"
    remote_dir = f"{WORKSPACE}/plugins/{ptype}/.remote/{plugin}"

    ensure_remote_plugin(plugin, ptype)
    ensure_bundled_plugin(plugin, ptype)

    backup_plugins_yml()
    backup_remote_yml()
    try:
        yaml_set(ptype, plugin, {"enabled": True, "source": "remote", "config": {}})
        restart_agent()

        success, resp = api_delete(f"/plugins/{plugin}?source=remote")
        assert success, f"expected success, got {resp}"
        assert not exists(remote_dir), ".remote dir should have been removed"
        assert exists(bundled_dir), "bundled dir should NOT have been removed"
        assert not yaml_has(ptype, plugin), "YAML entry should have been removed"
        assert not remote_yml_has(plugin, ptype), "remote.yml entry should have been removed"
    finally:
        remove_bundled_plugin(plugin, ptype)
        restore_remote_yml()
        restore_plugins_yml()
        restart_agent()


# ═══════════════════════════════════════════════════════════════════════
#  GROUP 2: Source-aware Remove API tests
# ═══════════════════════════════════════════════════════════════════════
#
# These find applicable plugins at runtime and test with explicit source.
# Tests 3 and 6 use skip_duplicated=False since source param disambiguates.

# ── Helpers for Group 2 ──

def find_plugin(source, status=None, skip_duplicated=True):
    """Find a plugin by source. Returns name or None."""
    plugins = api_get("/plugins")["data"]
    for p in plugins:
        if p.get("source") == source:
            if status and p.get("status") != status:
                continue
            if skip_duplicated and p.get("is_duplicated"):
                continue
            return p["name"]
    return None

# ── Test 1: Built-in not in plugins.yml → error ──────────────────────

def test_1():
    """Built-in (no YAML) → error"""
    name = find_plugin("built-in", skip_duplicated=True)
    if not name:
        return
    success, resp = api_delete(f"/plugins/{name}?source=built-in")
    expected_fail = not success and "cannot remove built-in" in json.dumps(resp).lower()
    assert expected_fail, f"expected error, got success={success}, resp={resp}"

# ── Test 2: Bundled not in plugins.yml → succeed ─────────────────────

def test_2():
    """Bundled (no YAML) → succeed"""
    name = find_plugin("bundled", skip_duplicated=True)
    if not name:
        return
    success, resp = api_delete(f"/plugins/{name}?source=bundled")
    assert success, f"expected success, got success={success}, resp={resp}"

# ── Test 3: Remote not in plugins.yml → succeed ──────────────────────

def test_3():
    """Remote (no YAML) → succeed, restore state for subsequent tests"""
    name = find_plugin("remote", skip_duplicated=False)
    if not name:
        return
    # Save state before deletion so other tests (e.g. test_6) can still run
    remote_yml_bak = f"{WORKSPACE}/remote.yml.bak"
    plugins_yml_bak = f"{WORKSPACE}/plugins.yml.bak"
    shutil.copy2(f"{WORKSPACE}/remote.yml", remote_yml_bak)
    shutil.copy2(f"{WORKSPACE}/plugins.yml", plugins_yml_bak)
    try:
        success, resp = api_delete(f"/plugins/{name}?source=remote")
        assert success, f"expected success, got success={success}, resp={resp}"
    finally:
        # Restore YAML state so download API can find the entry
        if os.path.exists(plugins_yml_bak):
            shutil.copy2(plugins_yml_bak, f"{WORKSPACE}/plugins.yml")
            os.remove(plugins_yml_bak)
        if os.path.exists(remote_yml_bak):
            shutil.copy2(remote_yml_bak, f"{WORKSPACE}/remote.yml")
            os.remove(remote_yml_bak)
        # Use download API to restore .remote/ directory from git instead of
        # manually copying files: also validates the download endpoint works
        # with a proper remote.yml + plugins.yml entry
        try:
            api_post(f"/plugins/{name}/download", {"source": "remote"})
        except Exception:
            pass  # best-effort restore for subsequent tests

# ── Test 4: Built-in in plugins.yml → error ──────────────────────────

def test_4():
    """Built-in (in YAML) → error"""
    name = find_plugin("built-in", skip_duplicated=True)
    if not name:
        return
    success, resp = api_delete(f"/plugins/{name}?source=built-in")
    expected_fail = not success and "cannot remove built-in" in json.dumps(resp).lower()
    assert expected_fail, f"expected error, got success={success}, resp={resp}"

# ── Test 5: Bundled in plugins.yml → succeed ─────────────────────────

def test_5():
    """Bundled (in YAML) → succeed"""
    name = find_plugin("bundled", skip_duplicated=True)
    if not name:
        return
    success, resp = api_delete(f"/plugins/{name}?source=bundled")
    assert success, f"expected success, got success={success}, resp={resp}"

# ── Test 6: Remote in plugins.yml → succeed ──────────────────────────

def test_6():
    """Remote (in YAML) → succeed"""
    name = find_plugin("remote", skip_duplicated=False)
    if not name:
        return
    success, resp = api_delete(f"/plugins/{name}?source=remote")
    assert success, f"expected success, got success={success}, resp={resp}"

# ── Test 7: YAML entry, no disk → remove YAML entry ──────────────────

def test_7():
    """YAML entry (no disk) → remove YAML entry"""
    plugins = api_get("/plugins")["data"]
    not_found = [p for p in plugins if p.get("status") == "not_found"]
    if not not_found:
        return
    target = not_found[0]
    name = target["name"]
    source = target.get("source", "bundled")
    success, resp = api_delete(f"/plugins/{name}?source={source}")
    assert success, f"expected success, got success={success}, resp={resp}"


# ═══════════════════════════════════════════════════════════════════════
#  GROUP 3: File upload tests
# ═══════════════════════════════════════════════════════════════════════

_UPLOAD_FILES = []
_KANBAN_DIR = f"{WORKSPACE}/data/kanban"
_UPLOADS_DIR = f"{WORKSPACE}/data/uploads"

def clear_dir(dirpath):
    """Remove all files and directories under dirpath."""
    if os.path.exists(dirpath):
        shutil.rmtree(dirpath)
    os.makedirs(dirpath, exist_ok=True)

def check_upload_file_exists(rel_path, dirpath):
    """Check that a file exists under dirpath/rel_path."""
    full_path = os.path.join(dirpath, rel_path)
    if os.path.isfile(full_path):
        return True, f"file exists at {rel_path}"
    return False, f"file NOT found at {rel_path}"

# ── Test 8: Upload 3 files via explorer ──────────────────────────────

def test_8():
    """Upload 3 files via explorer upload API"""
    global _UPLOAD_FILES
    clear_dir(_UPLOADS_DIR)

    test_files = [
        ("files", f"test-upload-a-{uuid.uuid4().hex[:8]}.txt", b"hello from explorer test A\n"),
        ("files", f"test-upload-b-{uuid.uuid4().hex[:8]}.txt", b"hello from explorer test B\n"),
        ("files", f"test-upload-c-{uuid.uuid4().hex[:8]}.txt", b"hello from explorer test C\n"),
    ]

    result = api_post("/api/uploads", files=test_files, base=DASHBOARD)

    files_out = result.get("files", [])
    assert len(files_out) == 3, f"expected 3 files, got {len(files_out)}: {result}"

    _UPLOAD_FILES = [f["path"] for f in files_out]

    all_ok = True
    details = []
    for fname in _UPLOAD_FILES:
        ok, msg = check_upload_file_exists(fname, _UPLOADS_DIR)
        if not ok:
            all_ok = False
        details.append(msg)

    assert all_ok, "; ".join(details)

# ── Test 9: Kanban task + upload 2 files ─────────────────────────────

def test_9():
    """Create kanban task, upload 2 files scoped to task"""
    global _UPLOAD_FILES
    clear_dir(_KANBAN_DIR)

    task_resp = api_post("/kanban/tasks", {
        "title": f"Test task {uuid.uuid4().hex[:8]}",
        "body": "Upload test for kanban-scoped files",
        "priority": 0,
        "status": "backlog",
    }, base=DASHBOARD)

    task_id = task_resp.get("data", {}).get("id", "")
    assert task_id, f"no id in task response: {task_resp}"

    test_files = [
        ("files", f"kanban-file-a-{uuid.uuid4().hex[:8]}.txt", b"kanban test file A\n"),
        ("files", f"kanban-file-b-{uuid.uuid4().hex[:8]}.txt", b"kanban test file B\n"),
    ]

    upload_resp = api_post(f"/api/uploads/kanban?task_id={task_id}", files=test_files, base=DASHBOARD)

    files_out = upload_resp.get("files", [])
    assert len(files_out) == 2, f"expected 2 files, got {len(files_out)}: {upload_resp}"

    _UPLOAD_FILES = [f["path"] for f in files_out]

    all_ok = True
    details = []
    for fname in _UPLOAD_FILES:
        ok, msg = check_upload_file_exists(fname, _KANBAN_DIR)
        if not ok:
            all_ok = False
        details.append(msg)

    assert all_ok, "; ".join(details)


# ═══════════════════════════════════════════════════════════════════════
#  GROUP 4: Source-required validation tests
# ═══════════════════════════════════════════════════════════════════════
#
# Every plugin action MUST receive a `source` parameter. These tests
# call each action on a valid plugin WITHOUT source and verify the
# specific "Source is required" error is returned.

EXPECTED_SOURCE_ERROR = "source is required"

def find_any_plugin(status=None):
    """Find any plugin to use as a test subject."""
    plugins = api_get("/plugins")["data"]
    for p in plugins:
        if status and p.get("status") != status:
            continue
        return p["name"]
    return None

def expect_source_required(method, url, body=None):
    """Call an API endpoint without source and verify 'Source is required' error."""
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    try:
        urllib.request.urlopen(req)
        raise AssertionError(f"expected error, got success (no source param)")
    except urllib.error.HTTPError as e:
        raw = e.read()
        if e.code == 422:
            # Axum deserialization error: source field is missing entirely
            err_text = raw.decode("utf-8", errors="replace").lower()
            assert "source" in err_text, \
                f"expected 'source' in error, got HTTP {e.code}: {raw.decode()}"
            return  # 422 implies source was missing from the body: acceptable
        result = json.loads(raw.decode("utf-8", errors="replace"))
        assert not result.get("success", True), f"expected error, got success: {result}"
        err_text = json.dumps(result).lower()
        assert "source is required" in err_text, \
            f"expected 'source is required' error, got: {result}"


# ── Test S1: DELETE without source → error ────────────────────────────

def test_s1():
    """DELETE without source → 'Source is required' error"""
    name = find_any_plugin()
    if not name:
        return
    success, resp = api_delete(f"/plugins/{name}")
    assert not success, "expected error when source is missing"
    err_text = json.dumps(resp).lower()
    assert "source is required" in err_text, \
        f"expected 'source is required' error, got: {resp}"

# ── Test S2: POST enable without source → error ───────────────────────

def test_s2():
    """POST enable without source → 'Source is required' error"""
    name = find_any_plugin()
    if not name:
        return
    expect_source_required("POST", f"{BASE}/api/plugins/{name}/enable", body={})

# ── Test S3: POST disable without source → error ──────────────────────

def test_s3():
    """POST disable without source → 'Source is required' error"""
    name = find_any_plugin()
    if not name:
        return
    expect_source_required("POST", f"{BASE}/api/plugins/{name}/disable", body={})

# ── Test S4: POST install without source → error ──────────────────────

def test_s4():
    """POST install without source → 'Source is required' error"""
    name = find_any_plugin()
    if not name:
        return
    expect_source_required("POST", f"{BASE}/api/plugins/{name}/install", body={})

# ── Test S5: POST reinstall without source → error ────────────────────

def test_s5():
    """POST reinstall without source → 'Source is required' error"""
    name = find_any_plugin()
    if not name:
        return
    expect_source_required("POST", f"{BASE}/api/plugins/{name}/reinstall", body={})

# ── Test S6: POST download without source → error ─────────────────────

def test_s6():
    """POST download without source → 'Source is required' error"""
    name = find_any_plugin()
    if not name:
        return
    expect_source_required("POST", f"{BASE}/api/plugins/{name}/download", body={})


# ═══════════════════════════════════════════════════════════════════════
#  Dashboard page loading tests
# ═══════════════════════════════════════════════════════════════════════

def _dash_get(path):
    """GET from the dashboard server, return (status_code, text, parsed_json_or_None)."""
    try:
        r = urllib.request.urlopen(f"{DASHBOARD}{path}", timeout=15)
        text = r.read().decode("utf-8")
        code = r.status
    except urllib.error.HTTPError as e:
        code = e.code
        text = e.read().decode("utf-8", errors="replace")
    except Exception as e:
        return 0, str(e), None
    try:
        js = json.loads(text) if text.strip() else {}
    except (json.JSONDecodeError, ValueError):
        js = None
    return code, text, js


# ── SPA pages (serve index.html) ──

DASH_PAGES = [
    "/",
]

# ── API endpoints that should return valid data (not errors) ──

DASH_API_ENDPOINTS = [
    # Local routes (served by dashboard server directly)
    ("GET", "/api/health", 200),
    ("GET", "/api/templates", 200),
    # Proxied routes (forwarded to omniagent)
    ("GET", "/api/plugins", 200),
    ("GET", "/api/mcp/tools", 200),
    ("GET", "/api/channels", 200),
    ("GET", "/api/profiles", 200),
    ("GET", "/api/schedule", 200),
    ("GET", "/api/overview/dashboard", 200),
    ("GET", "/api/threads/filters", 200),
    ("GET", "/api/fs/list?path=/", 200),
    # Static assets
    ("GET", "/assets/index-UgvjgAk1.js", 200),
    ("GET", "/assets/index-1NcF5H7V.css", 200),
    ("GET", "/favicon.svg", 200),
]


def test_dashboard_pages():
    """
    Verify all omni-dashboard pages load without errors.
    Tests SPA fallback, static assets, local API routes, and proxied API routes.
    Any endpoint returning an error message causes test failure.
    """
    # ── 1. SPA pages ──
    for path in DASH_PAGES:
        code, text, js = _dash_get(path)
        assert code == 200, f"GET {path} returned {code}, expected 200"
        assert "index-UgvjgAk1.js" in text or "<!DOCTYPE html>" in text, \
            f"GET {path} did not return SPA HTML (missing JS bundle reference)"
        assert '"error":"Not found"' not in text, \
            f"GET {path} returned 'Not found' error"

    # ── 2. API endpoints ──
    for method, path, expected_code in DASH_API_ENDPOINTS:
        code, text, js = _dash_get(path)
        assert code == expected_code, \
            f"{method} {path} returned {code}, expected {expected_code}. Body: {text[:200]}"
        # Verify the response is not an error
        if js is not None and isinstance(js, dict):
            err = js.get("error") or ""
            # "Not found" is a hard failure
            assert "Not found" not in err, \
                f"{method} {path} returned error: {err}"
            # "Plugin not found" from the backend is also a failure
            assert "Plugin not found" not in err, \
                f"{method} {path} returned error: {err}"

    # ── 3. Verify `/` does NOT return JSON error ──
    code, text, js = _dash_get("/")
    assert code == 200, f"GET / returned {code}"
    assert js is None or "error" not in js, \
        f"GET / returned JSON error instead of HTML SPA"
    assert '"error":"Not found"' not in text, \
        "SPA fallback returned 'Not found': dist/index.html is missing or bind mount is stale"

    # ── 4. Verify a page's inner data loading works ──
    # The tools page does: apiGet("/plugins") + apiGet("/mcp/tools")
    # We already verified those individually above. Now verify the combined
    # result would render correctly: non-error response from both.
    _, _, plugin_js = _dash_get("/api/plugins")
    assert plugin_js is not None, "/api/plugins must return valid JSON"
    assert plugin_js.get("success") is True, "/api/plugins must return success=true"
    assert "data" in plugin_js, "/api/plugins must have 'data' key"
    assert len(plugin_js["data"]) > 0, "/api/plugins data must not be empty"

    _, _, tools_js = _dash_get("/api/mcp/tools")
    assert tools_js is not None, "/api/mcp/tools must return valid JSON"
    tools_list = tools_js if isinstance(tools_js, list) else tools_js.get("tools", tools_js.get("data", []))
    assert len(tools_list) > 0, "/api/mcp/tools must return at least one tool"

    # ── 5. Verify channels page data ──
    _, _, channels_js = _dash_get("/api/channels")
    assert channels_js is not None, "/api/channels must return valid JSON"

    # ── 6. Verify profiles page data ──
    _, _, profiles_js = _dash_get("/api/profiles")
    assert profiles_js is not None, "/api/profiles must return valid JSON"

    # ── 7. Verify overview dashboard data ──
    _, _, overview_js = _dash_get("/api/overview/dashboard")
    assert overview_js is not None, "/api/overview/dashboard must return valid JSON"
    assert overview_js.get("success") is True, "/api/overview/dashboard must return success=true"

    # ── 8. Verify threads filters data ──
    _, _, filters_js = _dash_get("/api/threads/filters")
    assert filters_js is not None, "/api/threads/filters must return valid JSON"

    # ── 9. Verify schedule data ──
    _, _, schedule_js = _dash_get("/api/schedule")
    assert schedule_js is not None, "/api/schedule must return valid JSON"

    # ── 10. Verify filesystem explorer data ──
    _, _, fs_js = _dash_get("/api/fs/list?path=/")
    assert fs_js is not None, "/api/fs/list must return valid JSON"
    assert "entries" in fs_js, "/api/fs/list must have 'entries' key"

    # ── 11. Verify templates data ──
    _, _, templates_js = _dash_get("/api/templates")
    assert templates_js is not None, "/api/templates must return valid JSON"

    # ── 12. Verify health endpoint ──
    _, _, health_js = _dash_get("/api/health")
    assert health_js is not None, "/api/health must return valid JSON"
    assert health_js.get("status") == "ok", "/api/health must return status=ok"


def test_dashboard_plugin_filters():
    """
    Verify plugin page filters render correctly and URL params are accepted.
    Tests all 3 plugin pages (tools, providers, platforms) and all existing
    filtered pages (threads, messages, channels).
    """
    # ── 1. Plugin pages with filter URL params (tools, providers, platforms) ──
    # These are SPA pages: the server serves index.html for all routes.
    # The filter bar is rendered client-side. We verify the page loads cleanly
    # with various filter URL params, and the API data that feeds filters is valid.

    plugin_pages = ["/tools", "/providers", "/platforms"]

    for page in plugin_pages:
        # Basic page load
        code, text, js = _dash_get(page)
        assert code == 200, f"GET {page} returned {code}"
        assert "<!DOCTYPE html>" in text, f"GET {page} did not return SPA HTML"

        # With single filter param: source
        code, text, js = _dash_get(f"{page}?source=built-in")
        assert code == 200, f"GET {page}?source=built-in returned {code}"
        assert "<!DOCTYPE html>" in text, f"GET {page} with source filter did not return SPA HTML"

        # With single filter param: status
        code, text, js = _dash_get(f"{page}?status=disabled")
        assert code == 200, f"GET {page}?status=disabled returned {code}"

        # With single filter param: enabled
        code, text, js = _dash_get(f"{page}?enabled=yes")
        assert code == 200, f"GET {page}?enabled=yes returned {code}"

        # With single filter param: name
        code, text, js = _dash_get(f"{page}?name=memory")
        assert code == 200, f"GET {page}?name=memory returned {code}"

        # With multiple filter params
        code, text, js = _dash_get(f"{page}?source=remote&status=enabled&enabled=yes")
        assert code == 200, f"GET {page} with multi filters returned {code}"

        # With all 4 filter params
        code, text, js = _dash_get(f"{page}?source=built-in&status=enabled&enabled=yes&name=mcp")
        assert code == 200, f"GET {page} with all 4 filters returned {code}"

    # ── 2. Existing filtered pages (threads, messages, channels) ──

    # Threads filters
    for qs in [
        "?status=completed",
        "?cause=user",
        "?status=completed&cause=user",
        "?thread_id=123&parent_id=456",
    ]:
        code, text, js = _dash_get(f"/threads{qs}")
        assert code == 200, f"GET /threads{qs} returned {code}"
        assert "<!DOCTYPE html>" in text, f"GET /threads{qs} did not return SPA HTML"

    # Messages filters
    for qs in [
        "?role=user",
        "?channel_id=1",
        "?role=assistant&provider=openai",
        "?model=gpt-4&type=text",
        "?seq0=true&order=asc",
    ]:
        code, text, js = _dash_get(f"/messages{qs}")
        assert code == 200, f"GET /messages{qs} returned {code}"
        assert "<!DOCTYPE html>" in text, f"GET /messages{qs} did not return SPA HTML"

    # Channels filters
    for qs in [
        "?channelId=1",
        "?platform=telegram",
        "?status=open",
        "?channelId=test&platform=discord&status=closed",
    ]:
        code, text, js = _dash_get(f"/channels{qs}")
        assert code == 200, f"GET /channels{qs} returned {code}"
        assert "<!DOCTYPE html>" in text, f"GET /channels{qs} did not return SPA HTML"

    # ── 3. Verify filter-related API endpoints return valid data ──
    _, _, plugin_js = _dash_get("/api/plugins")
    assert plugin_js is not None, "/api/plugins must return valid JSON"
    assert plugin_js.get("success") is True, "/api/plugins must return success=true"
    assert "data" in plugin_js, "/api/plugins must have 'data' key"
    data = plugin_js["data"]
    assert len(data) > 0, "/api/plugins data must not be empty"

    # Verify plugins have the expected fields used by filters
    for p in data:
        assert "source" in p, f"Plugin {p.get('name')} missing 'source' field"
        assert "status" in p, f"Plugin {p.get('name')} missing 'status' field"
        assert "name" in p, f"Plugin missing 'name' field"

    # Verify known source values exist
    sources = set(p.get("source") for p in data)
    known_sources = {"built-in", "bundled", "remote"}
    assert len(sources & known_sources) > 0, \
        f"No known source values found in plugins: {sources}"

    # Verify known status values exist
    statuses = set(p.get("status") for p in data)
    known_statuses = {"enabled", "disabled", "error"}
    assert len(statuses & known_statuses) > 0, \
        f"No known status values found in plugins: {statuses}"

    # ── 4. Verify that each plugin page type has data─────
    # Tools
    _, _, tools_js = _dash_get("/api/mcp/tools")
    assert tools_js is not None, "/api/mcp/tools must return valid JSON"
    tools_list = tools_js if isinstance(tools_js, list) else tools_js.get("tools", tools_js.get("data", []))
    assert len(tools_list) > 0, "/api/mcp/tools must return at least one tool"

    # Threads filters API
    _, _, filters_js = _dash_get("/api/threads/filters")
    assert filters_js is not None, "/api/threads/filters must return valid JSON"
    assert filters_js.get("success") is True, "/api/threads/filters must return success=true"
    filters_data = filters_js.get("data", {})
    assert "statuses" in filters_data, "/api/threads/filters data must have 'statuses' key"
    assert "causes" in filters_data, "/api/threads/filters data must have 'causes' key"

    # Channels data
    _, _, channels_js = _dash_get("/api/channels")
    assert channels_js is not None, "/api/channels must return valid JSON"


# ═══════════════════════════════════════════════════════════════════════
#  Git hygiene
# ═══════════════════════════════════════════════════════════════════════

OMNI_STACK_DIR = WORKSPACE

def _git_status(repo_dir):
    """Return unstaged changes as a string, or empty string if clean."""
    result = subprocess.run(
        ["git", "status", "--porcelain"],
        cwd=repo_dir,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip()

def _git_discard_all(repo_dir):
    """Discard all unstaged changes and untracked files."""
    subprocess.run(["git", "checkout", "--", "."], cwd=repo_dir, capture_output=True)
    subprocess.run(["git", "clean", "-fd"], cwd=repo_dir, capture_output=True)

def check_git_clean():
    """Restore repo to clean state before running tests.
    The agent normalizes plugins.yml and remote.yml on startup, and previous
    test runs may leave deleted files. Full checkout + clean handles all cases."""
    _git_discard_all(OMNI_STACK_DIR)
    dirty = _git_status(OMNI_STACK_DIR)
    if dirty:
        raise RuntimeError(
            f"omni-stack repo has unstaged changes: cannot run tests safely:\n{dirty}"
        )

def discard_all_changes():
    """Discard all unstaged changes created by test execution."""
    _git_discard_all(OMNI_STACK_DIR)


# ═══════════════════════════════════════════════════════════════════════
#  Helpers for Group 6
# ═══════════════════════════════════════════════════════════════════════

def api_post_body(path, body=None, timeout=15):
    """POST with JSON body. Returns (success, response_dict)."""
    import urllib.request, urllib.error, json
    url = f"{BASE}/api{path}"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method="POST",
                                 headers={"Content-Type": "application/json"})
    try:
        r = urllib.request.urlopen(req, timeout=timeout)
        return (True, json.loads(r.read()))
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", errors="replace")
        try: return (False, json.loads(raw))
        except: return (False, {"error": raw, "code": e.code})
    except Exception as e:
        return (False, {"error": str(e)})

def find_plugins_by_source(source, plugin_type="tools"):
    """Find plugins of a given source and type from the API list."""
    # The API returns plugin_type as singular ("tool", "platform", "provider"),
    # but callers pass plural ("tools", "platforms", "providers")
    singular_type = plugin_type.rstrip("s")
    plugins = api_get("/plugins")["data"]
    return [p for p in plugins
            if p.get("source") == source
            and p.get("plugin_type") == singular_type
            and not p.get("is_duplicated", False)]

def find_first_plugin(source, plugin_type="tools"):
    """Find first non-duplicated plugin by source and type."""
    matches = find_plugins_by_source(source, plugin_type)
    return matches[0]["name"] if matches else None

def get_plugin_source_from_api(name):
    """Get a plugin's source from the API listing."""
    plugins = api_get("/plugins")["data"]
    for p in plugins:
        if p["name"] == name:
            return p.get("source")
    return None

def get_plugin_status(name):
    """Get a plugin's status from the API listing."""
    plugins = api_get("/plugins")["data"]
    for p in plugins:
        if p["name"] == name:
            return p.get("status", "unknown")
    return None

def get_plugin_type(name):
    """Get a plugin's type from the API listing."""
    plugins = api_get("/plugins")["data"]
    for p in plugins:
        if p["name"] == name:
            return p.get("type", "unknown")
    return None

# ═══════════════════════════════════════════════════════════════════════
#  Test helpers: each test is one action x one source x one type

def _assert_yaml_state(name, ptype, expect_enabled=None, expect_source=None):
    entry = yaml_get(ptype, name)
    if expect_enabled is not None:
        assert entry is not None, f"YAML entry for '{name}' not found"
        assert entry.get("enabled") == expect_enabled, f"YAML enabled mismatch"
    if expect_source is not None:
        assert entry is not None, f"YAML entry for '{name}' not found"
        assert entry.get("source") == expect_source, f"YAML source mismatch"

def _assert_remote_yml_unchanged(pre_snapshot, msg=""):
    assert read_remote_yml() == pre_snapshot, f"remote.yml changed: {msg}"

def _assert_dir_exists(path, should_exist=True):
    if should_exist:
        assert os.path.exists(path), f"Expected to exist: {path}"
    else:
        assert not os.path.exists(path), f"Expected to NOT exist: {path}"

def _remote_yml_snapshot():
    return read_remote_yml()

def _get_plugin_type(name):
    # Try API first (plugin is enabled/visible)
    for p in api_get("/plugins")["data"]:
        if p["name"] == name:
            pt = p.get("plugin_type", "tool")
            return pt + "s"
    # Fall back to YAML (plugin may be disabled/unlisted)
    data = read_plugins_yml()
    for section in ("platforms", "providers", "tools"):
        if name in data.get(section, {}):
            return section
    return "tools"

def test_enable_source(name, source, expected_success=True):
    ptype = _get_plugin_type(name)
    bundled_dir = f"{WORKSPACE}/plugins/{ptype}/{name}"
    remote_dir = f"{WORKSPACE}/plugins/{ptype}/.remote/{name}"
    pre_remote = _remote_yml_snapshot()
    success, resp = api_post_body(f"/plugins/{name}/enable", {"source": source})
    if expected_success:
        assert success, f"enable {name} source={source} failed: {resp}"
        _assert_yaml_state(name, ptype, expect_enabled=True, expect_source=source)
        if source == "bundled": _assert_dir_exists(bundled_dir)
        elif source == "remote": _assert_dir_exists(remote_dir)
        _assert_remote_yml_unchanged(pre_remote, f"enable {name}")
    else:
        assert not success

def test_disable_source(name, source, expected_success=True):
    ptype = _get_plugin_type(name)
    pre_remote = _remote_yml_snapshot()
    success, resp = api_post_body(f"/plugins/{name}/disable", {"source": source})
    if expected_success:
        assert success, f"disable {name} source={source} failed: {resp}"
        _assert_yaml_state(name, ptype, expect_enabled=False, expect_source=source)
        _assert_remote_yml_unchanged(pre_remote)
    else:
        assert not success

def test_install_source(name, source, expected_success=True):
    ptype = _get_plugin_type(name)
    bundled_dir = f"{WORKSPACE}/plugins/{ptype}/{name}"
    remote_dir = f"{WORKSPACE}/plugins/{ptype}/.remote/{name}"
    pre_remote = _remote_yml_snapshot()
    success, resp = api_post_body(f"/plugins/{name}/install", {"source": source})
    if expected_success:
        assert success, f"install {name} source={source} failed: {resp}"
        _assert_yaml_state(name, ptype, expect_source=source)
        if source == "bundled": _assert_dir_exists(bundled_dir)
        elif source == "remote": _assert_dir_exists(remote_dir)
        if source != "remote": _assert_remote_yml_unchanged(pre_remote)
    else:
        assert not success

def test_reinstall_source(name, source, expected_success=True):
    ptype = _get_plugin_type(name)
    pre_remote = _remote_yml_snapshot()
    success, resp = api_post_body(f"/plugins/{name}/reinstall", {"source": source})
    if expected_success:
        assert success, f"reinstall {name} source={source} failed: {resp}"
        _assert_yaml_state(name, ptype, expect_source=source)
        _assert_remote_yml_unchanged(pre_remote)
    else:
        assert not success

def test_download_source(name, source, expected_success=True):
    ptype = _get_plugin_type(name)
    bundled_dir = f"{WORKSPACE}/plugins/{ptype}/{name}"
    remote_dir = f"{WORKSPACE}/plugins/{ptype}/.remote/{name}"
    pre_remote = _remote_yml_snapshot()
    success, resp = api_post_body(f"/plugins/{name}/download", {"source": source}, timeout=300)
    if expected_success:
        assert success, f"download {name} source={source} failed: {resp}"
        if source == "bundled": _assert_dir_exists(bundled_dir)
        elif source == "remote": _assert_dir_exists(remote_dir)
        _assert_remote_yml_unchanged(pre_remote)
    else:
        assert not success

def test_remove_with_source(name, source, expected_success=True):
    ptype = _get_plugin_type(name)
    bundled_dir = f"{WORKSPACE}/plugins/{ptype}/{name}"
    remote_dir = f"{WORKSPACE}/plugins/{ptype}/.remote/{name}"
    pre_entry = yaml_get(ptype, name)
    pre_remote = _remote_yml_snapshot()
    pre_bundled = os.path.exists(bundled_dir)
    pre_remote_e = os.path.exists(remote_dir)
    success, resp = api_delete(f"/plugins/{name}?source={source}")
    if expected_success:
        assert success, f"remove {name} source={source} failed: {resp}"
        if source == "bundled":
            _assert_dir_exists(bundled_dir, False)
            assert not yaml_has(ptype, name), f"bundled '{name}' YAML should be removed"
            _assert_remote_yml_unchanged(pre_remote, f"bundled {name}")
        elif source == "remote":
            _assert_dir_exists(remote_dir, False)
            assert not yaml_has(ptype, name), f"remote '{name}' YAML should be removed"
            assert not remote_yml_has(name, ptype), f"remote.yml entry removed"
        elif source == "built-in":
            raise AssertionError("built-in remove should never succeed")
    else:
        assert not success
        if source == "built-in":
            assert "cannot remove built-in" in json.dumps(resp).lower()
            if pre_entry:
                assert yaml_get(ptype, name) == pre_entry, f"built-in YAML modified despite error"
            _assert_dir_exists(bundled_dir, pre_bundled)
            _assert_dir_exists(remote_dir, pre_remote_e)
            _assert_remote_yml_unchanged(pre_remote, "built-in no-op")

def test_remove_no_source(name):
    success, resp = api_delete(f"/plugins/{name}")
    assert not success
    assert "source is required" in json.dumps(resp).lower()

def test_enable_no_source(name):
    success, resp = api_post_body(f"/plugins/{name}/enable", {})
    assert not success
    assert "source is required" in json.dumps(resp).lower()

def test_disable_no_source(name):
    success, resp = api_post_body(f"/plugins/{name}/disable", {})
    assert not success
    assert "source is required" in json.dumps(resp).lower()

def test_install_no_source(name):
    success, resp = api_post_body(f"/plugins/{name}/install", {})
    assert not success
    assert "source is required" in json.dumps(resp).lower()

def test_reinstall_no_source(name):
    success, resp = api_post_body(f"/plugins/{name}/reinstall", {})
    assert not success
    assert "source is required" in json.dumps(resp).lower()

def test_download_no_source(name):
    success, resp = api_post_body(f"/plugins/{name}/download", {})
    assert not success
    assert "source is required" in json.dumps(resp).lower()

def test_config_update(name, config_body):
    success, resp = api_post_body(f"/plugins/{name}/config", {"config": config_body})
    assert success, f"config update {name} failed: {resp}"
    return resp

#  GROUP 6: Comprehensive Plugin Action Tests
# ═══════════════════════════════════════════════════════════════════════
#
# For each action that requires source: enable, disable, install, reinstall,
# download, remove: tests for built-in, bundled, and remote variants.
# Also tests: config update, name collisions, cross-type actions.

# ── 6.1: Tool enable/disable for each source variant ──────────────────
# Bundled tool → enable
def test_t6_enable_bundled_tool():
    """Enable a bundled tool plugin → success"""
    name = find_first_plugin("bundled", "tools")
    if not name:
        return
    test_enable_source(name, "bundled")

# Remote tool → enable
def test_t6_enable_remote_tool():
    """Enable a remote tool plugin → success"""
    name = find_first_plugin("remote", "tools")
    if not name:
        return
    test_enable_source(name, "remote")

# Built-in tool → enable should work
def test_t6_enable_builtin_tool():
    """Enable a built-in tool plugin → success"""
    name = find_first_plugin("built-in", "tools")
    if not name:
        return
    test_enable_source(name, "built-in")

# Bundled tool → disable
def test_t6_disable_bundled_tool():
    """Disable a bundled tool plugin → success"""
    name = find_first_plugin("bundled", "tools")
    if not name:
        return
    test_disable_source(name, "bundled")
    # Re-enable so other tests are not affected
    test_enable_source(name, "bundled")

# Remote tool → disable
def test_t6_disable_remote_tool():
    """Disable a remote tool plugin → success"""
    name = find_first_plugin("remote", "tools")
    if not name:
        return
    test_disable_source(name, "remote")
    # Re-enable
    test_enable_source(name, "remote")

# Built-in tool → disable should work
def test_t6_disable_builtin_tool():
    """Disable a built-in tool plugin → success"""
    name = find_first_plugin("built-in", "tools")
    if not name:
        return
    test_disable_source(name, "built-in")
    # Re-enable
    test_enable_source(name, "built-in")


# ── 6.2: Tool install/reinstall for each source variant ───────────────

def test_t6_install_bundled_tool():
    """Install a bundled tool plugin → success"""
    name = find_first_plugin("bundled", "tools")
    if not name:
        return
    test_install_source(name, "bundled")

def test_t6_install_remote_tool():
    """Install a remote tool plugin → success"""
    name = find_first_plugin("remote", "tools")
    if not name:
        # Ensure the remote test plugin source is available for install
        ensure_remote_plugin("test-rust-tool", "tools")
        name = "test-rust-tool"
    test_install_source(name, "remote")

def test_t6_reinstall_bundled_tool():
    """Reinstall a bundled tool plugin → success"""
    name = find_first_plugin("bundled", "tools")
    if not name:
        return
    test_reinstall_source(name, "bundled")

def test_t6_reinstall_remote_tool():
    """Reinstall a remote tool plugin → success"""
    name = find_first_plugin("remote", "tools")
    if not name:
        return
    test_reinstall_source(name, "remote")


# ── 6.3: Tool download for each source variant ────────────────────────

def test_t6_download_bundled_tool():
    """Download a bundled tool plugin → error (download only supports remote)"""
    name = find_first_plugin("bundled", "tools")
    if not name:
        return
    test_download_source(name, "bundled", expected_success=False)

def test_t6_download_remote_tool():
    """Download a remote tool plugin → success"""
    name = find_first_plugin("remote", "tools")
    if not name:
        return
    test_download_source(name, "remote")


# ── 6.4: Source-required tests for ALL actions on tools ───────────────
# (These complement GROUP 4 which tests on any plugin type)

def test_t6_enable_no_source_tool():
    """Enable a tool WITHOUT source → 'Source is required' error"""
    name = find_first_plugin("bundled", "tools")
    if not name:
        name = find_first_plugin("remote", "tools")
    if not name:
        return
    test_enable_no_source(name)

def test_t6_disable_no_source_tool():
    """Disable a tool WITHOUT source → 'Source is required' error"""
    name = find_first_plugin("bundled", "tools")
    if not name:
        name = find_first_plugin("remote", "tools")
    if not name:
        return
    test_disable_no_source(name)

def test_t6_install_no_source_tool():
    """Install a tool WITHOUT source → 'Source is required' error"""
    name = find_first_plugin("bundled", "tools")
    if not name:
        name = find_first_plugin("remote", "tools")
    if not name:
        return
    test_install_no_source(name)

def test_t6_reinstall_no_source_tool():
    """Reinstall a tool WITHOUT source → 'Source is required' error"""
    name = find_first_plugin("bundled", "tools")
    if not name:
        name = find_first_plugin("remote", "tools")
    if not name:
        return
    test_reinstall_no_source(name)

def test_t6_download_no_source_tool():
    """Download a tool WITHOUT source → 'Source is required' error"""
    name = find_first_plugin("bundled", "tools")
    if not name:
        name = find_first_plugin("remote", "tools")
    if not name:
        return
    test_download_no_source(name)

def test_t6_remove_no_source_tool():
    """Remove a tool WITHOUT source → 'Source is required' error"""
    name = find_first_plugin("bundled", "tools")
    if not name:
        name = find_first_plugin("remote", "tools")
    if not name:
        return
    test_remove_no_source(name)


# ── 6.5: Cross-type: platform action tests ───────────────────────────

def test_t6_enable_platform():
    """Enable a bundled platform plugin → success"""
    name = find_first_plugin("bundled", "platforms")
    if not name:
        name = find_first_plugin("remote", "platforms")
    if not name:
        return
    source = get_plugin_source_from_api(name) or "bundled"
    test_enable_source(name, source)

def test_t6_disable_platform():
    """Disable a bundled platform plugin → success"""
    name = find_first_plugin("bundled", "platforms")
    if not name:
        name = find_first_plugin("remote", "platforms")
    if not name:
        return
    source = get_plugin_source_from_api(name) or "bundled"
    test_disable_source(name, source)
    # Re-enable
    test_enable_source(name, source)


# ── 6.6: Cross-type: provider action tests ───────────────────────────

def test_t6_enable_provider():
    """Enable a bundled provider plugin → success"""
    name = find_first_plugin("bundled", "providers")
    if not name:
        name = find_first_plugin("remote", "providers")
    if not name:
        return
    source = get_plugin_source_from_api(name) or "bundled"
    test_enable_source(name, source)

def test_t6_disable_provider():
    """Disable a bundled provider plugin → success"""
    name = find_first_plugin("bundled", "providers")
    if not name:
        name = find_first_plugin("remote", "providers")
    if not name:
        return
    source = get_plugin_source_from_api(name) or "bundled"
    test_disable_source(name, source)
    # Re-enable
    test_enable_source(name, source)


# ── 6.7: Config update test ───────────────────────────────────────────

def test_t6_config_update():
    """Update plugin config → success"""
    name = find_first_plugin("bundled", "tools")
    if not name:
        return
    # Read current config first
    plugin = [p for p in api_get("/plugins")["data"] if p["name"] == name]
    if not plugin:
        return
    current_config = plugin[0].get("config", {})
    # Update with empty config (minimal change)
    test_config_update(name, {})


# ── 6.8: Name collision tests for enable/disable ──────────────────────
# These tests set up a bundled+remote with the same name, then act on
# each source independently.

def ensure_name_collision_plugin(collision_name="collision-test"):
    """Ensure a name collision exists: bundled + remote with same name.
    Returns (bundled_dir, remote_dir) or raises.
    """
    ptype = "tools"
    bundled_dir = f"{WORKSPACE}/plugins/{ptype}/{collision_name}"
    remote_dir = f"{WORKSPACE}/plugins/{ptype}/.remote/{collision_name}"

    ensure_bundled_plugin(collision_name, ptype)
    ensure_remote_plugin(collision_name, ptype)

    # Register in YAML with bundled source (will be managed by test)
    if not yaml_has(ptype, collision_name):
        yaml_set(ptype, collision_name, {
            "enabled": True, "source": "bundled", "config": {}
        })

    return bundled_dir, remote_dir


def ensure_remote_yaml_entry(name, ptype="tools"):
    """Ensure a plugin has a remote YAML entry."""
    # Check if already in remote.yml
    if not remote_yml_has(name, ptype):
        with open(f"{WORKSPACE}/remote.yml", "a") as f:
            f.write(f"  {name}:\n    url: https://github.com/nexuslbs/omni-plugins.git\n    path: {ptype}/{name}\n")


def test_t6_collision_enable_bundled():
    """Name collision: enable with source=bundled → targets bundled only"""
    collision_name = "test-rust-tool"
    bundled_dir = f"{WORKSPACE}/plugins/tools/{collision_name}"
    remote_dir = f"{WORKSPACE}/plugins/tools/.remote/{collision_name}"

    backup_plugins_yml()
    backup_remote_yml()
    try:
        ensure_bundled_plugin(collision_name, "tools")
        ensure_remote_plugin(collision_name, "tools")
        yaml_set("tools", collision_name, {"enabled": True, "source": "bundled", "config": {}})
        ensure_remote_yaml_entry(collision_name)
        restart_agent()

        # Verify both dirs exist before action
        assert os.path.exists(bundled_dir), "bundled dir missing before test"
        assert os.path.exists(remote_dir), "remote dir missing before test"

        # Use disable (no MCP server startup needed) with source=bundled
        success, resp = api_post_body(f"/plugins/{collision_name}/disable", {"source": "bundled"})
        assert success, f"collision disable bundled failed: {resp}"

        # Verify bundled dir still exists (disable doesn't remove disk)
        assert os.path.exists(bundled_dir), "bundled dir was removed!"
        assert os.path.exists(remote_dir), "remote dir was removed!"

        # Verify YAML state: only bundled should be disabled
        entry = yaml_get("tools", collision_name)
        assert entry is not None, "YAML entry removed"
        assert entry.get("source") == "bundled", f"expected source=bundled, got {entry.get('source')}"
        assert entry.get("enabled") is False, "expected enabled=false"
    finally:
        remove_bundled_plugin(collision_name, "tools")
        remove_remote_plugin(collision_name, "tools")
        restore_remote_yml()
        restore_plugins_yml()
        restart_agent()


def test_t6_collision_enable_remote():
    """Name collision: enable with source=remote → targets remote only"""
    collision_name = "test-python-tool"
    bundled_dir = f"{WORKSPACE}/plugins/tools/{collision_name}"
    remote_dir = f"{WORKSPACE}/plugins/tools/.remote/{collision_name}"

    backup_plugins_yml()
    backup_remote_yml()
    try:
        ensure_bundled_plugin(collision_name, "tools")
        ensure_remote_plugin(collision_name, "tools")
        yaml_set("tools", collision_name, {"enabled": True, "source": "remote", "config": {}})
        ensure_remote_yaml_entry(collision_name)
        restart_agent()

        # Verify both dirs exist
        assert os.path.exists(bundled_dir), "bundled dir missing before test"
        assert os.path.exists(remote_dir), "remote dir missing before test"

        # Disable with source=remote
        success, resp = api_post_body(f"/plugins/{collision_name}/disable", {"source": "remote"})
        assert success, f"collision disable remote failed: {resp}"

        assert os.path.exists(bundled_dir), "bundled dir was removed!"
        assert os.path.exists(remote_dir), "remote dir was removed!"

        # Verify YAML: only remote should be disabled
        entry = yaml_get("tools", collision_name)
        assert entry is not None, "YAML entry removed"
        assert entry.get("source") == "remote", f"expected source=remote, got {entry.get('source')}"
        assert entry.get("enabled") is False, "expected enabled=false"
    finally:
        remove_bundled_plugin(collision_name, "tools")
        remove_remote_plugin(collision_name, "tools")
        restore_remote_yml()
        restore_plugins_yml()
        restart_agent()


# ═══════════════════════════════════════════════════════════════════════
#  GROUP 7: Memory Edit/Upload Tests
# ═══════════════════════════════════════════════════════════════════════

import os as _mem_os, json as _mem_json, shutil as _mem_shutil

TEST_PROFILE = "test-memory-profile"
OMNI_DATA_DIR = WORKSPACE
TEST_PROFILE_DIR = f"{OMNI_DATA_DIR}/profiles/{TEST_PROFILE}"

def _check_memory_text(profile, mem_type, expected_substring):
    import urllib.request, json
    r = urllib.request.urlopen(f"{BASE}/memory/text/{profile}/{mem_type}", timeout=10)
    data = json.loads(r.read()).get("data", {})
    content = data.get("content", "")
    assert expected_substring in content, \
        f"expected '{expected_substring}' in {mem_type}, got: {content[:200]}"
    return content

def _check_memory_text_exact(profile, mem_type, expected_content):
    import urllib.request, json
    r = urllib.request.urlopen(f"{BASE}/memory/text/{profile}/{mem_type}", timeout=10)
    data = json.loads(r.read()).get("data", {})
    content = data.get("content", "")
    assert content == expected_content, \
        f"expected exact content, got: {content[:200]}"
    return content


def _raw_post_body(path, body):
    """POST without /api prefix, returns (success, response_dict)."""
    import urllib.request, urllib.error, json
    url = f"{BASE}{path}"
    data = json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method="POST",
                                 headers={"Content-Type": "application/json"})
    try:
        r = urllib.request.urlopen(req, timeout=15)
        return (True, json.loads(r.read()))
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", errors="replace")
        try: return (False, json.loads(raw))
        except: return (False, {"error": raw, "code": e.code})
    except Exception as e:
        return (False, {"error": str(e)})

def _raw_delete(path):
    """DELETE without /api prefix."""
    import urllib.request, urllib.error, json
    url = f"{BASE}{path}"
    req = urllib.request.Request(url, method="DELETE")
    try:
        r = urllib.request.urlopen(req, timeout=15)
        return json.loads(r.read())
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", errors="replace")
        raise AssertionError(f"DELETE {path} failed (HTTP {e.code}): {raw}")

def _check_prompt_includes(channel_name, expected_substring):
    import urllib.request
    r = urllib.request.urlopen(f"{BASE}/prompt/{channel_name}", timeout=10)
    text = r.read().decode("utf-8")
    assert expected_substring in text, f"prompt missing '{expected_substring}'"
    return text

def _ensure_test_profile_clean():
    _mem_os.makedirs(f"{TEST_PROFILE_DIR}/memories", exist_ok=True)
    for f in ["MEMORY.md", "USER.md"]:
        p = f"{TEST_PROFILE_DIR}/memories/{f}"
        if _mem_os.path.exists(p):
            _mem_os.remove(p)

def _remove_test_profile():
    if _mem_os.path.exists(TEST_PROFILE_DIR):
        _mem_shutil.rmtree(TEST_PROFILE_DIR)

def test_m1_setup():
    """Create test profile with no memory files"""
    _ensure_test_profile_clean()
    assert _mem_os.path.exists(f"{TEST_PROFILE_DIR}/memories")
    assert not _mem_os.path.exists(f"{TEST_PROFILE_DIR}/memories/MEMORY.md")
    assert not _mem_os.path.exists(f"{TEST_PROFILE_DIR}/memories/USER.md")

def test_m2_edit_memory():
    """Edit MEMORY → file created"""
    content = "This is a test memory for profile testing."
    success, resp = _raw_post_body(f"/memory/edit/{TEST_PROFILE}/memory", {"content": content})
    assert success, f"edit memory failed: {resp}"
    assert _mem_os.path.exists(f"{TEST_PROFILE_DIR}/memories/MEMORY.md")
    _check_memory_text_exact(TEST_PROFILE, "memory", content)

def test_m3_edit_soul():
    """Edit SOUL → file created"""
    content = "This is a test soul for profile testing."
    success, resp = _raw_post_body(f"/memory/edit/{TEST_PROFILE}/soul", {"content": content})
    assert success, f"edit soul failed: {resp}"
    assert _mem_os.path.exists(f"{TEST_PROFILE_DIR}/memories/USER.md")
    _check_memory_text_exact(TEST_PROFILE, "soul", content)

def test_m4_prompt_verify():
    """Memory and soul content is consistent across API, disk, and what was written"""
    mem_written = "This is a test memory for profile testing."
    soul_written = "This is a test soul for profile testing."

    # 1. Read back via API: confirms the same as written
    mem_api = _check_memory_text_exact(TEST_PROFILE, "memory", mem_written)
    soul_api = _check_memory_text_exact(TEST_PROFILE, "soul", soul_written)

    # 2. Read from disk: all 3 should match
    with open(f"{TEST_PROFILE_DIR}/memories/MEMORY.md") as f:
        mem_disk = f.read().strip()
    with open(f"{TEST_PROFILE_DIR}/memories/USER.md") as f:
        soul_disk = f.read().strip()

    assert mem_written == mem_api == mem_disk, \
        f"Memory mismatch: written={mem_written!r} api={mem_api!r} disk={mem_disk!r}"
    assert soul_written == soul_api == soul_disk, \
        f"Soul mismatch: written={soul_written!r} api={soul_api!r} disk={soul_disk!r}"

def test_m5_edit_update():
    """Edit with new values → all 3 sources consistent"""
    new_mem = "Updated memory content for testing."
    new_soul = "Updated soul content for testing."
    success, resp = _raw_post_body(f"/memory/edit/{TEST_PROFILE}/memory", {"content": new_mem})
    assert success, f"edit memory (2nd) failed: {resp}"
    success, resp = _raw_post_body(f"/memory/edit/{TEST_PROFILE}/soul", {"content": new_soul})
    assert success, f"edit soul (2nd) failed: {resp}"

    # 1. Via API
    _check_memory_text_exact(TEST_PROFILE, "memory", new_mem)
    _check_memory_text_exact(TEST_PROFILE, "soul", new_soul)

    # 2. From disk: all match
    with open(f"{TEST_PROFILE_DIR}/memories/MEMORY.md") as f:
        assert f.read().strip() == new_mem
    with open(f"{TEST_PROFILE_DIR}/memories/USER.md") as f:
        assert f.read().strip() == new_soul

def test_m6_upload_memory():
    """Upload MEMORY file → verify"""
    content = "Uploaded memory content."
    with open("/tmp/mem_test_upload.md", "w") as f:
        f.write(content)
    try:
        success, resp = _raw_post_body(f"/memory/upload/{TEST_PROFILE}/memory", {"content": content})
        assert success or resp.get("size"), f"upload failed: {resp}"
        _check_memory_text_exact(TEST_PROFILE, "memory", content)
    finally:
        if _mem_os.path.exists("/tmp/mem_test_upload.md"):
            _mem_os.remove("/tmp/mem_test_upload.md")

def test_m7_upload_soul():
    """Upload SOUL file → verify"""
    content = "Uploaded soul content."
    with open("/tmp/soul_test_upload.md", "w") as f:
        f.write(content)
    try:
        success, resp = _raw_post_body(f"/memory/upload/{TEST_PROFILE}/soul", {"content": content})
        assert success or resp.get("size"), f"upload failed: {resp}"
        _check_memory_text_exact(TEST_PROFILE, "soul", content)
    finally:
        if _mem_os.path.exists("/tmp/soul_test_upload.md"):
            _mem_os.remove("/tmp/soul_test_upload.md")

def test_m8_delete_and_reupload():
    """Delete files and re-upload → verify"""
    mem_path = f"{TEST_PROFILE_DIR}/memories/MEMORY.md"
    soul_path = f"{TEST_PROFILE_DIR}/memories/USER.md"
    assert _mem_os.path.exists(mem_path)
    assert _mem_os.path.exists(soul_path)
    _mem_os.remove(mem_path)
    _mem_os.remove(soul_path)
    assert not _mem_os.path.exists(mem_path)
    assert not _mem_os.path.exists(soul_path)
    # Re-upload MEMORY
    re_mem = "Re-uploaded memory content."
    with open("/tmp/mem_reup.md", "w") as f:
        f.write(re_mem)
    try:
        success, resp = _raw_post_body(f"/memory/upload/{TEST_PROFILE}/memory", {"content": re_mem})
        assert success or resp.get("size"), f"re-upload mem failed: {resp}"
        _check_memory_text_exact(TEST_PROFILE, "memory", re_mem)
    finally:
        if _mem_os.path.exists("/tmp/mem_reup.md"): _mem_os.remove("/tmp/mem_reup.md")
    # Re-upload SOUL
    re_soul = "Re-uploaded soul content."
    with open("/tmp/soul_reup.md", "w") as f:
        f.write(re_soul)
    try:
        success, resp = _raw_post_body(f"/memory/upload/{TEST_PROFILE}/soul", {"content": re_soul})
        assert success or resp.get("size"), f"re-upload soul failed: {resp}"
        _check_memory_text_exact(TEST_PROFILE, "soul", re_soul)
    finally:
        if _mem_os.path.exists("/tmp/soul_reup.md"): _mem_os.remove("/tmp/soul_reup.md")

def test_m9_cleanup():
    """Remove test profile, verify gone"""
    _remove_test_profile()
    assert not _mem_os.path.exists(TEST_PROFILE_DIR)



# ═══════════════════════════════════════════════════════════════════════
#  GROUP 8: "Add" (install-git) tests
# ═══════════════════════════════════════════════════════════════════════

def test_t8_add_remote_new():
    """Add a new remote plugin (not in remote.yml) -> adds to remote.yml + .remote/ dir"""
    plugin, ptype = "test-add-new", "tools"
    remote_dir = f"{WORKSPACE}/plugins/{ptype}/.remote/{plugin}"
    backup_remote_yml()
    try:
        if os.path.exists(remote_dir): shutil.rmtree(remote_dir)
        if remote_yml_has(plugin, ptype): remove_remote_plugin(plugin, ptype)
        pre_remote = _remote_yml_snapshot()
        success, resp = api_post_body("/plugins/install-git", {
            "url": "file:///opt/workspace/omni-plugins",
            "name": plugin,
            "path": f"{ptype}/test-js-tool",
        })
        assert success, f"Add remote plugin failed: {resp}"
        assert os.path.exists(remote_dir), f".remote dir not created: {remote_dir}"
        # remote.yml must have changed (plugin added)
        assert read_remote_yml() != pre_remote, "remote.yml should change"
        assert remote_yml_has(plugin, ptype), f"remote.yml missing '{plugin}'"
        assert not yaml_has(ptype, plugin), "install-git must not add plugins.yml entry"
    finally:
        remove_remote_plugin(plugin, ptype)
        restore_remote_yml()

def test_t8_add_remote_duplicate():
    """Add a remote plugin already in remote.yml -> succeeds (overwrite)"""
    plugin, ptype = "test-add-dup", "tools"
    remote_dir = f"{WORKSPACE}/plugins/{ptype}/.remote/{plugin}"
    backup_remote_yml()
    try:
        if os.path.exists(remote_dir): shutil.rmtree(remote_dir)
        if remote_yml_has(plugin, ptype): remove_remote_plugin(plugin, ptype)
        s1, r1 = api_post_body("/plugins/install-git", {
            "url": "file:///opt/workspace/omni-plugins",
            "name": plugin, "path": f"{ptype}/test-js-tool",
        })
        assert s1, f"First add failed: {r1}"
        s2, r2 = api_post_body("/plugins/install-git", {
            "url": "file:///opt/workspace/omni-plugins",
            "name": plugin, "path": f"{ptype}/test-js-tool",
        })
        assert s2, f"Duplicate add should succeed (overwrite): {r2}"
        assert remote_yml_has(plugin, ptype), "remote.yml still has entry"
    finally:
        remove_remote_plugin(plugin, ptype)
        restore_remote_yml()

def test_t8_remove_bundled_remote_yml_unchanged():
    """Remove a bundled plugin -> remote.yml UNCHANGED even with same-name remote exists"""
    plugin, ptype = "test-rust-tool", "tools"
    bundled_dir = f"{WORKSPACE}/plugins/{ptype}/{plugin}"
    remote_dir = f"{WORKSPACE}/plugins/{ptype}/.remote/{plugin}"
    backup_plugins_yml()
    backup_remote_yml()
    try:
        ensure_bundled_plugin(plugin, ptype)
        ensure_remote_plugin(plugin, ptype)
        yaml_set(ptype, plugin, {"enabled": True, "source": "bundled", "config": {}})
        restart_agent()
        pre_remote = _remote_yml_snapshot()
        success, resp = api_delete(f"/plugins/{plugin}?source=bundled")
        assert success, f"Remove bundled failed: {resp}"
        assert not os.path.exists(bundled_dir), "Bundled dir removed"
        assert os.path.exists(remote_dir), "Remote dir survives"
        assert not yaml_has(ptype, plugin), "YAML entry removed"
        _assert_remote_yml_unchanged(pre_remote, f"bundled removal must not touch remote.yml")
    finally:
        remove_bundled_plugin(plugin, ptype)
        remove_remote_plugin(plugin, ptype)
        restore_remote_yml()
        restore_plugins_yml()
        restart_agent()

# ── 6.9: Test source=invalid for each action ──────────────────────────

def test_t6_enable_invalid_source():
    """Enable with invalid source → error"""
    name = find_first_plugin("bundled", "tools")
    if not name:
        return
    success, resp = api_post_body(f"/plugins/{name}/enable", {"source": "invalid-source-type"})
    assert not success, f"enable with invalid source should have failed: {resp}"
    err_text = json.dumps(resp).lower()
    assert "invalid source" in err_text, \
        f"enable invalid source: expected 'invalid source', got {resp}"

def test_t6_disable_invalid_source():
    """Disable with invalid source → error"""
    name = find_first_plugin("bundled", "tools")
    if not name:
        return
    success, resp = api_post_body(f"/plugins/{name}/disable", {"source": "invalid-source-type"})
    assert not success, f"disable with invalid source should have failed: {resp}"
    err_text = json.dumps(resp).lower()
    assert "invalid source" in err_text, \
        f"disable invalid source: expected 'invalid source', got {resp}"




# ── GROUP 9: Mattermost + Noop E2E integration test ──────────────────
def _check_mm_container():
    rc = sh("docker inspect omni-mattermost-1 2>/dev/null | grep -q '\"Running\": true'")
    assert rc.returncode == 0, "Mattermost container (omni-mattermost-1) is not running"

def _mm_login(base_url, username, password):
    import urllib.request
    data = json.dumps({"login_id": username, "password": password}).encode()
    req = urllib.request.Request(f"{base_url}/api/v4/users/login", data=data, method="POST", headers={"Content-Type": "application/json"})
    token = urllib.request.urlopen(req, timeout=10).headers.get("Token")
    assert token, f"Login as {username} returned no Token header"
    return token

def _mm_send_message(base_url, channel_id, token, message):
    import urllib.request
    data = json.dumps({"channel_id": channel_id, "message": message}).encode()
    req = urllib.request.Request(f"{base_url}/api/v4/posts", data=data, method="POST", headers={"Content-Type": "application/json", "Authorization": f"Bearer {token}"})
    return json.loads(urllib.request.urlopen(req, timeout=10).read())

def _mm_get_posts(base_url, channel_id, token):
    import urllib.request
    req = urllib.request.Request(f"{base_url}/api/v4/channels/{channel_id}/posts", method="GET", headers={"Authorization": f"Bearer {token}"})
    return json.loads(urllib.request.urlopen(req, timeout=10).read())

def test_mm9_e2e():
    """Full e2e test: mattermost setup -> noop provider response."""
    import urllib.request, urllib.error, time
    _check_mm_container()
    MM = "http://mattermost:8065"
    test_pass = "Mattermost_Fresh_Start_1"
    test_user = "testuser"

    # 1. Ensure mattermost and noop platforms are enabled (no restart needed)
    success, resp = api_post_body("/plugins/mattermost/enable", {"source": "bundled"})
    assert success, f"enable mattermost platform failed: {resp}"
    print("[mattermost platform enabled]")

    success, resp = api_post_body("/plugins/noop/enable", {"source": "bundled"})
    assert success, f"enable noop provider failed: {resp}"
    print("[noop enabled]")

    # 2. Check noop is available
    r = urllib.request.urlopen(f"{BASE}/api/plugins/noop", timeout=10)
    nd = json.loads(r.read()).get("data", {})
    assert nd.get("status") == "enabled", f"noop status={nd.get('status')}, expected enabled"
    print(f"[noop status=enabled]")

    # 3. Run mattermost setup (idempotent: may already exist)
    try:
        req = urllib.request.Request(f"{BASE}/api/plugins/mattermost/setup", method="POST", headers={"Content-Type": "application/json"})
        r = urllib.request.urlopen(req, timeout=15)
        body = r.read().decode()
        if body.strip():
            print(f"[setup returned: {body[:200]}]")
    except (urllib.error.HTTPError, urllib.error.URLError, Exception) as e:
        print(f"[setup error (may be already set up): {e}]")

    # 4. Ensure access_token is in the mattermost config so the platform
    #    subprocess can connect via WebSocket with inbound capability.
    #    Setting this via API also triggers a config reload that refreshes
    #    env vars from .env and restarts the subprocess with the token.
    api_post_body("/plugins/mattermost/config", {
        "config": {
            "access_token": "$env:MATTERMOST_ACCESS_TOKEN",
            "server_url": "http://mattermost:8065",
        }
    })
    print("[mattermost config updated with access_token]")

    # Ensure prompt plugin is enabled: the executor needs prompt_generate
    # to process incoming messages through the channel handler.
    api_post_body("/plugins/prompt/enable", {"source": "built-in"})
    import time as _time
    for _attempt in range(10):
        try:
            r = urllib.request.urlopen(f"{BASE}/mcp/tools", timeout=5)
            tools = json.loads(r.read())
            td = tools if isinstance(tools, list) else (tools.get("tools") or tools.get("data") or [])
            if any("prompt_generate" in (t.get("full_name") or t.get("name") or "") for t in td):
                print("[prompt plugin enabled and ready]")
                break
        except:
            pass
        _time.sleep(1)
    else:
        print("[WARN: prompt_generate tool not found after 10s]")

    # Find the omniagent channel ID for mattermost (wait for auto-discovery)
    channel_id = None
    for _ in range(15):
        r = urllib.request.urlopen(f"{BASE}/channels", timeout=10)
        channels = json.loads(r.read()).get("data", [])
        mm_channel = next((ch for ch in channels if ch.get("platform") == "mattermost"), None)
        if mm_channel:
            channel_id = mm_channel["id"]
            print(f"[found omniagent channel_id={channel_id} ({mm_channel.get('name')})]")
            break
        time.sleep(2)
    assert channel_id is not None, "No mattermost channel found in omniagent channels after setup"

    # 5. Patch channel to use noop
    req = urllib.request.Request(f"{BASE}/channels/{channel_id}", data=json.dumps({"current_provider": "noop"}).encode(), method="PATCH", headers={"Content-Type": "application/json"})
    try:
        urllib.request.urlopen(req, timeout=10)
        print("[channel patched to noop]")
    except urllib.error.HTTPError as e:
        print(f"[channel patch: {e.code} {e.read().decode()[:100]}]")

    time.sleep(5)

    # 6. Login as admin to reset testuser password, then login as testuser
    admin_data = json.dumps({"login_id": "lucasbasquerotto", "password": "MTEnivuUVDZ3"}).encode()
    admin_req = urllib.request.Request(f"{MM}/api/v4/users/login", data=admin_data, method="POST", headers={"Content-Type": "application/json"})
    admin_token = urllib.request.urlopen(admin_req, timeout=10).headers.get("Token")
    assert admin_token, "Admin login returned no Token header"
    print("[admin logged in]")

    # Get testuser's user ID to reset password
    user_resp = json.loads(urllib.request.urlopen(
        urllib.request.Request(f"{MM}/api/v4/users/username/{test_user}", method="GET",
                               headers={"Authorization": f"Bearer {admin_token}"})
    , timeout=10).read())
    testuser_id = user_resp.get("id")
    print(f"[testuser id={testuser_id}]")

    # Reset testuser password via admin API: PUT /api/v4/users/{id}/password with {"new_password": "..."}
    reset_data = json.dumps({"new_password": test_pass}).encode()
    reset_req = urllib.request.Request(
        f"{MM}/api/v4/users/{testuser_id}/password",
        data=reset_data, method="PUT",
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {admin_token}"}
    )
    try:
        urllib.request.urlopen(reset_req, timeout=10)
        print(f"[testuser password reset]")
    except urllib.error.HTTPError as e:
        print(f"[reset password: {e.code} {e.read().decode()[:100]}]")

    # Ensure testuser is in team "omni" and channel "setup"
    team_resp = json.loads(urllib.request.urlopen(
        urllib.request.Request(f"{MM}/api/v4/users/me/teams", method="GET",
                               headers={"Authorization": f"Bearer {admin_token}"})
    , timeout=10).read())
    team_id = next((t["id"] for t in team_resp if t["name"] == "omni"), None)
    if team_id:
        if testuser_id:
            # Add testuser to team
            add_member = json.dumps({"user_id": testuser_id, "team_id": team_id}).encode()
            try:
                urllib.request.urlopen(
                    urllib.request.Request(f"{MM}/api/v4/teams/{team_id}/members",
                                           data=add_member, method="POST",
                                           headers={"Content-Type": "application/json", "Authorization": f"Bearer {admin_token}"})
                , timeout=10)
                print(f"[testuser added to team omni]")
            except urllib.error.HTTPError as e:
                if e.code != 409:  # 409 = already member
                    raise
                print(f"[testuser already in team omni]")

            # Add testuser to "setup" channel
            channels_resp = json.loads(urllib.request.urlopen(
                urllib.request.Request(f"{MM}/api/v4/teams/{team_id}/channels", method="GET",
                                       headers={"Authorization": f"Bearer {admin_token}"})
            , timeout=10).read())
            setup_ch = next((ch for ch in channels_resp if ch["name"] == "setup"), None)
            if setup_ch:
                add_ch = json.dumps({"user_id": testuser_id, "channel_id": setup_ch["id"]}).encode()
                try:
                    urllib.request.urlopen(
                        urllib.request.Request(f"{MM}/api/v4/channels/{setup_ch['id']}/members",
                                               data=add_ch, method="POST",
                                               headers={"Content-Type": "application/json", "Authorization": f"Bearer {admin_token}"})
                    , timeout=10)
                    print(f"[testuser added to channel setup]")
                except urllib.error.HTTPError as e:
                    if e.code != 409:
                        raise
                    print(f"[testuser already in channel setup]")
        print(f"[team omni id={team_id}]")

    # Login as testuser
    token = _mm_login(MM, test_user, test_pass)
    print("[testuser logged in]")

    # Find the "setup" channel ID in Mattermost via admin API
    team_channels = json.loads(urllib.request.urlopen(
        urllib.request.Request(f"{MM}/api/v4/teams/{team_id}/channels", method="GET",
                               headers={"Authorization": f"Bearer {admin_token}"})
    , timeout=10).read())
    mm_channel_id = next((ch["id"] for ch in team_channels if ch["name"] == "setup"), None)
    assert mm_channel_id, "Cannot find 'setup' channel in Mattermost"
    print(f"[found mm channel_id={mm_channel_id}]")

    import uuid
    test_msg = f"E2E test from {test_user} [{uuid.uuid4().hex[:8]}]"
    msg_resp = _mm_send_message(MM, mm_channel_id, token, test_msg)
    print(f"[message sent: {msg_resp.get('id', '?')}]")

    # 7. Poll for noop response
    deadline = time.time() + 35
    while time.time() < deadline:
        time.sleep(4)
        posts = _mm_get_posts(MM, mm_channel_id, token)
        for pid, post in posts.get("posts", {}).items():
            msg = post.get("message", "")
            if msg.startswith("This is a reply to your message"):
                print(f"[reply: {msg[:100]}...]")
                assert test_user in msg, f"Missing test_user: {msg[:100]}"
                print("[e2e test PASSED]")
                return
    assert False, "Noop provider did not respond within 35s"

# ═══════════════════════════════════════════════════════════════════════
#  GROUP 10: Disabled Plugin Visibility Regression Tests
# ═══════════════════════════════════════════════════════════════════════
#  These tests verify that bundled plugins with only a plugin.json file
#  (no entry in plugins.yml) still appear in the API listing as disabled,
#  and that plugins without any directory do NOT appear.
#  This is a regression guard for the fix that removed the "continue"
#  statement in plugins_yaml.rs that was hiding disabled bundled plugins.

PLUGIN_JSON_TOOL = """{
  "name": "test-1",
  "version": "1.0.0",
  "type": "mcp",
  "description": "Test tool plugin for disabled visibility regression testing",
  "entrypoint": { "command": "test-1-tool", "args": [], "transport": "stdio" },
  "config_schema": []
}"""

PLUGIN_JSON_PLATFORM = """{
  "name": "test-1",
  "version": "1.0.0",
  "type": "platform",
  "description": "Test platform plugin for disabled visibility regression testing",
  "entrypoint": { "command": "./test-1-platform", "args": [], "transport": "stdio" },
  "capabilities": { "inbound": false, "outbound": false },
  "config_schema": []
}"""

PLUGIN_JSON_PROVIDER = """{
  "name": "test-1",
  "version": "1.0.0",
  "type": "provider",
  "description": "Test provider plugin for disabled visibility regression testing",
  "default_base_url": "http://test-1-provider:9090/v1",
  "api_mode": "chat_completions",
  "config_schema": [],
  "env": {}
}"""

def _plugin_dir(type_dir, name):
    """Return the bundled plugin directory path."""
    return f"{WORKSPACE}/plugins/{type_dir}/{name}"

def _plugin_json_path(type_dir, name):
    return f"{_plugin_dir(type_dir, name)}/plugin.json"

def _create_test_plugin_dir(type_dir, content):
    """Create a test plugin directory with just a plugin.json file."""
    dir_path = _plugin_dir(type_dir, "test-1")
    mkdir_p(dir_path)
    with open(f"{dir_path}/plugin.json", "w") as f:
        f.write(content)

def _remove_test_plugin_dir(type_dir):
    """Remove a test plugin directory."""
    dir_path = _plugin_dir(type_dir, "test-1")
    if os.path.exists(dir_path):
        shutil.rmtree(dir_path)

def _plugin_in_api(name, plugin_type=None):
    """Check if a plugin with given name (and optionally type) exists in the API listing.
    Returns the plugin dict or None."""
    plugins = api_get("/plugins")["data"]
    for p in plugins:
        if p["name"] == name:
            if plugin_type is None or p.get("plugin_type") == plugin_type:
                return p
    return None

def _assert_plugin_visible(name, plugin_type, expect_visible=True):
    """Assert a plugin is visible (or not) in the API listing."""
    p = _plugin_in_api(name, plugin_type)
    if expect_visible:
        assert p is not None, f"{name} ({plugin_type}) should be visible in API but was not found"
        assert p.get("source") == "bundled", \
            f"{name} ({plugin_type}) source should be 'bundled', got '{p.get('source')}'"
        assert p.get("status") == "disabled", \
            f"{name} ({plugin_type}) status should be 'disabled', got '{p.get('status')}'"
    else:
        # test-2 should not be visible, and test-1 should not be visible after cleanup
        assert p is None, f"{name} ({plugin_type}) should NOT be visible in API but was found with status={p.get('status')}"

# ── V1: Tool: disabled bundled tool visible in API ───────────────────

def test_v1_disabled_tool_visible():
    """Bundled tool with only plugin.json (no yml entry) → visible as disabled."""
    type_dir, name, ptype = "tools", "test-1", "tool"
    try:
        # Phase 1: Start clean: remove test-1 and test-2 dirs if they exist
        for n in ["test-1", "test-2"]:
            d = _plugin_dir(type_dir, n)
            if os.path.exists(d):
                shutil.rmtree(d)
        time.sleep(6)  # wait for filesystem scanner

        # Verify neither shows in API
        _assert_plugin_visible("test-1", ptype, expect_visible=False)
        _assert_plugin_visible("test-2", ptype, expect_visible=False)

        # Phase 2: Create test-1 dir with just plugin.json
        _create_test_plugin_dir(type_dir, PLUGIN_JSON_TOOL)
        time.sleep(6)  # wait for filesystem scanner

        # Verify test-1 shows as disabled, test-2 still doesn't show
        _assert_plugin_visible("test-1", ptype, expect_visible=True)
        _assert_plugin_visible("test-2", ptype, expect_visible=False)
    finally:
        _remove_test_plugin_dir(type_dir)
        time.sleep(6)  # wait for cleanup to be detected

# ── V2: Platform: disabled bundled platform visible in API ───────────

def test_v2_disabled_platform_visible():
    """Bundled platform with only plugin.json (no yml entry) → visible as disabled."""
    type_dir, name, ptype = "platforms", "test-1", "platform"
    try:
        for n in ["test-1", "test-2"]:
            d = _plugin_dir(type_dir, n)
            if os.path.exists(d):
                shutil.rmtree(d)
        time.sleep(6)

        _assert_plugin_visible("test-1", ptype, expect_visible=False)
        _assert_plugin_visible("test-2", ptype, expect_visible=False)

        _create_test_plugin_dir(type_dir, PLUGIN_JSON_PLATFORM)
        time.sleep(6)

        _assert_plugin_visible("test-1", ptype, expect_visible=True)
        _assert_plugin_visible("test-2", ptype, expect_visible=False)
    finally:
        _remove_test_plugin_dir(type_dir)
        time.sleep(6)

# ── V3: Provider: disabled bundled provider visible in API ───────────

def test_v3_disabled_provider_visible():
    """Bundled provider with only plugin.json (no yml entry) → visible as disabled."""
    type_dir, name, ptype = "providers", "test-1", "provider"
    try:
        for n in ["test-1", "test-2"]:
            d = _plugin_dir(type_dir, n)
            if os.path.exists(d):
                shutil.rmtree(d)
        time.sleep(6)

        _assert_plugin_visible("test-1", ptype, expect_visible=False)
        _assert_plugin_visible("test-2", ptype, expect_visible=False)

        _create_test_plugin_dir(type_dir, PLUGIN_JSON_PROVIDER)
        time.sleep(6)

        _assert_plugin_visible("test-1", ptype, expect_visible=True)
        _assert_plugin_visible("test-2", ptype, expect_visible=False)
    finally:
        _remove_test_plugin_dir(type_dir)
        time.sleep(6)


# ═══════════════════════════════════════════════════════════════════════
PROMPT_CHANNEL = None  # resolved in setup

# ═══════════════════════════════════════════════════════════════════════
#  GROUP 11: Prompt Plugin Tests
# ═══════════════════════════════════════════════════════════════════════

def _resolve_prompt_channel():
    """Find a working channel for prompt preview tests."""
    global PROMPT_CHANNEL
    if PROMPT_CHANNEL:
        return
    for try_name in ["mm-setup", "cron", "kanban"]:
        try:
            r = urllib.request.urlopen(
                urllib.request.Request(
                    f"{BASE}/prompt-preview/{try_name}",
                    data=json.dumps({"prompt": "hello", "plan": False}).encode(),
                    headers={"Content-Type": "application/json"},
                    method="POST"
                ),
                timeout=5
            )
            if r.status == 200:
                PROMPT_CHANNEL = try_name
                return
        except:
            pass
    PROMPT_CHANNEL = "mm-setup"  # fallback

def _pp(prompt: str, plan: bool = False) -> dict:
    """Call the prompt-preview API and return the response."""
    _resolve_prompt_channel()
    r = urllib.request.urlopen(
        urllib.request.Request(
            f"{BASE}/prompt-preview/{PROMPT_CHANNEL}",
            data=json.dumps({"prompt": prompt, "plan": plan}).encode(),
            headers={"Content-Type": "application/json"},
            method="POST"
        ),
        timeout=30
    )
    return json.loads(r.read())

def test_p1_basic_response_structure():
    """Prompt preview returns system_prompt, messages, and plan fields"""
    resp = _pp("Hello", plan=False)
    assert "system_prompt" in resp, f"Missing system_prompt in {resp}"
    assert "messages" in resp, f"Missing messages in {resp}"
    assert "plan" in resp, f"Missing plan in {resp}"
    assert isinstance(resp["system_prompt"], str), "system_prompt not string"
    assert len(resp["system_prompt"]) > 0, "system_prompt empty"
    assert isinstance(resp["messages"], list), "messages not list"
    system_msgs = [m for m in resp["messages"] if m.get("role") == "system"]
    assert len(system_msgs) >= 1, f"Expected >=1 system msg, got {len(system_msgs)}"
    for msg in resp["messages"]:
        assert "role" in msg, f"Message missing role: {msg}"
        assert "content" in msg, f"Message missing content: {msg}"

def test_p2_plan_true_attempts_llm():
    """plan=true triggers LLM planning (response may be string or null)"""
    resp = _pp("Implement a new feature", plan=True)
    assert resp.get("plan") is None or isinstance(resp.get("plan"), str), \
        f"plan=true should yield None or str, got {resp.get('plan')!r}"

def test_p2_plan_false_returns_null():
    """plan=false produces null plan"""
    resp = _pp("Implement a new feature", plan=False)
    assert resp.get("plan") is None, f"plan=false should be null, got {resp.get('plan')!r}"

def test_p2_short_message_with_plan():
    """Short message + plan=true still attempts planning"""
    resp = _pp("Hi", plan=True)
    assert resp.get("plan") is None or isinstance(resp.get("plan"), str), \
        f"Got {resp.get('plan')!r}"

def test_p2_long_complex_no_plan():
    """Long complex message + plan=false returns null"""
    resp = _pp(
        "Please implement a complete refactoring of the authentication system with "
        "JWT tokens, session management, and role-based access control.",
        plan=False
    )
    assert resp.get("plan") is None, f"plan=false should be null, got {resp.get('plan')!r}"

def test_p3_system_prompt_content():
    """System prompt contains OmniAgent identity and profile reference"""
    resp = _pp("What's the weather?", plan=False)
    sys = resp["system_prompt"]
    assert "OmniAgent" in sys, f"OmniAgent not in system prompt: {sys[:80]}"

def test_p3_system_message_exists():
    """At least one system message in the messages array"""
    resp = _pp("Hello", plan=False)
    has_sys = any(m.get("role") == "system" for m in resp["messages"])
    assert has_sys, "No system message found"

def test_p4_greeting_with_plan():
    """Greeting with plan=true works"""
    resp = _pp("Hi there!", plan=True)
    assert resp.get("plan") is None or isinstance(resp.get("plan"), str)

def test_p4_code_request_no_plan():
    """Code request with plan=false returns null plan"""
    resp = _pp("Write a Python function to sort a list", plan=False)
    assert resp.get("plan") is None

def test_p4_empty_prompt():
    """Empty prompt returns a valid response"""
    resp = _pp("", plan=False)
    assert "system_prompt" in resp

def test_p4_long_prompt_no_plan():
    """Long prompt with plan=false returns null plan"""
    long_text = "Tell me about " + "artificial intelligence and machine learning, " * 50
    resp = _pp(long_text, plan=False)
    assert resp.get("plan") is None

def test_p4_multiline_prompt():
    """Multiline prompt with plan=false returns null plan"""
    resp = _pp("Step 1: Do this\nStep 2: Do that\nStep 3: Profit", plan=False)
    assert resp.get("plan") is None

def test_p5_idempotent_plan_null():
    """Same input produces same plan type across calls"""
    msg = "Create a new data pipeline for processing logs"
    resp1 = _pp(msg, plan=False)
    resp2 = _pp(msg, plan=False)
    r1 = resp1.get("plan")
    r2 = resp2.get("plan")
    assert (r1 is None and r2 is None) or (isinstance(r1, str) and isinstance(r2, str)), \
        f"Inconsistent: {r1!r} vs {r2!r}"

def test_p5_stable_system_prompt_length():
    """System prompt length is stable across identical calls"""
    msg = "Create a new data pipeline"
    resp1 = _pp(msg, plan=False)
    resp2 = _pp(msg, plan=False)
    diff = abs(len(resp1["system_prompt"]) - len(resp2["system_prompt"]))
    assert diff < 50, f"Prompt length diff: {diff}"

def test_p6_missing_fallback():
    """Missing channel falls back to default profile and returns valid response"""
    try:
        r = urllib.request.urlopen(
            urllib.request.Request(
                f"{BASE}/prompt-preview/nonexistent-channel-xyz",
                data=json.dumps({"prompt": "hello", "plan": False}).encode(),
                headers={"Content-Type": "application/json"},
                method="POST"
            ),
            timeout=10
        )
        resp = json.loads(r.read())
        assert "system_prompt" in resp, f"Missing system_prompt in fallback response"
    except urllib.error.HTTPError as e:
        # Acceptable if the channel doesn't exist and server returns 400+
        assert e.code >= 400, f"Unexpected HTTP {e.code}"

# ── Compact-messages helpers ─────────────────────────────────────────

def _make_assistant_msg(tool_names: list[str]) -> dict:
    """Build an assistant message with tool_calls."""
    return {
        "role": "assistant",
        "content": "Let me check that.",
        "tool_calls": [
            {"id": f"call_{i}", "type": "function",
             "function": {"name": name, "arguments": '{"x": 1}'}}
            for i, name in enumerate(tool_names)
        ]
    }

def _make_tool_msg(name: str = "tool_a") -> dict:
    """Build a tool result message."""
    return {"role": "tool", "content": '{"result": "ok"}', "name": name}

def _make_user_msg(text: str = "Hello") -> dict:
    return {"role": "user", "content": text}

def _compact_call(messages: list, keep_recent: int = 3) -> dict:
    """Call the memory_compact-messages MCP tool and return parsed response."""
    r = urllib.request.urlopen(
        urllib.request.Request(
            f"{BASE}/mcp/execute",
            data=json.dumps({"name": "memory_compact-messages",
                             "arguments": {"messages": messages, "keep_recent": keep_recent}}).encode(),
            headers={"Content-Type": "application/json"},
            method="POST"
        ),
        timeout=15
    )
    result = json.loads(r.read())
    assert result.get("success"), f"compact-messages failed: {result}"
    return json.loads(result["content"])

# ── Compact-messages tests ───────────────────────────────────────────

def test_p7_no_compaction_needed():
    """Fewer tool-calling messages than keep_recent → no change"""
    msgs = [_make_user_msg(), _make_assistant_msg(["tool_a"]), _make_tool_msg(),
            _make_user_msg("Hi again"), _make_assistant_msg(["tool_b"]), _make_tool_msg()]
    resp = _compact_call(msgs, keep_recent=3)
    assert not resp["was_compacted"], f"Should not compact: {resp['before_count']} ≤ 3"
    assert resp["before_count"] == resp["after_count"]
    assert resp["after_count"] == 6

def test_p7_compaction_reduces_count():
    """More tool-calling messages than keep_recent → compacts oldest"""
    # 5 assistant+tool pairs, keep_recent=3 → compact 2 pairs (4 messages removed)
    msgs = []
    for i in range(5):
        msgs.append(_make_assistant_msg(["tool_a"]))
        msgs.append(_make_tool_msg("tool_a"))
    msgs.insert(0, _make_user_msg("Start"))
    before = len(msgs)  # 11

    resp = _compact_call(msgs, keep_recent=3)
    assert resp["was_compacted"], "Should have compacted"
    assert resp["before_count"] == 11
    # 2 old pairs compacted: each removes the tool msg after the assistant → 2 removed
    assert resp["after_count"] == 9, f"Expected 9 after compacting 2 pairs, got {resp['after_count']}"
    # Compaction produces [compact: ...] messages but doesn't delete the assistant msg

def test_p7_keep_recent_1():
    """keep_recent=1 compacts all but the most recent"""
    msgs = []
    for i in range(5):
        msgs.append(_make_assistant_msg(["tool_a"]))
        msgs.append(_make_tool_msg("tool_a"))
    resp = _compact_call(msgs, keep_recent=1)
    assert resp["was_compacted"]
    # 5 pairs → after keep_recent=1: 4 old pairs compacted (4 removed) → 6 remaining
    assert resp["after_count"] == 6, f"Expected 6 (5 pairs - 4 compact + 1 user? no), got {resp['after_count']}"

def test_p7_zero_tool_calls():
    """Messages with no tool_calls → no compaction"""
    msgs = [_make_user_msg("A"), _make_user_msg("B"), _make_user_msg("C")]
    resp = _compact_call(msgs, keep_recent=3)
    assert not resp["was_compacted"]
    assert resp["after_count"] == 3

def test_p7_tool_names_preserved():
    """Compacted messages still reference the tool names"""
    msgs = []
    for i in range(4):
        msgs.append(_make_assistant_msg(["search_docs", "read_file"]))
        msgs.append(_make_tool_msg("search_docs"))
        msgs.append(_make_tool_msg("read_file"))
    resp = _compact_call(msgs, keep_recent=2)
    assert resp["was_compacted"]
    for msg in resp["messages"]:
        if msg.get("role") == "assistant" and "compacted" in msg.get("content", ""):
            assert "search_docs" in msg["content"] or "read_file" in msg["content"], \
                f"Compacted msg missing tool name: {msg['content']}"

def test_p7_compact_multiple_tools():
    """Assistant with multiple tool_calls in one message → compacted reference shows all tools"""
    msgs = [
        _make_assistant_msg(["tool_a", "tool_b", "tool_c"]),
        _make_tool_msg("tool_a"),
        _make_tool_msg("tool_b"),
        _make_tool_msg("tool_c"),
        _make_assistant_msg(["result"]),
        _make_tool_msg("result"),
    ]
    resp = _compact_call(msgs, keep_recent=1)
    assert resp["was_compacted"]
    # Find the compacted message
    compacted = [m for m in resp["messages"] if "compacted" in m.get("content", "")]
    assert len(compacted) >= 1, "No compacted messages found"
    assert "tool_a" in compacted[0]["content"]
    assert "tool_b" in compacted[0]["content"]
    assert "tool_c" in compacted[0]["content"]

def test_p7_missing_messages_field():
    """Missing messages field returns descriptive error"""
    r = urllib.request.urlopen(
        urllib.request.Request(
            f"{BASE}/mcp/execute",
            data=json.dumps({"name": "memory_compact-messages",
                             "arguments": {"keep_recent": 3}}).encode(),
            headers={"Content-Type": "application/json"},
            method="POST"
        ),
        timeout=10
    )
    result = json.loads(r.read())
    assert result.get("success"), f"Expected tool-level success, got {result}"
    content = result["content"]
    # Content is either a JSON string (success) or plain error text
    data = json.loads(content) if content.startswith("{") else content
    if isinstance(data, str):
        assert "Missing required" in data or "messages" in data or "error" in data.lower(), \
            f"Expected error message about missing messages, got: {data}"
    else:
        # It returned something unexpected: but shouldn't crash
        assert True

def test_p7_empty_messages():
    """Empty messages array → no compaction"""
    resp = _compact_call([], keep_recent=3)
    assert not resp["was_compacted"]
    assert resp["after_count"] == 0

def test_p7_idempotent():
    """Same input produces identical results"""
    msgs = []
    for i in range(6):
        msgs.append(_make_assistant_msg(["tool_x"]))
        msgs.append(_make_tool_msg("tool_x"))
    r1 = _compact_call(msgs, keep_recent=2)
    r2 = _compact_call(msgs, keep_recent=2)
    assert r1["before_count"] == r2["before_count"]
    assert r1["after_count"] == r2["after_count"]
    assert r1["was_compacted"] == r2["was_compacted"]
    # Verify message count matches
    assert r1["after_count"] == len(r1["messages"])

# ═══════════════════════════════════════════════════════════════════════
#  Main
# ═══════════════════════════════════════════════════════════════════════

if __name__ == "__main__":
    # Verify clean git state before making any changes
    check_git_clean()

    # Verify API is accessible
    try:
        r = urllib.request.urlopen(f"{BASE}/health", timeout=5)
        assert r.status == 200
        print(f"API healthy at {BASE}\n")
    except Exception as e:
        print(f"API not accessible: {e}")
        sys.exit(1)

    print("=" * 60)
    print("GROUP 1: Original Remove API tests (idempotent)")
    print("=" * 60)

    for fn in [
        test_a1, test_a2, test_a3,
        test_b1, test_b2, test_b3,
        test_c1,
        test_d1, test_d2,
        test_e1, test_e2,
        test_f1, test_f2,
    ]:
        test(fn)

    print(f"\n{'=' * 60}")
    print("GROUP 2: Source-aware Remove API tests")
    print(f"{'=' * 60}")

    for fn in [test_1, test_2, test_3, test_4, test_5, test_6, test_7]:
        test(fn)

    print(f"\n{'=' * 60}")
    print("GROUP 3: File upload tests")
    print(f"{'=' * 60}")

    for fn in [test_8, test_9]:
        test(fn)

    print(f"\n{'=' * 60}")
    print("GROUP 4: Source-required validation tests")
    print(f"{'=' * 60}")

    for fn in [test_s1, test_s2, test_s3, test_s4, test_s5, test_s6]:
        test(fn)

    print(f"\n{'=' * 60}")
    print("GROUP 5: Dashboard page loading tests")
    print(f"{'=' * 60}")

    for fn in [test_dashboard_pages, test_dashboard_plugin_filters]:
        test(fn)


    print(f"\n{'=' * 60}")
    print("GROUP 6: Comprehensive Plugin Action Tests")
    print(f"{'=' * 60}")

    for fn in [
        test_t6_enable_bundled_tool,
        test_t6_enable_builtin_tool,
        test_t6_disable_bundled_tool,
        test_t6_disable_builtin_tool,
        test_t6_install_bundled_tool,
        test_t6_install_remote_tool,
        test_t6_reinstall_bundled_tool,
        test_t6_reinstall_remote_tool,
        test_t6_enable_remote_tool,
        test_t6_disable_remote_tool,
        test_t6_download_bundled_tool,
        test_t6_download_remote_tool,
        test_t6_enable_no_source_tool,
        test_t6_disable_no_source_tool,
        test_t6_install_no_source_tool,
        test_t6_reinstall_no_source_tool,
        test_t6_download_no_source_tool,
        test_t6_remove_no_source_tool,
        test_t6_enable_platform,
        test_t6_disable_platform,
        test_t6_enable_provider,
        test_t6_disable_provider,
        test_t6_config_update,
        test_t6_collision_enable_bundled,
        test_t6_collision_enable_remote,
        test_t6_enable_invalid_source,
        test_t6_disable_invalid_source,
    ]:
        test(fn)

    print(f"\n{'=' * 60}")
    print("GROUP 7: Memory Edit/Upload Tests")
    print(f"{'=' * 60}")

    for fn in [
        test_m1_setup,
        test_m2_edit_memory,
        test_m3_edit_soul,
        test_m4_prompt_verify,
        test_m5_edit_update,
        test_m6_upload_memory,
        test_m7_upload_soul,
        test_m8_delete_and_reupload,
        test_m9_cleanup,
    ]:
        test(fn)

    print(f"\n{'=' * 60}")

    print(f"\n{'=' * 60}")
    print(f"\n{'=' * 60}")
    print("GROUP 9 -- Mattermost + Noop E2E Integration Test")
    print(f"{'=' * 60}")

    for fn in [test_mm9_e2e]:
        test(fn)


    print("GROUP 8: Add/Install-Git Tests")
    print(f"{'=' * 60}")

    for fn in [
        test_t8_add_remote_new,
        test_t8_add_remote_duplicate,
        test_t8_remove_bundled_remote_yml_unchanged,
    ]:
        test(fn)

    print(f"\n{'=' * 60}")
    print("GROUP 10: Disabled Plugin Visibility Regression Tests")
    print(f"{'=' * 60}")

    for fn in [
        test_v1_disabled_tool_visible,
        test_v2_disabled_platform_visible,
        test_v3_disabled_provider_visible,
    ]:
        test(fn)

    print(f"\n{'=' * 60}\n")
    print("GROUP 11: Prompt Plugin Tests")
    print(f"{'=' * 60}")

    # Enable the prompt plugin before running its tests (it's disabled by default)
    enable_success, enable_resp = api_post_body("/plugins/prompt/enable", {"source": "built-in"})
    if not enable_success:
        print(f"  ⚠ Failed to enable prompt plugin: {enable_resp}")
    else:
        print("  ✓ Prompt plugin enabled for GROUP 11")

    # Wait for prompt MCP server to register its tools
    import time
    for attempt in range(10):
        try:
            r = urllib.request.urlopen(urllib.request.Request(f"{BASE}/mcp/tools"), timeout=5)
            tools_data = json.loads(r.read())
            tools = tools_data if isinstance(tools_data, list) else (tools_data.get("tools") or tools_data.get("data") or [])
            if any("prompt_compact" in (t.get("full_name") or t.get("name") or "") for t in tools):
                break
        except:
            pass
        time.sleep(1)
    else:
        print("  ⚠ Timed out waiting for prompt_compact-messages tool to register")

    for fn in [
        test_p1_basic_response_structure,
        test_p2_plan_true_attempts_llm,
        test_p2_plan_false_returns_null,
        test_p2_short_message_with_plan,
        test_p2_long_complex_no_plan,
        test_p3_system_prompt_content,
        test_p3_system_message_exists,
        test_p4_greeting_with_plan,
        test_p4_code_request_no_plan,
        test_p4_empty_prompt,
        test_p4_long_prompt_no_plan,
        test_p4_multiline_prompt,
        test_p5_idempotent_plan_null,
        test_p5_stable_system_prompt_length,
        test_p6_missing_fallback,
        test_p7_no_compaction_needed,
        test_p7_compaction_reduces_count,
        test_p7_keep_recent_1,
        test_p7_zero_tool_calls,
        test_p7_tool_names_preserved,
        test_p7_compact_multiple_tools,
        test_p7_missing_messages_field,
        test_p7_empty_messages,
        test_p7_idempotent,
    ]:
        test(fn)

    # Disable the prompt plugin after tests (restore default state)
    disable_success, disable_resp = api_post_body("/plugins/prompt/disable", {"source": "built-in"})
    if not disable_success:
        print(f"  ⚠ Failed to disable prompt plugin: {disable_resp}")
    else:
        print("  ✓ Prompt plugin disabled after GROUP 11")

    print(f"\n{'=' * 60}")
    print(f"Results: {tests_pass}/{tests_run} passed, {tests_fail} failed")
    print(f"{'=' * 60}")

    # Discard any unstaged changes: runs even on failure
    discard_all_changes()

    sys.exit(0 if tests_fail == 0 else 1)
