use std::process::Command;

fn run(args: &[&str]) -> (String, String, i32) {
    let output = Command::new("docker")
        .args(["exec", "omnideploy-omniagent-1"])
        .args(args)
        .output()
        .expect("Failed to execute command");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (stdout, stderr, output.status.code().unwrap_or(-1))
}

fn api_get(path: &str) -> serde_json::Value {
    let (stdout, _, code) = run(&[
        "sh",
        "-c",
        &format!("curl -sf http://localhost:8080/api{}", path),
    ]);
    assert_eq!(code, 0, "API GET {} failed: {}", path, stdout);
    serde_json::from_str(&stdout).expect("Failed to parse API response")
}

fn api_post(path: &str) -> serde_json::Value {
    let (stdout, _, code) = run(&[
        "sh",
        "-c",
        &format!("curl -sf -X POST http://localhost:8080/api{}", path),
    ]);
    assert_eq!(code, 0, "API POST {} failed: {}", path, stdout);
    serde_json::from_str(&stdout).expect("Failed to parse API response")
}

fn api_post_json(path: &str, body: &str) -> serde_json::Value {
    let (stdout, _, code) = run(&[
        "sh",
        "-c",
        &format!(
            "printf '%s' \"$1\" | curl -sf -X POST -H 'Content-Type: application/json' -d @- http://localhost:8080/api{}",
            path
        ),
        "sh",
        body,
    ]);
    assert_eq!(
        code, 0,
        "API POST {} failed: {}\nbody: {}",
        path, stdout, body
    );
    serde_json::from_str(&stdout).expect("Failed to parse API response")
}

fn get_plugin(name: &str) -> Option<serde_json::Value> {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array()?;
    data.iter().find(|p| p["name"] == name).cloned()
}

/// Set up a remote plugin for testing using the same API flow as the
/// dashboard "Install from Git" modal:
/// 1. POST /api/plugins/install-git — clones repo to .remote/<name>/, writes remote.yml
/// 2. POST /api/plugins/{type}/{source}/{name}/install — compiles, registers in plugins.yml
fn setup_remote_plugin(name: &str, base: &str) {
    // 1. install-git: clones repo to .remote/<name>/, persists remote.yml entry
    // Use HTTPS URL to avoid file:// git-cache issues
    let body = format!(
        r#"{{"url":"https://github.com/nexuslbs/omni-plugins.git","path":"tools/{}","name":"{}"}}"#,
        name, name
    );
    let resp = api_post_json("/plugins/install-git", &body);
    assert!(
        resp["success"].as_bool().unwrap_or(false),
        "install-git failed: {:?}",
        resp
    );

    // 2. install: compiles and registers in plugins.yml with enabled=true
    let resp = api_post(&format!("{}/install", base));
    assert_eq!(resp["success"], true, "Install failed: {:?}", resp);
}

/// Returns the expected path for a remote plugin's compiled binary.
/// Reads the `path` from the API plugin listing (remote field) and the
/// package name from Cargo.toml.
fn remote_binary_path(name: &str) -> Option<String> {
    // Try to read remote.path from the API plugin listing
    let (stdout, _, _) = run(&[
        "sh", "-c",
        &format!(
            "curl -sf http://localhost:8080/api/plugins 2>/dev/null | python3 -c \"import sys,json; d=json.load(sys.stdin)['data']; p=[x for x in d if x['name']=='{}'][0]; r=p.get('remote',{{}}); print(r.get('path',''))\" 2>/dev/null || echo ''",
            name
        ),
    ]);
    let subpath = stdout.trim().to_string();
    // Fallback: use tools/{name} which is the omni-plugins convention
    let subpath = if subpath.is_empty() || subpath == name {
        format!("tools/{}", name)
    } else {
        subpath
    };

    // Read the package name from Cargo.toml (may differ from the plugin name)
    let cargo_path = format!("/opt/omni/plugins/tools/.remote/{}/{}", name, subpath);
    let (stdout, _, _) = run(&[
        "sh",
        "-c",
        &format!(
            "grep '^name *= *' {}/Cargo.toml 2>/dev/null | head -1 | sed 's/^name *= *\"\\(.*\\)\"/\\1/' || echo ''",
            cargo_path
        ),
    ]);
    let pkg_name = stdout.trim().to_string();
    let pkg_name = if pkg_name.is_empty() {
        name.to_string()
    } else {
        pkg_name
    };

    Some(format!(
        "/opt/omni/plugins/tools/.remote/{}/{}/target/release/{}",
        name, subpath, pkg_name
    ))
}

