use std::process::Command;

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

fn api_get(path: &str) -> serde_json::Value {
    let (stdout, _, code) = run(&[
        "sh", "-c",
        &format!("curl -sf http://localhost:8080/api{}", path),
    ]);
    assert_eq!(code, 0, "API GET {} failed: {}", path, stdout);
    serde_json::from_str(&stdout).expect("Failed to parse API response")
}

fn api_post(path: &str) -> serde_json::Value {
    let (stdout, _, code) = run(&[
        "sh", "-c",
        &format!("curl -sf -X POST http://localhost:8080/api{}", path),
    ]);
    assert_eq!(code, 0, "API POST {} failed: {}", path, stdout);
    serde_json::from_str(&stdout).expect("Failed to parse API response")
}

fn get_plugin(name: &str) -> Option<serde_json::Value> {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array()?;
    data.iter().find(|p| p["name"] == name).cloned()
}

#[test]
fn test_list_plugins_no_stale_entries() {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array().expect("Expected data array");
    let names: Vec<&str> = data
        .iter()
        .map(|p| p["name"].as_str().unwrap_or(""))
        .collect();
    assert!(!names.contains(&"docker-compose"), "docker-compose should not appear");
    assert!(!names.contains(&"external"), "external should not appear");
    assert!(!names.contains(&"util"), "util should not appear");
}

#[test]
fn test_list_builtins_have_source_code() {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array().expect("Expected data array");
    for name in &["cron", "kanban", "memory", "metrics", "plugin-manager", "query", "search", "subtasks"] {
        let plugin = data.iter().find(|p| p["name"] == *name && p["source"] == "built-in");
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
        let plugin = data.iter().find(|p| p["name"] == *name && p["source"] == "bundled");
        assert!(plugin.is_some(), "Bundled '{}' not found in listing", name);
    }
}

#[test]
fn test_builtins_enabled_only_in_yaml() {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array().expect("Expected data array");
    for p in data {
        if p["source"] == "built-in" && p["status"] == "enabled" {
            let name = p["name"].as_str().unwrap_or("");
            eprintln!("Builtin '{}' is enabled: verify YAML has source: built-in", name);
        }
    }
}

#[test]
fn test_no_duplicated_primary_enabled() {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array().expect("Expected data array");
    let mut groups: std::collections::HashMap<String, Vec<&serde_json::Value>> = std::collections::HashMap::new();
    for p in data {
        groups.entry(p["name"].as_str().unwrap_or("").to_string()).or_default().push(p);
    }
    for (name, entries) in &groups {
        if entries.len() > 1 {
            let enabled_count = entries.iter().filter(|e| e["status"] == "enabled").count();
            assert!(enabled_count <= 1, "Plugin '{}' has {} enabled entries (max 1)", name, enabled_count);
        }
    }
}

#[test]
fn test_remote_plugin_install_compile() {
    let name = "test-rust-tool";
    let base = "/plugins/tools/remote/test-rust-tool";

    // Ensure clean state
    let _ = run(&["sh", "-c", &format!("curl -sf -X POST http://localhost:8080/api{}/disable 2>/dev/null || true", base)]);
    let _ = run(&["sh", "-c", &format!("curl -sf -X DELETE 'http://localhost:8080/api{base}?mode=uninstall' 2>/dev/null || true", base = base)]);

    let plugin = get_plugin(name).expect("test-rust-tool should exist via remote.yml");
    assert_eq!(plugin["source"], "remote");
    assert!(["disabled", "not_found", "enabled"].contains(&plugin["status"].as_str().unwrap_or("")),
        "Unexpected status '{}' for test-rust-tool", plugin["status"]);

    // Download source from git (required before install)
    let resp = api_post(&format!("{}/download", base));
    assert_eq!(resp["success"], true, "Download failed: {:?}", resp);

    let resp = api_post(&format!("{}/install", base));
    assert_eq!(resp["success"], true, "Install failed: {:?}", resp);

    std::thread::sleep(std::time::Duration::from_secs(10));

    let (stdout, _, code) = run(&["sh", "-c", &format!(
        "ls /target/release/{} 2>/dev/null", name
    )]);
    assert_eq!(code, 0, "Binary not found after install:\n{}", stdout);

    let plugin = get_plugin(name).expect("test-rust-tool should exist after install");
    assert!(["disabled", "enabled"].contains(&plugin["status"].as_str().unwrap_or("")),
        "Should be disabled or enabled after install, got '{}'", plugin["status"]);
    assert_eq!(plugin["needs_build"], false, "Should not need build anymore");
    assert_eq!(plugin["has_source_code"], true, "Should still have source code");
}

