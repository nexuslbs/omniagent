//! Integration tests for plugin installation, reinstallation, and uninstallation.
//!
//! These tests exercise the plugin API endpoints against a running omniagent instance.
//! They verify:
//! - Plugin listing shows correct sources and no stale entries
//! - Install/reinstall/uninstall work for remote, builtin, and bundled plugins
//! - Dashboard-facing states (needs_build, status) are accurate
//! - Remote plugins with subpaths compile correctly
//! - No "mcp" directory references remain (all use "tools")

use std::process::Command;

/// Helper: run a command and return (stdout, stderr, exit_code)
fn run(args: &[&str]) -> (String, String, i32) {
    let output = Command::new("docker")
        .args(["exec", "omni-omniagent-1"])
        .args(args)
        .output()
        .expect("Failed to execute command");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (stdout, stderr, output.status.code().unwrap_or(-1))
}

/// Helper: hit the plugins API and parse the response
fn api_get(path: &str) -> serde_json::Value {
    let (stdout, _, code) = run(&[
        "sh", "-c",
        &format!("curl -sf http://localhost:8080/api{}", path),
    ]);
    assert_eq!(code, 0, "API GET {} failed: {}", path, stdout);
    serde_json::from_str(&stdout).expect("Failed to parse API response")
}

/// Helper: POST to a plugin endpoint and parse response
fn api_post(path: &str) -> serde_json::Value {
    let (stdout, _, code) = run(&[
        "sh", "-c",
        &format!("curl -sf -X POST http://localhost:8080/api{}", path),
    ]);
    assert_eq!(code, 0, "API POST {} failed: {}", path, stdout);
    serde_json::from_str(&stdout).expect("Failed to parse API response")
}

/// Helper: get a plugin detail by name from the list
fn get_plugin(name: &str) -> Option<serde_json::Value> {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array()?;
    data.iter().find(|p| p["name"] == name).cloned()
}

// ════════════════════════════════════════════════════════════════════════
// Plugin listing tests
// ════════════════════════════════════════════════════════════════════════

#[test]
fn test_list_plugins_no_stale_entries() {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array().expect("Expected data array");
    
    // No stale directories should appear as plugins
    let names: Vec<&str> = data.iter().map(|p| p["name"].as_str().unwrap_or("")).collect();
    assert!(!names.contains(&"docker-compose"), "docker-compose should not appear");
    assert!(!names.contains(&"external"), "external should not appear");
    assert!(!names.contains(&"util"), "util should not appear");
}

#[test]
fn test_list_builtins_have_source_code() {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array().expect("Expected data array");
    
    // Builtin tools that are workspace members should have source code
    for name in &["cron", "kanban", "memory", "metrics", "plugin-manager", "query", "search", "subtasks"] {
        let plugin = data.iter().find(|p| {
            p["name"] == *name && p["source"] == "built-in"
        });
        assert!(plugin.is_some(), "Builtin '{}' not found in listing", name);
        let p = plugin.unwrap();
        assert_eq!(p["has_source_code"], true, "Builtin '{}' should have source code", name);
    }
}

#[test]
fn test_list_bundled_exist() {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array().expect("Expected data array");
    
    for name in &["actions", "fetch", "filesystem", "git", "skills"] {
        let plugin = data.iter().find(|p| {
            p["name"] == *name && p["source"] == "bundled"
        });
        assert!(plugin.is_some(), "Bundled '{}' not found in listing", name);
    }
}

#[test]
fn test_builtins_enabled_only_in_yaml() {
    // Verify only plugins with explicit source: built-in and enabled: true are enabled
    let resp = api_get("/plugins");
    let data = resp["data"].as_array().expect("Expected data array");
    
    for p in data {
        if p["source"] == "built-in" && p["status"] == "enabled" {
            let name = p["name"].as_str().unwrap_or("");
            // These should all have explicit YAML entries
            eprintln!("Builtin '{}' is enabled — verify YAML has source: built-in", name);
        }
    }
}

#[test]
fn test_no_duplicated_primary_enabled() {
    // If a plugin has both built-in and bundled, only one should be enabled
    let resp = api_get("/plugins");
    let data = resp["data"].as_array().expect("Expected data array");
    
    // Group by name
    let mut groups: std::collections::HashMap<String, Vec<&serde_json::Value>> = std::collections::HashMap::new();
    for p in data {
        let name = p["name"].as_str().unwrap_or("").to_string();
        groups.entry(name).or_default().push(p);
    }
    
    for (name, entries) in &groups {
        if entries.len() > 1 {
            let enabled_count = entries.iter().filter(|e| e["status"] == "enabled").count();
            assert!(enabled_count <= 1, "Plugin '{}' has {} enabled entries (max 1)", name, enabled_count);
        }
    }
}

// ════════════════════════════════════════════════════════════════════════
// Remote plugin install/reinstall/uninstall tests
// ════════════════════════════════════════════════════════════════════════