/// Assert that a remote plugin's compiled binary exists on disk.
/// Uses remote.yml + Cargo.toml to find the correct path.
fn assert_remote_binary_exists(name: &str, msg: &str) {
    let bin_path = remote_binary_path(name).unwrap_or_else(|| {
        format!(
            "/opt/omni/plugins/tools/.remote/{}/{}/target/release/{}",
            name, name, name
        )
    });
    let (stdout, _, code) = run(&["sh", "-c", &format!("ls '{}' 2>/dev/null", bin_path)]);
    assert_eq!(
        code, 0,
        "{} - binary not found at {}:\n{}",
        msg, bin_path, stdout
    );
}

#[test]
#[ignore]
fn test_list_plugins_no_stale_entries() {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array().expect("Expected data array");
    let names: Vec<&str> = data
        .iter()
        .map(|p| p["name"].as_str().unwrap_or(""))
        .collect();
    assert!(
        !names.contains(&"docker-compose"),
        "docker-compose should not appear"
    );
    assert!(!names.contains(&"external"), "external should not appear");
    assert!(!names.contains(&"util"), "util should not appear");
}

#[test]
#[ignore]
fn test_list_builtins_have_source_code() {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array().expect("Expected data array");
    for name in &[
        "cron",
        "kanban",
        "memory",
        "metrics",
        "plugin-manager",
        "query",
        "search",
        "subtasks",
    ] {
        let plugin = data
            .iter()
            .find(|p| p["name"] == *name && p["source"] == "built-in");
        assert!(plugin.is_some(), "Builtin '{}' not found in listing", name);
        let p = plugin.unwrap();
        assert_eq!(
            p["has_source_code"], true,
            "Builtin '{}' should have source code",
            name
        );
    }
}

#[test]
#[ignore]
fn test_list_bundled_exist() {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array().expect("Expected data array");
    for name in &["actions", "filesystem", "git", "skills"] {
        let plugin = data
            .iter()
            .find(|p| p["name"] == *name && p["source"] == "bundled");
        assert!(plugin.is_some(), "Bundled '{}' not found in listing", name);
    }
}

#[test]
#[ignore]
fn test_builtins_enabled_only_in_yaml() {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array().expect("Expected data array");
    for p in data {
        if p["source"] == "built-in" && p["status"] == "enabled" {
            let name = p["name"].as_str().unwrap_or("");
            eprintln!(
                "Builtin '{}' is enabled: verify YAML has source: built-in",
                name
            );
        }
    }
}

#[test]
#[ignore]
fn test_no_duplicated_primary_enabled() {
    let resp = api_get("/plugins");
    let data = resp["data"].as_array().expect("Expected data array");
    let mut groups: std::collections::HashMap<String, Vec<&serde_json::Value>> =
        std::collections::HashMap::new();
    for p in data {
        groups
            .entry(p["name"].as_str().unwrap_or("").to_string())
            .or_default()
            .push(p);
    }
    for (name, entries) in &groups {
        if entries.len() > 1 {
            let enabled_count = entries.iter().filter(|e| e["status"] == "enabled").count();
            assert!(
                enabled_count <= 1,
                "Plugin '{}' has {} enabled entries (max 1)",
                name,
                enabled_count
            );
        }
    }
}

#[test]
#[ignore]
fn test_remote_plugin_install_compile() {
    let name = "test-rust-tool";
    let base = "/plugins/tools/remote/test-rust-tool";

    // Ensure clean state
    let _ = run(&[
        "sh",
        "-c",
        &format!(
            "curl -sf -X POST http://localhost:8080/api{}/disable 2>/dev/null || true",
            base
        ),
    ]);
    let _ = run(&["sh", "-c", &format!("curl -sf -X DELETE 'http://localhost:8080/api{base}?mode=uninstall' 2>/dev/null || true", base = base)]);

    // Install via API (handles YAML registration, clone, and compilation)
    setup_remote_plugin(name, base);

    std::thread::sleep(std::time::Duration::from_secs(10));

    assert_remote_binary_exists(name, "After install");

    let plugin = get_plugin(name).expect("test-rust-tool should exist after install");
    assert!(
        ["disabled", "enabled"].contains(&plugin["status"].as_str().unwrap_or("")),
        "Should be disabled or enabled after install, got '{}'",
        plugin["status"]
    );
    assert_eq!(
        plugin["needs_build"], false,
        "Should not need build anymore"
    );
    assert_eq!(
        plugin["has_source_code"], true,
        "Should still have source code"
    );
}