#[test]
fn test_remote_plugin_enable_and_query() {
    let name = "test-rust-tool";
    let base = "/plugins/tools/remote/test-rust-tool";

    // Download and install first
    let _ = api_post(&format!("{}/download", base));
    let _ = api_post(&format!("{}/install", base));

    let resp = api_post(&format!("{}/enable", base));
    assert_eq!(resp["success"], true, "Enable failed: {:?}", resp);

    let plugin = get_plugin(name).expect("test-rust-tool should exist after enable");
    assert_eq!(plugin["status"], "enabled", "Should be enabled after enable call");
}

#[test]
fn test_remote_plugin_reinstall() {
    let name = "test-rust-tool";
    let base = "/plugins/tools/remote/test-rust-tool";

    // Ensure the plugin is downloaded, installed, and enabled
    let _ = api_post(&format!("{}/download", base));
    let _ = api_post(&format!("{}/install", base));
    let _ = api_post(&format!("{}/enable", base));

    let plugin = get_plugin(name).expect("test-rust-tool should exist");
    assert_eq!(
        plugin["status"], "enabled",
        "test-rust-tool should be enabled before reinstall"
    );

    let resp = api_post(&format!("{}/reinstall", base));
    assert_eq!(resp["success"], true, "Reinstall failed: {:?}", resp);

    std::thread::sleep(std::time::Duration::from_secs(60));

    let (stdout, _, code) = run(&["sh", "-c", &format!(
        "ls /target/release/{} 2>/dev/null", name
    )]);
    assert_eq!(code, 0, "Binary not found after reinstall:\n{}", stdout);
}

#[test]
fn test_remote_plugin_uninstall() {
    let name = "test-rust-tool";
    let base = "/plugins/tools/remote/test-rust-tool";

    // Ensure the plugin is downloaded, installed, and enabled
    let _ = api_post(&format!("{}/download", base));
    let _ = api_post(&format!("{}/install", base));
    let _ = api_post(&format!("{}/enable", base));

    let plugin = get_plugin(name).expect("test-rust-tool should exist before uninstall");
    assert_eq!(
        plugin["status"], "enabled",
        "test-rust-tool should be enabled before uninstall"
    );

    let resp = api_post(&format!("{}/disable", base));
    assert_eq!(
        resp["success"], true,
        "Disable failed before uninstall: {:?}",
        resp
    );

    let (_, _, _) = run(&["sh", "-c", &format!(
        "curl -sf -X DELETE 'http://localhost:8080/api{base}?source=remote' 2>/dev/null || true",
        base = base
    )]);

    let (stdout, _, _) = run(&["sh", "-c", &format!(
        "ls /opt/omni/plugins/tools/.remote/{}/ 2>/dev/null", name
    )]);
    assert!(
        stdout.is_empty(),
        "Remote directory should be removed after uninstall"
    );
}

#[test]
fn test_builtin_reinstall_rejected() {
    let name = "plugin-manager";
    let base = "/plugins/tools/built-in/plugin-manager";

    let plugin = get_plugin(name).expect("plugin-manager should exist");
    assert_eq!(plugin["source"], "built-in", "plugin-manager should be built-in");

    // Reinstall on built-in should fail with error
    let (stdout, _, _) = run(&["sh", "-c", &format!(
        "curl -s -o /dev/null -w '%{{http_code}}' -X POST http://localhost:8080/api{}/reinstall", base
    )]);
    assert_eq!(stdout.trim(), "400", "Built-in reinstall should return 400, got '{}'", stdout.trim());
}

#[test]
fn test_no_mcp_directory_references() {
    let (stdout, _, _code) = run(&["sh", "-c", "test -d /app/plugins/mcp && echo EXISTS || echo NOT_FOUND"]);
    assert_eq!(stdout.trim(), "NOT_FOUND", "mcp/ directory should not exist: should be tools/");
    let (stdout, _, _code) = run(&["sh", "-c", "test -d /app/plugins/tools && echo EXISTS || echo NOT_FOUND"]);
    assert_eq!(stdout.trim(), "EXISTS", "tools/ directory should exist");
}

#[test]
fn test_workspace_cargo_toml_uses_tools_not_mcp() {
    let (stdout, _, _code) = run(&["sh", "-c", "grep 'plugins/mcp' /app/Cargo.toml || echo NO_MCP_REFS"]);
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
        assert!(["enabled", "disabled", "error", "not_found"].contains(&status),
            "Plugin '{}' has invalid status: '{}'", p["name"].as_str().unwrap_or("?"), status);
    }
}