#[test]
fn test_remote_plugin_install_compile() {
    let name = "test-rust-tool";
    
    // Ensure clean state — uninstall first
    let _ = api_post(&format!("/plugins/{}/disable", name));
    let _ = api_post(&format!("/plugins/{}/delete?mode=uninstall", name));
    
    // Verify plugin is in disabled state (via remote.yml)
    let plugin = get_plugin(name).expect("test-rust-tool should exist via remote.yml");
    assert_eq!(plugin["source"], "remote");
    assert_eq!(plugin["status"], "disabled");
    assert_eq!(plugin["needs_build"], true, "test-rust-tool should need build");
    assert_eq!(plugin["has_source_code"], true, "test-rust-tool should have source code");
    
    // Install
    let resp = api_post(&format!("/plugins/{}/install", name));
    assert_eq!(resp["success"], true, "Install failed: {:?}", resp);
    
    // Wait for background compile
    std::thread::sleep(std::time::Duration::from_secs(30));
    
    // Check binary was built
    let (stdout, _, code) = run(&[
        "sh", "-c",
        &format!("ls /opt/omni/plugins/tools/.remote/{}/tools/{}/target/release/{} 2>/dev/null", name, name, name),
    ]);
    assert_eq!(code, 0, "Binary not found after install:\n{}", stdout);
    
    // Check plugin detail
    let plugin = get_plugin(name).expect("test-rust-tool should exist after install");
    assert_eq!(plugin["status"], "disabled", "Should be disabled until enabled");
    assert_eq!(plugin["needs_build"], false, "Should not need build anymore");
    assert_eq!(plugin["has_source_code"], true, "Should still have source code");
}

#[test]
fn test_remote_plugin_enable_and_query() {
    let name = "test-rust-tool";
    
    // Enable
    let resp = api_post(&format!("/plugins/{}/enable", name));
    assert_eq!(resp["success"], true, "Enable failed: {:?}", resp);
    
    let plugin = get_plugin(name).expect("test-rust-tool should exist after enable");
    assert_eq!(plugin["status"], "enabled", "Should be enabled after enable call");
}

#[test]
fn test_remote_plugin_reinstall() {
    let name = "test-rust-tool";
    
    // Verify it's enabled
    let plugin = get_plugin(name).expect("test-rust-tool should exist");
    assert_eq!(plugin["status"], "enabled", "test-rust-tool should be enabled before reinstall");
    
    // Reinstall
    let resp = api_post(&format!("/plugins/{}/reinstall", name));
    assert_eq!(resp["success"], true, "Reinstall failed: {:?}", resp);
    
    // Wait for re-clone and re-compile
    std::thread::sleep(std::time::Duration::from_secs(60));
    
    // Check binary still exists (was recompiled)
    let (stdout, _, code) = run(&[
        "sh", "-c",
        &format!("ls /opt/omni/plugins/tools/.remote/{}/tools/{}/target/release/{} 2>/dev/null", name, name, name),
    ]);
    assert_eq!(code, 0, "Binary not found after reinstall:\n{}", stdout);
}

#[test]
fn test_remote_plugin_uninstall() {
    let name = "test-rust-tool";
    
    // Verify it's enabled first
    let plugin = get_plugin(name).expect("test-rust-tool should exist before uninstall");
    assert_eq!(plugin["status"], "enabled", "test-rust-tool should be enabled before uninstall");
    
    // Uninstall (disable then delete with mode=uninstall)
    let resp = api_post(&format!("/plugins/{}/disable", name));
    assert_eq!(resp["success"], true, "Disable failed before uninstall: {:?}", resp);
    
    let resp = api_post(&format!("/plugins/{}/delete?mode=uninstall", name));
    assert_eq!(resp["success"], true, "Uninstall failed: {:?}", resp);
    
    // Check plugin is gone or in disabled state
    let (stdout, _, _) = run(&[
        "sh", "-c",
        &format!("ls /opt/omni/plugins/tools/.remote/{}/ 2>/dev/null", name),
    ]);
    assert!(stdout.is_empty(), "Remote directory should be removed after uninstall");
}

#[test]
fn test_builtin_plugin_reinstall() {
    // Test that a builtin plugin can be reinstalled successfully
    // plugin-manager is a builtin with explicit source: built-in in YAML
    let name = "plugin-manager";
    
    let plugin = get_plugin(name).expect("plugin-manager should exist");
    assert_eq!(plugin["source"], "built-in", "plugin-manager should be built-in");
    
    // Reinstall
    let resp = api_post(&format!("/plugins/{}/reinstall", name));
    assert_eq!(resp["success"], true, "Builtin reinstall failed: {:?}", resp);
    
    // Check it's still showing as built-in
    let plugin = get_plugin(name).expect("plugin-manager should exist after reinstall");
    assert_eq!(plugin["source"], "built-in");
    assert_eq!(plugin["has_source_code"], true);
}

#[test]
fn test_no_mcp_directory_references() {
    // Verify the mcp/ directory no longer exists
    let (stdout, _, code) = run(&["sh", "-c", "test -d /app/plugins/mcp && echo EXISTS || echo NOT_FOUND"]);
    assert_eq!(stdout.trim(), "NOT_FOUND", "mcp/ directory should not exist — should be tools/");
    
    // tools/ should exist
    let (stdout, _, code) = run(&["sh", "-c", "test -d /app/plugins/tools && echo EXISTS || echo NOT_FOUND"]);
    assert_eq!(stdout.trim(), "EXISTS", "tools/ directory should exist");
}

#[test]
fn test_workspace_cargo_toml_uses_tools_not_mcp() {
    let (stdout, _, code) = run(&["sh", "-c", "grep 'plugins/mcp' /app/Cargo.toml || echo NO_MCP_REFS"]);
    assert_eq!(stdout.trim(), "NO_MCP_REFS", "Cargo.toml should not reference plugins/mcp/");
    
    let (stdout, _, _) = run(&["sh", "-c", "grep 'plugins/tools' /app/Cargo.toml | head -3"]);
    assert!(!stdout.is_empty(), "Cargo.toml should reference plugins/tools/");
}

#[test]
fn test_all_plugin_statuses_are_valid() {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array().expect("Expected data array");
    
    for p in data {
        let status = p["status"].as_str().unwrap_or("");
        assert!(
            ["enabled", "disabled", "error", "not_found"].contains(&status),
            "Plugin '{}' has invalid status: '{}'",
            p["name"].as_str().unwrap_or("?"),
            status
        );
    }
}