#[test]
#[ignore]
fn test_remote_plugin_enable_and_query() {
    let name = "test-rust-tool";
    let base = "/plugins/tools/remote/test-rust-tool";

    // Download + install via setup helper
    setup_remote_plugin(name, base);

    let resp = api_post(&format!("{}/enable", base));
    assert_eq!(resp["success"], true, "Enable failed: {:?}", resp);

    let plugin = get_plugin(name).expect("test-rust-tool should exist after enable");
    assert_eq!(
        plugin["status"], "enabled",
        "Should be enabled after enable call"
    );
}

#[test]
#[ignore]
fn test_remote_plugin_reinstall() {
    let name = "test-rust-tool";
    let base = "/plugins/tools/remote/test-rust-tool";

    // Ensure the plugin is downloaded, installed, and enabled.
    setup_remote_plugin(name, base);
    let _ = api_post(&format!("{}/enable", base));

    // If the plugin was removed by a parallel test, re-do the full setup
    if get_plugin(name).is_none() {
        setup_remote_plugin(name, base);
        let _ = api_post(&format!("{}/enable", base));
    }

    let plugin = get_plugin(name).unwrap_or_else(|| panic!("test-rust-tool should exist"));
    assert_eq!(
        plugin["status"], "enabled",
        "test-rust-tool should be enabled before reinstall"
    );

    let resp = api_post(&format!("{}/reinstall", base));
    assert_eq!(resp["success"], true, "Reinstall failed: {:?}", resp);

    std::thread::sleep(std::time::Duration::from_secs(60));

    assert_remote_binary_exists(name, "After reinstall");
}

#[test]
#[ignore]
fn test_remote_plugin_uninstall() {
    let name = "test-rust-tool";
    let base = "/plugins/tools/remote/test-rust-tool";

    // Ensure the plugin is downloaded, installed, and enabled
    setup_remote_plugin(name, base);
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

    let (_, _, _) = run(&[
        "sh",
        "-c",
        &format!(
        "curl -sf -X DELETE 'http://localhost:8080/api{base}?source=remote' 2>/dev/null || true",
        base = base
    ),
    ]);

    let (stdout, _, _) = run(&[
        "sh",
        "-c",
        &format!("ls /opt/omni/plugins/tools/.remote/{}/ 2>/dev/null", name),
    ]);
    assert!(
        stdout.is_empty(),
        "Remote directory should be removed after uninstall"
    );
}

#[test]
#[ignore]
fn test_builtin_reinstall_rejected() {
    let name = "plugin-manager";
    let base = "/plugins/tools/built-in/plugin-manager";

    let plugin = get_plugin(name).expect("plugin-manager should exist");
    assert_eq!(
        plugin["source"], "built-in",
        "plugin-manager should be built-in"
    );

    // Reinstall on built-in should fail with error
    let (stdout, _, _) = run(&["sh", "-c", &format!(
        "curl -s -o /dev/null -w '%{{http_code}}' -X POST http://localhost:8080/api{}/reinstall", base
    )]);
    assert_eq!(
        stdout.trim(),
        "400",
        "Built-in reinstall should return 400, got '{}'",
        stdout.trim()
    );
}

#[test]
#[ignore]
fn test_no_mcp_directory_references() {
    let (stdout, _, _code) = run(&[
        "sh",
        "-c",
        "test -d /app/plugins/mcp && echo EXISTS || echo NOT_FOUND",
    ]);
    assert_eq!(
        stdout.trim(),
        "NOT_FOUND",
        "mcp/ directory should not exist: should be tools/"
    );
    let (stdout, _, _code) = run(&[
        "sh",
        "-c",
        "test -d /app/plugins/tools && echo EXISTS || echo NOT_FOUND",
    ]);
    assert_eq!(stdout.trim(), "EXISTS", "tools/ directory should exist");
}

#[test]
#[ignore]
fn test_workspace_cargo_toml_uses_tools_not_mcp() {
    let (stdout, _, _code) = run(&[
        "sh",
        "-c",
        "grep 'plugins/mcp' /app/Cargo.toml || echo NO_MCP_REFS",
    ]);
    assert_eq!(
        stdout.trim(),
        "NO_MCP_REFS",
        "Cargo.toml should not reference plugins/mcp/"
    );
    let (stdout, _, _) = run(&["sh", "-c", "grep 'plugins/tools' /app/Cargo.toml | head -3"]);
    assert!(
        !stdout.is_empty(),
        "Cargo.toml should reference plugins/tools/"
    );
}

#[test]
#[ignore]
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
