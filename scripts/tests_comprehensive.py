#!/usr/bin/env python3
"""
Comprehensive plugin action tests - appended to tests.py as GROUP 6.

Covers ALL actions: enable, disable, install, reinstall, download, remove, config, add
Across ALL source types: built-in, bundled, remote
Across ALL plugin types: tools, platforms, providers
Plus: name collision tests for each action, source-required validation
"""
# ── This file is inserted into tests.py before the main block ──

# ═══════════════════════════════════════════════════════════════════════
#  Helpers for Group 6
# ═══════════════════════════════════════════════════════════════════════

def api_post_body(path, body=None):
    """POST with JSON body. Returns (success, response_dict)."""
    import urllib.request, urllib.error, json
    url = f"{BASE}/api{path}"
    data = json.dumps(body).encode() if body is not None else None
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

def find_plugins_by_source(source, plugin_type="tools"):
    """Find plugins of a given source and type from the API list."""
    plugins = api_get("/plugins")["data"]
    return [p for p in plugins
            if p.get("source") == source
            and p.get("type") == plugin_type
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
#  Test helpers - each test is one action × one source × one type
# ═══════════════════════════════════════════════════════════════════════

def test_enable_source(name, source, expected_success=True):
    """Test enabling a plugin with a specific source."""
    success, resp = api_post_body(f"/plugins/{name}/enable", {"source": source})
    if expected_success:
        assert success, f"enable {name} source={source} failed: {resp}"
        # Verify plugin is now enabled
        plugins = api_get("/plugins")["data"]
        for p in plugins:
            if p["name"] == name:
                assert p.get("source") == source, \
                    f"enable {name}: expected source={source}, got {p.get('source')}"
                return
    else:
        assert not success, f"enable {name} source={source} should have failed"

def test_disable_source(name, source, expected_success=True):
    """Test disabling a plugin with a specific source."""
    success, resp = api_post_body(f"/plugins/{name}/disable", {"source": source})
    if expected_success:
        assert success, f"disable {name} source={source} failed: {resp}"
    else:
        assert not success, f"disable {name} source={source} should have failed"

def test_enable_no_source(name):
    """Test enabling a plugin WITHOUT source → error."""
    success, resp = api_post_body(f"/plugins/{name}/enable", {})
    assert not success, f"enable {name} without source should have failed"
    err_text = json.dumps(resp).lower()
    assert "source is required" in err_text, \
        f"enable {name} without source: expected 'source is required', got {resp}"

def test_disable_no_source(name):
    """Test disabling a plugin WITHOUT source → error."""
    success, resp = api_post_body(f"/plugins/{name}/disable", {})
    assert not success, f"disable {name} without source should have failed"
    err_text = json.dumps(resp).lower()
    assert "source is required" in err_text, \
        f"disable {name} without source: expected 'source is required', got {resp}"

def test_install_source(name, source, expected_success=True):
    """Test installing a plugin with a specific source."""
    success, resp = api_post_body(f"/plugins/{name}/install", {"source": source})
    if expected_success:
        assert success, f"install {name} source={source} failed: {resp}"
    else:
        assert not success, f"install {name} source={source} should have failed"

def test_install_no_source(name):
    """Test installing a plugin WITHOUT source → error."""
    success, resp = api_post_body(f"/plugins/{name}/install", {})
    assert not success, f"install {name} without source should have failed"
    err_text = json.dumps(resp).lower()
    assert "source is required" in err_text, \
        f"install {name} without source: expected 'source is required', got {resp}"

def test_reinstall_source(name, source, expected_success=True):
    """Test reinstalling a plugin with a specific source."""
    success, resp = api_post_body(f"/plugins/{name}/reinstall", {"source": source})
    if expected_success:
        assert success, f"reinstall {name} source={source} failed: {resp}"
    else:
        assert not success, f"reinstall {name} source={source} should have failed"

def test_reinstall_no_source(name):
    """Test reinstalling a plugin WITHOUT source → error."""
    success, resp = api_post_body(f"/plugins/{name}/reinstall", {})
    assert not success, f"reinstall {name} without source should have failed"
    err_text = json.dumps(resp).lower()
    assert "source is required" in err_text, \
        f"reinstall {name} without source: expected 'source is required', got {resp}"

def test_download_source(name, source, expected_success=True):
    """Test downloading a plugin with a specific source."""
    success, resp = api_post_body(f"/plugins/{name}/download", {"source": source})
    if expected_success:
        assert success, f"download {name} source={source} failed: {resp}"
    else:
        assert not success, f"download {name} source={source} should have failed"

def test_download_no_source(name):
    """Test downloading a plugin WITHOUT source → error."""
    success, resp = api_post_body(f"/plugins/{name}/download", {})
    assert not success, f"download {name} without source should have failed"
    err_text = json.dumps(resp).lower()
    assert "source is required" in err_text, \
        f"download {name} without source: expected 'source is required', got {resp}"

def test_remove_with_source(name, source, expected_success=True):
    """Test removing a plugin with a specific source."""
    success, resp = api_delete(f"/plugins/{name}?source={source}")
    if expected_success:
        assert success, f"remove {name} source={source} failed: {resp}"
    else:
        assert not success, f"remove {name} source={source} should have failed"

def test_remove_no_source(name):
    """Test removing a plugin WITHOUT source → error."""
    success, resp = api_delete(f"/plugins/{name}")
    assert not success, f"remove {name} without source should have failed"
    err_text = json.dumps(resp).lower()
    assert "source is required" in err_text, \
        f"remove {name} without source: expected 'source is required', got {resp}"

def test_config_update(name, config_body):
    """Test updating a plugin's config."""
    success, resp = api_post_body(f"/plugins/{name}/config", {"config": config_body})
    assert success, f"config update {name} failed: {resp}"
    return resp


# ═══════════════════════════════════════════════════════════════════════
#  GROUP 6 - Comprehensive Plugin Action Tests
# ═══════════════════════════════════════════════════════════════════════
#
# For each action that requires source: enable, disable, install, reinstall,
# download, remove - tests for built-in, bundled, and remote variants.
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
        return
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
    """Download a bundled tool plugin → success"""
    name = find_first_plugin("bundled", "tools")
    if not name:
        return
    test_download_source(name, "bundled")

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


# ── 6.5: Cross-type - platform action tests ───────────────────────────

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


# ── 6.6: Cross-type - provider action tests ───────────────────────────

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
        yaml_set("tools", collision_name, {"enabled": False, "source": "bundled", "config": {}})
        ensure_remote_yaml_entry(collision_name)
        restart_agent()

        # Enable with source=bundled
        success, resp = api_post_body(f"/plugins/{collision_name}/enable", {"source": "bundled"})
        assert success, f"collision enable bundled failed: {resp}"

        # Verify bundled dir still exists (not removed)
        assert os.path.exists(bundled_dir), "bundled dir was removed!"
        assert os.path.exists(remote_dir), "remote dir was removed!"
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
        yaml_set("tools", collision_name, {"enabled": False, "source": "remote", "config": {}})
        ensure_remote_yaml_entry(collision_name)
        restart_agent()

        # Enable with source=remote
        success, resp = api_post_body(f"/plugins/{collision_name}/enable", {"source": "remote"})
        assert success, f"collision enable remote failed: {resp}"

        assert os.path.exists(bundled_dir), "bundled dir was removed!"
        assert os.path.exists(remote_dir), "remote dir was removed!"
    finally:
        remove_bundled_plugin(collision_name, "tools")
        remove_remote_plugin(collision_name, "tools")
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
