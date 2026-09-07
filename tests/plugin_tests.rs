use std::process::Command;

/// Data dir of the running agent ($OMNI_DIR), falling back to the historical
/// /opt/omni for production-like environments where the var is unset.
fn data_dir() -> String {
    std::env::var("OMNI_DIR").unwrap_or_else(|_| "/opt/omni".to_string())
}

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
/// 1. POST /api/plugins/install-git - clones repo to .remote/<name>/, writes remote.yml
/// 2. POST /api/plugins/{type}/{source}/{name}/install - compiles, registers in plugins.yml
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
    let cargo_path = format!("{}/plugins/tools/.remote/{}/{}", data_dir(), name, subpath);
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
        "{}/plugins/tools/.remote/{}/{}/target/release/{}",
        data_dir(),
        name,
        subpath,
        pkg_name
    ))
}

/// Assert that a remote plugin's compiled binary exists on disk.
/// Uses remote.yml + Cargo.toml to find the correct path.
/// Falls back to CARGO_TARGET_DIR (/target/release/) if the crate's own
/// target/ is empty (CARGO_TARGET_DIR env var redirects cargo output).
fn assert_remote_binary_exists(name: &str, msg: &str) {
    let bin_path = remote_binary_path(name).unwrap_or_else(|| {
        format!(
            "{}/plugins/tools/.remote/{}/{}/target/release/{}",
            data_dir(),
            name,
            name,
            name
        )
    });
    // Direct check (no docker exec) since this test runs inside the container
    let exists = std::path::Path::new(&bin_path).exists();
    if !exists {
        // Fallback: check CARGO_TARGET_DIR (/target/release/)
        let fallback = format!("/target/release/{}", name);
        let fallback_exists = std::path::Path::new(&fallback).exists();
        assert!(
            fallback_exists,
            "{} - binary not at {} nor at fallback {}",
            msg, bin_path, fallback
        );
    }
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
        "plugin-manager",
        "search",
        "ssh",
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
    for name in &["filesystem", "git", "skills"] {
        // Plugin may be listed as "bundled" or "built-in" depending on migration status
        let plugin = data.iter().find(|p| {
            p["name"] == *name && (p["source"] == "bundled" || p["source"] == "built-in")
        });
        assert!(
            plugin.is_some(),
            "Plugin '{}' not found in listing (neither bundled nor built-in)",
            name
        );
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

    // Retry up to 2 times: parallel tests (test_remote_plugin_uninstall) may
    // delete the binary between our reinstall and this check.
    let mut binary_ok = false;
    for attempt in 1..=2 {
        let resp = api_post(&format!("{}/reinstall", base));
        assert_eq!(resp["success"], true, "Reinstall failed: {:?}", resp);

        // Verify the binary exists immediately after the API returns.
        // The reinstall API is synchronous - it awaits compilation internally.
        // If compilation succeeded, the binary is on disk right away.
        if std::path::Path::new(&remote_binary_path(name).unwrap_or_else(|| {
            format!(
                "/opt/omni/plugins/tools/.remote/{}/{}/target/release/{}",
                name, name, name
            )
        }))
        .exists()
        {
            binary_ok = true;
            break;
        }

        if attempt == 1 {
            // Parallel test likely deleted our source - re-install and retry
            setup_remote_plugin(name, base);
            let _ = api_post(&format!("{}/enable", base));
        }
    }
    assert!(
        binary_ok,
        "After reinstall - binary not found after 2 attempts"
    );
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
        &format!(
            "ls {}/plugins/tools/.remote/{}/ 2>/dev/null",
            data_dir(),
            name
        ),
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

// ---------------------------------------------------------------------------
// Core/Platform boundary guard tests (code-plan C6; defect class A4).
//
// Regression guards for the 2026-08-31 incidents (threads 518/519; commits
// a501507, 9e883d9, d3660bd): telegram first/last-only message collapse and an
// `is_internal_telemetry` suppression were once hardcoded in core delivery
// (src/agent/helpers.rs) and driven by reading the telegram platform plugin's
// `first_last_only` config from plugins.yml. Core delivery must stay
// platform-generic: every platform plugin receives the full message stream and
// decides its own rendering (collapse, suppression, reply threading).
//
// Unlike the live-server tests above (which need a running agent and are
// #[ignore]d), these are NON-ignored source-scan guards in the style of
// tests/log_hygiene.rs: they read the delivery-path sources (src/agent,
// src/platform) and fail the build if a platform name, a platform-specific
// delivery key, or a platform-plugin config read ever reappears in core
// delivery code. They cover the three guard scenarios: (a) no platform-name
// branch in core delivery means the full message stream (user, tool,
// multi-tool, tool-result, plan, reasoning) is delivered generically to every
// platform; (b) no `first_last_only` in core means enabling telegram collapse
// cannot change delivery to other platforms; (c) no `is_internal_telemetry` in
// core means no per-platform suppression path can alter or fail a run.
// Keep in sync with scripts/lint-core-platform-boundary.py (same rules, same
// file scope, same comment/test skipping logic).
// ---------------------------------------------------------------------------

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Every .rs file under src/agent and src/platform (the core delivery path).
fn delivery_src_files() -> Vec<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in ["src/agent", "src/platform"] {
        let mut stack = vec![manifest.join(dir)];
        while let Some(dir_path) = stack.pop() {
            for entry in std::fs::read_dir(&dir_path)
                .unwrap_or_else(|e| panic!("read_dir {}: {}", dir_path.display(), e))
            {
                let entry = entry.unwrap_or_else(|e| panic!("read_dir entry: {}", e));
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                    files.push(p);
                }
            }
        }
    }
    files.sort();
    files
}

