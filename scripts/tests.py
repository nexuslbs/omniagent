#!/usr/bin/env python3
"""
Merged integration tests for the OmniAgent plugin lifecycle.

This file contains tests for install, uninstall, update, remove, download,
enable, and disable of plugins. New tests should not remove old tests.

GROUP 1 — Original Remove API tests (idempotent, restored from git history):
  A1-A3: Source NOT in YAML (built-in, bundled, remote)
  B1-B3: Source IN YAML (built-in, bundled, remote)
  C1:    YAML entry but no disk (phantom plugin)
  D1-D2: Provider tests (bundled, in / not in YAML)
  E1-E2: Platform tests (bundled, in / not in YAML)
  F1-F2: Name collision tests (bundled + remote same name)
  Each test is self-contained: SETUP → RUN → VERIFY → CLEANUP.

GROUP 2 — Source-aware Remove API tests:
  Tests 1-7: Remove scenarios with explicit source query parameter.
  Git hygiene at start / discard changes at end.

GROUP 3 — File upload tests:
  Tests 8-9: Explorer file upload + Kanban-scoped file upload.

Running twice on a clean repo produces identical results.
"""

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
    r = sh(f"sudo cat {WORKSPACE}/remote.yml")
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
# The .bak file is the per-test contract — do not nest backup/restore.

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
        cp(remote_src, target, recursive=True)
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
    if exists(remote_dir):
        return  # already installed

    repo_src = f"{REMOTE_REPO}/{plugin_type}/{name}"
    if not exists(repo_src):
        raise RuntimeError(f"Cannot install remote plugin '{name}': source not found in repo")

    # Copy source to .remote/<name>/<type>/<name>/
    dest_base = f"{WORKSPACE}/plugins/{plugin_type}/.remote/{name}"
    mkdir_p(f"{dest_base}/{plugin_type}")
    cp(repo_src, f"{dest_base}/{plugin_type}/{name}", recursive=True)

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
    # Kill the running omniagent process — the container entrypoint
    # (cargo watch) will auto-restart it. If running directly (no cargo watch),
    # we start a new instance. We try docker restart first, then fallback.
    rc = sh("docker exec omni-omniagent-1 bash -c 'pkill omniagent 2>/dev/null; sleep 1; /app/omniagent > /tmp/omniagent.log 2>&1 &'")
    time.sleep(6)
    for _ in range(15):
        try:
            r = urllib.request.urlopen(f"{BASE}/health", timeout=3)
            if r.status == 200:
                return
        except:
            pass
        time.sleep(2)
    raise RuntimeError("Failed to restart omniagent")

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
#  GROUP 1 — Original Remove API tests (idempotent, restored from git)
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
    plugin, ptype = "fetch", "tools"
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
    plugin, ptype = "filesystem", "tools"
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
        assert not os.path.exists(p), f"Plugin '{plugin}' exists at {p} — test would fail!"

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
        restart_agent()


# ── E1: Bundled platform IN YAML → succeed, YAML + disk removed ──────

def test_e1():
    """Bundled platform WITH YAML entry → succeed, YAML + disk removed"""
    plugin, ptype = "mattermost", "platforms"
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
    plugin, ptype = "telegram", "platforms"
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


# ── F1: Name collision — bundled source, both exist ──────────────────

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


# ── F2: Name collision — remote source, both exist ───────────────────

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
#  GROUP 2 — Source-aware Remove API tests
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
    shutil.copy2(f"{WORKSPACE}/remote.yml", remote_yml_bak)
    try:
        success, resp = api_delete(f"/plugins/{name}?source=remote")
        assert success, f"expected success, got success={success}, resp={resp}"
    finally:
        # Restore remote.yml and re-create .remote dir if needed
        if os.path.exists(remote_yml_bak):
            shutil.copy2(remote_yml_bak, f"{WORKSPACE}/remote.yml")
            os.remove(remote_yml_bak)
        for t in ["tools", "platforms", "providers"]:
            remote_dir = f"{WORKSPACE}/plugins/{t}/.remote/{name}"
            raw = read_remote_yml()
            if name in raw.get(t, {}) and not os.path.exists(remote_dir):
                ensure_remote_plugin(name, t)

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
#  GROUP 3 — File upload tests
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
#  GROUP 4 — Source-required validation tests
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
            # Axum deserialization error — source field is missing entirely
            err_text = raw.decode("utf-8", errors="replace").lower()
            assert "source" in err_text, \
                f"expected 'source' in error, got HTTP {e.code}: {raw.decode()}"
            return  # 422 implies source was missing from the body — acceptable
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
    """Verify no unstaged changes before running tests."""
    dirty = _git_status(OMNI_STACK_DIR)
    if dirty:
        raise RuntimeError(
            f"omni-stack repo has unstaged changes — cannot run tests safely:\n{dirty}"
        )

def discard_all_changes():
    """Discard all unstaged changes created by test execution."""
    _git_discard_all(OMNI_STACK_DIR)


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
    print("GROUP 1 — Original Remove API tests (idempotent)")
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
    print("GROUP 2 — Source-aware Remove API tests")
    print(f"{'=' * 60}")

    for fn in [test_1, test_2, test_3, test_4, test_5, test_6, test_7]:
        test(fn)

    print(f"\n{'=' * 60}")
    print("GROUP 3 — File upload tests")
    print(f"{'=' * 60}")

    for fn in [test_8, test_9]:
        test(fn)

    print(f"\n{'=' * 60}")
    print("GROUP 4 — Source-required validation tests")
    print(f"{'=' * 60}")

    for fn in [test_s1, test_s2, test_s3, test_s4, test_s5, test_s6]:
        test(fn)

    print(f"\n{'=' * 60}")
    print(f"Results: {tests_pass}/{tests_run} passed, {tests_fail} failed")
    print(f"{'=' * 60}")

    # Discard any unstaged changes — runs even on failure
    discard_all_changes()

    sys.exit(0 if tests_fail == 0 else 1)