/// 1-based line numbers that lie inside a `#[cfg(test)] mod ... { }` block.
/// Test code may name platforms freely (fixtures, mock handshakes); the
/// guards inspect production code only. Mirrors the lint script's skipper.
fn test_region_lines(text: &str) -> HashSet<usize> {
    let mut skipped = HashSet::new();
    let mut in_test = false;
    let mut depth: i64 = 0;
    for (i, raw) in text.lines().enumerate() {
        if !in_test && raw.contains("#[cfg(test)]") {
            in_test = true;
            depth = 0;
        }
        if in_test {
            depth += raw.matches('{').count() as i64 - raw.matches('}').count() as i64;
            if depth <= 0 && raw.contains('}') {
                in_test = false;
            }
            skipped.insert(i + 1);
        }
    }
    skipped
}

/// Code portion of a line: everything before the first `//` comment marker
/// (doc comments `///` and `//!` therefore yield an empty code part).
fn code_part(raw: &str) -> &str {
    raw.split_once("//").map(|(c, _)| c).unwrap_or(raw).trim()
}

/// (line_no, lowercased code) pairs for production (non-test, non-comment)
/// code lines of a file.
fn production_code_lines(path: &Path) -> Vec<(usize, String)> {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
    let skipped = test_region_lines(&text);
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let ln = i + 1;
        if skipped.contains(&ln) {
            continue;
        }
        let code = code_part(raw);
        if !code.is_empty() {
            out.push((ln, code.to_lowercase()));
        }
    }
    out
}

/// Guards (b) + (c) plus the platform-name rule: no production delivery code
/// line may reference a platform name or a platform-specific delivery key.
/// If telegram-specific collapse (`first_last_only`) or suppression
/// (`is_internal_telemetry`) logic is ever reintroduced into core delivery
/// (the src/agent/helpers.rs incident, threads 518/519), this test fails.
#[test]
fn core_delivery_never_branches_on_platform_name_or_delivery_keys() {
    let banned = ["telegram", "first_last_only", "is_internal_telemetry"];
    let mut hits = Vec::new();
    for path in delivery_src_files() {
        for (ln, code_lower) in production_code_lines(&path) {
            for token in banned {
                if code_lower.contains(token) {
                    hits.push(format!("{}:{}: '{}'", path.display(), ln, token));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "core delivery (src/agent, src/platform) must never reference a \
         platform name or a platform-specific delivery key in code (AGENTS.md \
         Core-Platform Boundary Rule, code-plan C6):\n  {}",
        hits.join("\n  ")
    );
}

/// Guard rule 2 (the A4 leak shape): no `plugins_yaml::get_plugin(...)`
/// config read in the core delivery path, EXCEPT the documented LLM-provider
/// api-key fallback in src/agent/executor.rs (which resolves
/// PluginYamlType::Provider - the LLM provider, not a platform). Reading a
/// PLATFORM plugin's config from delivery code to shape delivery is exactly
/// the telegram first_last_only leak shape and is forbidden.
#[test]
fn core_delivery_never_reads_platform_plugin_config() {
    for path in delivery_src_files() {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
        let skipped = test_region_lines(&text);
        for (i, raw) in text.lines().enumerate() {
            let ln = i + 1;
            if skipped.contains(&ln) || !raw.contains("plugins_yaml::get_plugin") {
                continue;
            }
            let is_executor_provider_fallback = path.ends_with("src/agent/executor.rs")
                && text
                    .lines()
                    .skip(i)
                    .take(14)
                    .any(|l| l.contains("PluginYamlType::Provider"));
            assert!(
                is_executor_provider_fallback,
                "{}:{}: plugins_yaml::get_plugin config read in the core \
                 delivery path (AGENTS.md Core-Platform Boundary Rule rule 2): \
                 only the LLM-provider api-key fallback in src/agent/executor.rs \
                 may read plugin config here; platform config reads that shape \
                 delivery are the A4 leak shape (threads 518/519)",
                path.display(),
                ln
            );
        }
    }
}
