//! mcp-server-filesystem: standalone MCP server for local file operations.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: filesystem__read (char paging; line-numbered output + line paging via lines=true),
//! filesystem__write, filesystem__list, filesystem__search, filesystem__info,
//! filesystem__grep (ripgrep-backed recursive content search, caps + spill to file),
//! filesystem__str_replace, filesystem__insert, filesystem__apply_patch (precise, reviewable
//! file-edit primitives)
//!
//! SANDBOX: only WRITE operations are confined to the configured
//! `workspace_dir` (default `/opt/workspace`) and its subdirectories.
//! Reads, lists, searches, and metadata lookups are allowed anywhere -
//! reading is side-effect free, and the agent legitimately needs to inspect
//! files outside the workspace (configs, wiki, credentials paths, ...).

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use mcp_server_util::*;
use parking_lot::Mutex;
use regex::RegexBuilder;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};
use std::sync::Arc;

const DEFAULT_WORKSPACE_DIR: &str = "/opt/workspace";

/// Resolve the sandbox workspace dir: configured value or `/opt/workspace`.
fn resolve_workspace_dir(cfg_ws: &str) -> String {
    if cfg_ws.is_empty() {
        DEFAULT_WORKSPACE_DIR.to_string()
    } else {
        cfg_ws.to_string()
    }
}

/// Normalize a path, resolving `.` / `..` lexically (no filesystem access).
fn normalize_path(p: &Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// WS-6: resolve the OMNI_DIR root (config value -> env OMNI_DIR -> /opt/omni).
fn resolve_omni_dir(cfg_omni: &str) -> String {
    if !cfg_omni.is_empty() {
        cfg_omni.to_string()
    } else {
        std::env::var("OMNI_DIR").unwrap_or_else(|_| "/opt/omni".to_string())
    }
}

/// WS-6: allowed write roots - the workspace dir (always) plus the OMNI_DIR
/// subdirs enabled by config. `write_omni_all` supersedes the three subdir
/// toggles.
fn allowed_write_roots(cfg: &Config) -> Vec<String> {
    let mut roots = vec![resolve_workspace_dir(&cfg.workspace_dir)];
    let omni = resolve_omni_dir(&cfg.omni_dir);
    if cfg.write_omni_all {
        roots.push(omni);
    } else {
        if cfg.write_profiles {
            roots.push(format!("{omni}/profiles"));
        }
        if cfg.write_data {
            roots.push(format!("{omni}/data"));
        }
        if cfg.write_plugins {
            roots.push(format!("{omni}/plugins"));
        }
    }
    roots
}

/// WS-6: replaces the single-root `restrict_to_workspace` for writes. A write
/// path must normalize INSIDE at least one allowed root (workspace dir always;
/// OMNI_DIR or its profiles/data/plugins subdirs per config). Relative paths
/// resolve against the workspace root; `..` traversal that escapes every root
/// is rejected.
fn restrict_write_path(path: &str, cfg: &Config) -> Result<String, String> {
    if path.trim().is_empty() {
        return Err("path must not be empty".to_string());
    }
    let ws = resolve_workspace_dir(&cfg.workspace_dir);
    let candidate = if path.trim_start().starts_with('/') {
        path.to_string()
    } else {
        format!(
            "{}/{}",
            ws.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    };
    let normalized = normalize_path(std::path::Path::new(&candidate));
    let roots = allowed_write_roots(cfg);
    let allowed = roots.iter().any(|root| {
        let root = normalize_path(std::path::Path::new(root));
        normalized == root
            || normalized.starts_with(std::path::Path::new(&format!("{}/", root.display())))
    });
    if allowed {
        Ok(normalized.to_string_lossy().to_string())
    } else {
        Err(format!(
            "path outside allowed write roots; allowed roots: {} (got {})",
            roots.join(", "),
            normalized.display()
        ))
    }
}

/// Resolve a READ path: reads are unrestricted (anywhere on the filesystem),
/// so this only normalizes the path. Relative paths still resolve against the
/// workspace root for convenience, but absolute paths may point anywhere -
/// reading is side-effect free.
fn resolve_read_path(path: &str, workspace_dir: &str) -> String {
    let requested = Path::new(path);
    if requested.is_absolute() {
        normalize_path(requested).to_string_lossy().to_string()
    } else {
        normalize_path(&Path::new(workspace_dir).join(requested))
            .to_string_lossy()
            .to_string()
    }
}

/// Wrap a SYNC handler so (1) its work runs on tokio's blocking pool - never
/// inline on an async worker thread - and (2) any Err(e) becomes
/// Ok((error_msg, true)) so access-denied / file-not-found / invalid-input
/// errors never trip the MCP circuit breaker on the client side.
///
/// CRITICAL (Sep 2026, filesystem tools timing out): the previous version
/// called `h(args)` directly inside the async block, i.e. on a tokio WORKER
/// thread. All filesystem handlers are synchronous std::fs work - whole-file
/// reads (`fs::read_to_string` of arbitrarily large files before slicing),
/// `fs::read_dir` listings, `glob::glob` tree walks, and the recursive grep
/// walk that reads every file in a tree. A blocking call that takes a long
/// time (large file, deep tree, slow/hung mount) holds the worker thread
/// hostage: it CANNOT be interrupted by dropping the handler future (client
/// timeout / notifications/cancelled), and once enough concurrent slow calls
/// saturate every worker the WHOLE plugin wedges - every filesystem MCP call
/// then times out (the reported 2026-09-01 symptom; same failure class as the
/// actions-plugin incident documented in mcp-server-util's `sync_handler`).
///
/// Running the sync body on the blocking pool guarantees the async runtime
/// stays responsive: a slow call ties up at most one blocking-pool thread and
/// the handler future stays droppable (spawn_blocking tasks are detached on
/// drop), so concurrent filesystem calls proceed in parallel and client
/// cancellation resolves immediately.
fn soft_error<F>(handler: F) -> ToolHandler
where
    F: Fn(Value) -> Result<(String, bool)> + Clone + Send + Sync + 'static,
{
    Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let h = handler.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || h(args)).await {
                Ok(Ok((text, is_error))) => Ok((text, is_error)),
                Ok(Err(e)) => Ok((format!("{}", e), true)),
                Err(e) => Ok((format!("sync handler panicked: {e}"), true)),
            }
        })
    })
}

fn format_size(size: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if size < KB {
        format!("{} bytes", size)
    } else if size < MB {
        format!("{:.1} KB", size as f64 / KB as f64)
    } else {
        format!("{:.1} MB", size as f64 / MB as f64)
    }
}

// ---------------------------------------------------------------------------
// Tool: filesystem__read
// ---------------------------------------------------------------------------

/// R6: render a LINE-NUMBERED, line-paged read of `content`. Line numbers are
/// 1-based and use the plugin's own line convention (a trailing newline does
/// not open an extra empty line), so they match the line numbers reported by
/// filesystem__insert and filesystem__apply_patch. `offset` is the 1-based
/// number of the first line to show; `limit` is the max number of lines. The
/// returned text always ends with a bracket note describing the shown line
/// range and the file's total line count, so callers can page forward
/// deterministically without ever re-reading a line they already saw.
fn read_numbered_lines(content: &str, offset: usize, limit: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total == 0 {
        return "[file is empty (0 lines)]".to_string();
    }
    if limit == 0 {
        return format!(
            "[no lines shown: limit must be >= 1 (file has {} line(s))]",
            total
        );
    }
    if offset > total {
        return format!(
            "[file has {} line(s); nothing to show: offset {} is past the end]",
            total, offset
        );
    }
    let first = offset.max(1);
    let last = first.saturating_add(limit).saturating_sub(1).min(total);
    let mut out = String::new();
    for n in first..=last {
        out.push_str(&format!("{}:{}\n", n, lines[n - 1]));
    }
    let note = if last < total {
        format!(
            "[... truncated: showing lines {}-{} of {} total lines]",
            first, last, total
        )
    } else {
        format!(
            "[showing lines {}-{} of {} total lines]",
            first, last, total
        )
    };
    out.push('\n');
    out.push_str(&note);
    out
}

fn handle_read(args: Value, workspace_dir: &str) -> Result<(String, bool)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    // Reads are unrestricted - allowed anywhere on the filesystem.
    let safe_path = resolve_read_path(path, workspace_dir);
    let content = fs::read_to_string(&safe_path)
        .map_err(|e| anyhow::anyhow!("Failed to read file '{}': {}", safe_path, e))?;
    // R6: optional LINE-NUMBERED mode (lines=true). Each shown line is
    // prefixed with its 1-based number and paging is in lines, so reads are
    // cheap to reference (numbers match filesystem__insert/apply_patch) and
    // deterministic to page without re-reading.
    if args["lines"].as_bool().unwrap_or(false) {
        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"].as_u64().unwrap_or(500) as usize;
        let out = read_numbered_lines(&content, offset, limit);
        return Ok((out, false));
    }
    // Char-based paged reads: offset = starting char position (default 0),
    // limit = max chars returned (default 50_000, the legacy truncation).
    // The response reports the total file size and the returned slice so the
    // agent can page through a large file deterministically.
    let total_chars = content.chars().count();
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
    let limit = args["limit"].as_u64().unwrap_or(50_000) as usize;
    let start = offset.min(total_chars);
    let end = start.saturating_add(limit).min(total_chars);
    let slice: String = content.chars().skip(start).take(end - start).collect();
    let mut out = slice;
    if start > 0 || end < total_chars {
        let note = if end < total_chars {
            format!(
                "[... truncated: showing chars {}-{} of {} total chars]",
                start, end, total_chars
            )
        } else {
            format!(
                "[showing chars {}-{} of {} total chars]",
                start, end, total_chars
            )
        };
        out.push_str("\n\n");
        out.push_str(&note);
    }
    Ok((out, false))
}

// ---------------------------------------------------------------------------
// Tool: filesystem__write
// ---------------------------------------------------------------------------

fn handle_write(args: Value, cfg: &Config) -> Result<(String, bool)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    let content = args["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'content' argument"))?;
    let append = args["append"].as_bool().unwrap_or(false);

    // Validate path is within the workspace sandbox (lexical - works for
    // files that don't exist yet).
    let safe_path_str = restrict_write_path(path, cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let safe_path = Path::new(&safe_path_str);

    if let Some(parent) = safe_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("Failed to create parent directories: {}", e))?;
    }
    if append {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(safe_path)
            .map_err(|e| {
                anyhow::anyhow!("Failed to open file '{}' for append: {}", safe_path_str, e)
            })?;
        f.write_all(content.as_bytes())
            .map_err(|e| anyhow::anyhow!("Failed to append to file '{}': {}", safe_path_str, e))?;
        Ok((
            format!(
                "Successfully appended {} bytes to {}",
                content.len(),
                safe_path_str
            ),
            false,
        ))
    } else {
        fs::write(safe_path, content)
            .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {}", safe_path_str, e))?;
        Ok((
            format!(
                "Successfully wrote {} bytes to {}",
                content.len(),
                safe_path_str
            ),
            false,
        ))
    }
}

// ---------------------------------------------------------------------------
// Tool: file-edit primitives (filesystem__str_replace / filesystem__insert /
// filesystem__apply_patch)
//
// R4: precise, reviewable edits instead of whole-file rewrites. Edits are
// WRITES: the target path must pass the same sandbox as filesystem__write, and
// the file must already exist (create files with filesystem__write first).
// ---------------------------------------------------------------------------

/// Load an existing file for editing: resolve the write sandbox, require the
/// file to exist, then read its UTF-8 content.
fn load_file_for_edit(path: &str, cfg: &Config) -> Result<(String, String), String> {
    let safe_path_str = restrict_write_path(path, cfg)?;
    let safe_path = Path::new(&safe_path_str);
    if !safe_path.is_file() {
        return Err(format!(
            "cannot edit '{}': file does not exist (create it with filesystem__write first)",
            safe_path_str
        ));
    }
    let content = fs::read_to_string(safe_path)
        .map_err(|e| format!("Failed to read file '{}': {}", safe_path_str, e))?;
    Ok((content, safe_path_str))
}

/// Snapshot the write-relevant config for an edit-handler closure (same
/// pattern as the filesystem__write handler).
fn snapshot_write_cfg(cfg: &Config) -> Config {
    Config {
        workspace_dir: cfg.workspace_dir.clone(),
        omni_dir: cfg.omni_dir.clone(),
        write_profiles: cfg.write_profiles,
        write_data: cfg.write_data,
        write_plugins: cfg.write_plugins,
        write_omni_all: cfg.write_omni_all,
    }
}

/// Number of (non-overlapping) occurrences of `needle` in `haystack`.
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    let mut count = 0;
    let mut start = 0;
    while let Some(rel) = haystack[start..].find(needle) {
        count += 1;
        start += rel + needle.len();
    }
    count
}

/// Byte index of the `n`th (1-based) occurrence of `needle`, if any.
fn nth_occurrence(haystack: &str, needle: &str, n: usize) -> Option<usize> {
    let mut start = 0;
    let mut found = 0;
    while let Some(rel) = haystack[start..].find(needle) {
        let abs = start + rel;
        found += 1;
        if found == n {
            return Some(abs);
        }
        start = abs + needle.len();
    }
    None
}

/// 1-based line number ('\n'-separated) containing the byte offset `pos`.
fn line_of_offset(text: &str, pos: usize) -> usize {
    text[..pos.min(text.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
        + 1
}

/// 1-based start offsets of every line in `text`. A trailing newline does not
/// open an extra empty line ("a\nb\n" has lines "a" and "b"). Empty text has
/// zero lines.
fn line_starts(text: &str) -> Vec<usize> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut starts = vec![0usize];
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' && i + 1 < bytes.len() {
            starts.push(i + 1);
        }
    }
    starts
}

/// Render a possibly-multiline snippet as one short preview line.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.replace('\n', "\\n");
    if flat.chars().count() > max {
        let cut: String = flat.chars().take(max).collect();
        format!("{}...", cut)
    } else {
        flat
    }
}

/// Replace, in `buf`, the `target`th (1-based) occurrence of `old_string` with
/// `new_string`. `occurrence` 0 means "must be unique". Returns a short
/// confirmation string describing the change.
fn do_replace(
    buf: &mut String,
    old_string: &str,
    new_string: &str,
    occurrence: usize,
) -> Result<String, String> {
    if old_string.is_empty() {
        return Err("'old_string' must not be empty".to_string());
    }
    let count = count_occurrences(buf, old_string);
    if count == 0 {
        return Err(format!(
            "old_string not found in the current content ({} bytes). Read the file and copy the exact text to replace.",
            buf.len()
        ));
    }
    let target = if occurrence == 0 {
        if count > 1 {
            return Err(format!(
                "old_string occurs {} times in the current content. Make old_string unique by including surrounding context, or pass occurrence=N (1-based) to replace exactly the Nth match.",
                count
            ));
        }
        1
    } else if occurrence <= count {
        occurrence
    } else {
        return Err(format!(
            "occurrence {} out of range: old_string occurs {} time(s) in the current content.",
            occurrence, count
        ));
    };
    let idx = nth_occurrence(buf, old_string, target)
        .ok_or_else(|| "internal error: nth_occurrence failed after count check".to_string())?;
    let line = line_of_offset(buf, idx);
    buf.replace_range(idx..idx + old_string.len(), new_string);
    Ok(format!(
        "replaced at line {} (occurrence {}/{}): '{}' -> '{}'",
        line,
        target,
        count,
        one_line(old_string, 80),
        one_line(new_string, 80)
    ))
}

/// Insert `content` before 1-based `line` of `buf` (last_line + 1 appends at
/// the end of the file). The inserted content always occupies its own whole
/// lines. Returns a short confirmation string.
fn do_insert(buf: &mut String, line: usize, content: &str) -> Result<String, String> {
    if content.is_empty() {
        return Err("'content' must not be empty".to_string());
    }
    if line == 0 {
        return Err("'line' must be >= 1".to_string());
    }
    let starts = line_starts(buf);
    let total = starts.len();
    if line > total + 1 {
        return Err(format!(
            "'line' {} out of range: the file has {} line(s); valid insert lines are 1..={}",
            line,
            total,
            total + 1
        ));
    }
    let at_end = line == total + 1;
    let pos = if at_end { buf.len() } else { starts[line - 1] };
    let mut insert = content.to_string();
    // Appending after a last line that has no trailing newline: open the line
    // so the inserted content starts on its own line.
    if at_end && !buf.is_empty() && !buf.ends_with('\n') {
        insert.insert(0, '\n');
    }
    // Mid-file insert: close the inserted content's last line so the text that
    // follows stays on its own line.
    if pos < buf.len() && !insert.ends_with('\n') {
        insert.push('\n');
    }
    buf.insert_str(pos, &insert);
    Ok(format!(
        "inserted {} line(s) starting at line {}",
        content.lines().count(),
        line
    ))
}

// ---------------------------------------------------------------------------
// Tool: filesystem__str_replace
// ---------------------------------------------------------------------------

fn handle_str_replace(args: Value, cfg: &Config) -> Result<(String, bool)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    let old_string = args["old_string"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'old_string' argument"))?;
    let new_string = args["new_string"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'new_string' argument"))?;
    let occurrence = args["occurrence"].as_u64().unwrap_or(0) as usize;
    let (mut content, safe_path_str) =
        load_file_for_edit(path, cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let old_len = content.len();
    let confirmation =
        do_replace(&mut content, old_string, new_string, occurrence).map_err(anyhow::Error::msg)?;
    fs::write(&safe_path_str, &content)
        .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {}", safe_path_str, e))?;
    Ok((
        format!(
            "Successfully replaced old_string in '{}':\n  {}\nFile size: {} -> {} bytes.",
            safe_path_str,
            confirmation,
            old_len,
            content.len()
        ),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: filesystem__insert
// ---------------------------------------------------------------------------

fn handle_insert(args: Value, cfg: &Config) -> Result<(String, bool)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    let line = args["line"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("Missing 'line' argument"))? as usize;
    let content = args["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'content' argument"))?;
    let (mut buf, safe_path_str) =
        load_file_for_edit(path, cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let old_len = buf.len();
    let confirmation = do_insert(&mut buf, line, content).map_err(anyhow::Error::msg)?;
    fs::write(&safe_path_str, &buf)
        .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {}", safe_path_str, e))?;
    let total_now = line_starts(&buf).len();
    Ok((
        format!(
            "Successfully inserted content into '{}':\n  {}\nFile size: {} -> {} bytes ({} line(s) now).",
            safe_path_str, confirmation, old_len, buf.len(), total_now
        ),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: filesystem__apply_patch
// ---------------------------------------------------------------------------

fn handle_apply_patch(args: Value, cfg: &Config) -> Result<(String, bool)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    let edits = args["edits"].as_array().cloned().unwrap_or_default();
    if edits.is_empty() {
        return Err(anyhow::anyhow!(
            "'edits' must be a non-empty array of edit operations, e.g. [{{\"op\": \"replace\", \"old_string\": \"...\", \"new_string\": \"...\"}}]"
        ));
    }
    let (mut buf, safe_path_str) =
        load_file_for_edit(path, cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let old_len = buf.len();
    let mut confirmations: Vec<String> = Vec::with_capacity(edits.len());
    for (i, edit) in edits.iter().enumerate() {
        let op = edit.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let result = match op {
            "replace" => {
                let old_string =
                    edit.get("old_string")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            anyhow::anyhow!("edits[{}]: replace op needs 'old_string'", i)
                        })?;
                let new_string = edit
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let occurrence =
                    edit.get("occurrence").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                do_replace(&mut buf, old_string, new_string, occurrence)
            }
            "insert" => {
                let line = edit.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let content = edit
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("edits[{}]: insert op needs 'content'", i))?;
                do_insert(&mut buf, line, content)
            }
            "" => Err(format!(
                "edits[{}]: missing 'op' (use \"replace\" or \"insert\")",
                i
            )),
            other => Err(format!(
                "edits[{}]: unknown op '{}' (use \"replace\" or \"insert\")",
                i, other
            )),
        };
        let confirmation = result
            .map_err(|e| anyhow::anyhow!("edits[{}] failed: {}; no changes were written", i, e))?;
        confirmations.push(format!("  {}/{} {}", i + 1, edits.len(), confirmation));
    }
    // Every edit validated and applied to the in-memory copy - write once.
    fs::write(&safe_path_str, &buf)
        .map_err(|e| anyhow::anyhow!("Failed to write file '{}': {}", safe_path_str, e))?;
    Ok((
        format!(
            "Successfully applied {} edit(s) to '{}':\n{}\nFile size: {} -> {} bytes ({} line(s) now).",
            edits.len(),
            safe_path_str,
            confirmations.join("\n"),
            old_len,
            buf.len(),
            line_starts(&buf).len()
        ),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: filesystem__list
// ---------------------------------------------------------------------------

fn handle_list(args: Value, workspace_dir: &str) -> Result<(String, bool)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    // Listing is a read - allowed anywhere on the filesystem.
    let safe_path = resolve_read_path(path, workspace_dir);

    let entries = fs::read_dir(&safe_path)
        .map_err(|e| anyhow::anyhow!("Failed to list '{}': {}", safe_path, e))?;

    let mut results: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let typ = if entry.file_type()?.is_dir() {
            "directory"
        } else {
            "file"
        };
        results.push(format!("[{}] {}", typ.to_uppercase(), name));
    }
    results.sort();

    let max_entries = 2000;
    let output = if results.len() > max_entries {
        let joined = results[..max_entries].join("\n");
        format!(
            "{}\n[... truncated from {} to ~{} entries]",
            joined,
            results.len(),
            max_entries
        )
    } else if results.is_empty() {
        "(empty directory)".to_string()
    } else {
        results.join("\n")
    };

    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Tool: filesystem__search
// ---------------------------------------------------------------------------

fn handle_search(args: Value, workspace_dir: &str) -> Result<(String, bool)> {
    let pattern = args["pattern"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'pattern' argument"))?;
    // Default the search base to the workspace root, but searches may point
    // anywhere - searching is a read.
    let base_path = args["path"].as_str().unwrap_or(workspace_dir);
    let safe_base = resolve_read_path(base_path, workspace_dir);

    // Build a glob matcher for the requested pattern. The directory walk
    // below is HARD-BOUNDED so a pattern over a huge tree (e.g. "/") can
    // never run unbounded and wedge the whole plugin server (Sep 2026
    // outage: filesystem read/write/list down for whole threads after one
    // no-match search over "/").
    let matcher =
        glob::Pattern::new(pattern).map_err(|e| anyhow::anyhow!("Invalid glob pattern: {}", e))?;

    const MAX_DIRS: usize = 50_000;
    const MAX_FILES: usize = 200_000;
    const MAX_RESULTS: usize = 1000;
    const MAX_WALK_MILLIS: u128 = 10_000;

    let walk_start = std::time::Instant::now();
    let mut results: Vec<String> = Vec::new();
    let mut dirs_visited: usize = 0;
    let mut files_checked: usize = 0;
    let mut budget_exhausted = false;
    let mut stack: Vec<std::path::PathBuf> = vec![std::path::PathBuf::from(&safe_base)];
    while let Some(dir) = stack.pop() {
        dirs_visited += 1;
        if dirs_visited > MAX_DIRS || walk_start.elapsed().as_millis() > MAX_WALK_MILLIS {
            budget_exhausted = true;
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            files_checked += 1;
            if files_checked > MAX_FILES {
                budget_exhausted = true;
                break;
            }
            // Match against the path relative to the search base, which
            // reproduces glob::glob("{base}/{pattern}") semantics while
            // keeping the walk bounded and immune to symlink cycles.
            if let Ok(rel) = path.strip_prefix(&safe_base) {
                if matcher.matches_path(rel) {
                    results.push(path.to_string_lossy().to_string());
                    if results.len() >= MAX_RESULTS {
                        budget_exhausted = true;
                        break;
                    }
                }
            }
        }
        if budget_exhausted {
            break;
        }
    }
    results.sort();

    let output = if results.is_empty() && budget_exhausted {
        format!(
            "No files matching '{}' in {} (search budget exhausted - tree too large)",
            pattern, safe_base
        )
    } else if results.is_empty() {
        format!("No files matching '{}' in {}", pattern, safe_base)
    } else {
        let joined = results.join("\n");
        if budget_exhausted || results.len() >= MAX_RESULTS {
            format!(
                "{}\n[... truncated to {} results (search budget exhausted)]",
                joined,
                results.len()
            )
        } else {
            joined
        }
    };

    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Tool: filesystem__info
// ---------------------------------------------------------------------------

fn handle_info(args: Value, workspace_dir: &str) -> Result<(String, bool)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    // Metadata lookup is a read - allowed anywhere on the filesystem.
    let safe_path = resolve_read_path(path, workspace_dir);

    let metadata = fs::metadata(&safe_path)
        .map_err(|e| anyhow::anyhow!("Failed to stat '{}': {}", safe_path, e))?;

    let modified = metadata
        .modified()
        .map(|t| {
            let dt: DateTime<Utc> = t.into();
            dt.to_rfc3339()
        })
        .unwrap_or_default();

    let created = metadata
        .created()
        .map(|t| {
            let dt: DateTime<Utc> = t.into();
            dt.to_rfc3339()
        })
        .unwrap_or_default();

    let typ = if metadata.is_dir() {
        "directory"
    } else {
        "file"
    };

    let output = format!(
        "Path: {}\nType: {}\nSize: {}\nPermissions: {:o}\nCreated: {}\nModified: {}",
        safe_path,
        typ,
        format_size(metadata.len()),
        metadata.permissions().mode() & 0o777,
        created,
        modified,
    );

    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Tool: filesystem__grep (recursive regex content search)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tool: filesystem__grep - RIPGREP-BACKED recursive content search (R5)
//
// Directory traversal uses ignore::WalkBuilder, the same engine ripgrep is
// built on (cycle-safe, no symlink following, opt-in hidden/.gitignore
// filtering). Content matching stays per-line regex (case-insensitive by
// default). Inline results are capped at max_results; when the cap is
// exceeded the FULL result list is spilled verbatim to a file whose path is
// reported, so no match is ever silently dropped.
// ---------------------------------------------------------------------------

const GREP_MAX_SPILL_BYTES: u64 = 64 * 1024 * 1024; // stop spilling past 64 MiB

/// Preferred spill root honoring the write sandbox: platform-style
/// {OMNI_DIR}/data/spill when OMNI_DIR/data writes are enabled (default),
/// else {workspace}/.grep-spill (the workspace root is always writable).
fn grep_spill_root(cfg: &Config) -> std::path::PathBuf {
    let ws = resolve_workspace_dir(&cfg.workspace_dir);
    let omni = resolve_omni_dir(&cfg.omni_dir);
    if cfg.write_omni_all || cfg.write_data {
        std::path::PathBuf::from(omni).join("data").join("spill")
    } else {
        std::path::PathBuf::from(ws).join(".grep-spill")
    }
}

/// Keep only `[A-Za-z0-9._-]` (max 64 chars) so a base path can never inject
/// separators or metacharacters into a spill file name.
fn sanitize_spill_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() || out == "." || out == ".." {
        return "grep".to_string();
    }
    if out.chars().count() > 64 {
        out = out.chars().take(64).collect();
    }
    out
}

/// Streaming sink for spilled grep results. Created lazily the first time the
/// inline cap is exceeded; every match (including the inline head) is written
/// so the spill file is a complete record of the hit list.
struct GrepSpill {
    writer: std::io::BufWriter<std::fs::File>,
    path: std::path::PathBuf,
    bytes: u64,
    stopped: bool, // hit the byte cap or a write error
}

impl GrepSpill {
    fn create(cfg: &Config, base_tag: &str) -> std::io::Result<GrepSpill> {
        let root = grep_spill_root(cfg);
        fs::create_dir_all(&root)?;
        let tag = sanitize_spill_segment(base_tag);
        for nonce in 0..100u32 {
            let name = format!(
                "filesystem__grep-{}-{}-{}.txt",
                tag,
                std::process::id(),
                nonce
            );
            let path = root.join(name);
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(f) => {
                    return Ok(GrepSpill {
                        writer: std::io::BufWriter::new(f),
                        path,
                        bytes: 0,
                        stopped: false,
                    })
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique grep spill file",
        ))
    }

    /// Append one formatted hit line. After the byte cap or a write error the
    /// sink stops accepting lines but the caller keeps counting matches.
    fn push(&mut self, line: &str) {
        use std::io::Write;
        if self.stopped {
            return;
        }
        let mut buf = String::with_capacity(line.len() + 1);
        buf.push_str(line);
        buf.push('\n');
        self.bytes += buf.len() as u64;
        if self.bytes > GREP_MAX_SPILL_BYTES {
            self.stopped = true;
            return;
        }
        if self.writer.write_all(buf.as_bytes()).is_err() {
            self.stopped = true;
        }
    }

    /// Flush buffered lines so the spill file is complete before the caller
    /// reports its path.
    fn finish(&mut self) {
        use std::io::Write;
        let _ = self.writer.flush();
    }
}

fn handle_grep(args: Value, cfg: &Config) -> Result<(String, bool)> {
    let pattern = args["pattern"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'pattern' argument"))?;
    let workspace_dir = resolve_workspace_dir(&cfg.workspace_dir);
    // Default the base to the workspace root, but searches may point
    // anywhere - searching is a read.
    let base_path = args["path"].as_str().unwrap_or(&workspace_dir);
    let safe_base = resolve_read_path(base_path, &workspace_dir);
    let case_sensitive = args["case_sensitive"].as_bool().unwrap_or(false);
    // hidden and git_ignore default to the tool's historical broad search;
    // set hidden=false / git_ignore=true for ripgrep's own defaults.
    let hidden = args["hidden"].as_bool().unwrap_or(true);
    let git_ignore = args["git_ignore"].as_bool().unwrap_or(false);
    let max_results = args["max_results"].as_u64().unwrap_or(200).min(1000) as usize;

    // Optional file-name filter: validated ONCE up front. A glob containing a
    // path separator matches the full path; one without it matches the file
    // name at any depth (rg semantics, e.g. '*.rs' finds src/main.rs too).
    let glob_pat = match args["glob"].as_str() {
        Some(g) => Some(
            glob::Pattern::new(g).map_err(|e| anyhow::anyhow!("Invalid glob '{}': {}", g, e))?,
        ),
        None => None,
    };
    let glob_is_full_path = args["glob"]
        .as_str()
        .map(|g| g.contains('/'))
        .unwrap_or(false);

    // Case-insensitive by default (like grep -i). The regex itself carries
    // the flag - never lowercase the input line, which would corrupt
    // character classes and Unicode.
    let re = RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|e| anyhow::anyhow!("Invalid regex pattern '{}': {}", pattern, e))?;

    // Hard walk bounds so a grep over a huge tree (e.g. "/") can never run
    // unbounded and wedge the whole plugin server (Sep 2026 outage class:
    // filesystem read/write/list down after an unbounded recursive search).
    const MAX_DIRS: usize = 50_000;
    const MAX_FILES: usize = 100_000;
    const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024; // 16 MiB per file
    const MAX_WALK_MILLIS: u128 = 10_000;
    const MAX_LINE_CHARS: usize = 200;

    // Ripgrep-backed traversal: ignore::WalkBuilder is the engine ripgrep is
    // built on. hidden/git_ignore are opt-in flags; .git, target and
    // node_modules are always pruned; follow_links is off so symlink cycles
    // are impossible.
    let mut builder = WalkBuilder::new(std::path::PathBuf::from(&safe_base));
    builder
        .hidden(!hidden)
        .ignore(git_ignore)
        .git_ignore(git_ignore)
        .git_global(false)
        .git_exclude(git_ignore)
        .parents(false)
        .follow_links(false)
        .filter_entry(|entry| {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    if name == ".git" || name == "target" || name == "node_modules" {
                        return false;
                    }
                }
            }
            true
        });
    let walk = builder.build();

    let walk_start = std::time::Instant::now();
    let mut inline: Vec<String> = Vec::new();
    let mut spill: Option<GrepSpill> = None;
    let mut spill_attempted = false;
    let mut total_matches: usize = 0;
    let mut files_checked: usize = 0;
    let mut dirs_visited: usize = 0;
    let mut budget_exhausted = false;

    for entry in walk {
        dirs_visited += 1;
        if dirs_visited > MAX_DIRS || walk_start.elapsed().as_millis() > MAX_WALK_MILLIS {
            budget_exhausted = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() || !ft.is_file() {
            continue;
        }
        let path = entry.path();
        // Optional file-name filter, applied BEFORE reading so non-matching
        // files cost nothing.
        if let Some(pat) = &glob_pat {
            let matched = if glob_is_full_path {
                pat.matches_path(path)
            } else {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| pat.matches(n))
                    .unwrap_or(false)
            };
            if !matched {
                continue;
            }
        }
        // Skip oversized files entirely: reading them fully is both slow and
        // a memory hazard (multi-GB logs, virtual files under /proc).
        if let Ok(md) = fs::metadata(path) {
            if md.len() > MAX_FILE_BYTES {
                continue;
            }
        }
        // Skip binary files: sniff for a NUL byte in the first 8 KiB.
        let Ok(bytes) = fs::read(path) else { continue };
        let head = &bytes[..bytes.len().min(8192)];
        if head.contains(&0) {
            continue;
        }
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };
        files_checked += 1;
        if files_checked > MAX_FILES {
            budget_exhausted = true;
            break;
        }
        for (i, line) in content.lines().enumerate() {
            if !re.is_match(line) {
                continue;
            }
            total_matches += 1;
            let line_num = i + 1;
            let display = if line.chars().count() > MAX_LINE_CHARS {
                let trunc = line
                    .char_indices()
                    .nth(MAX_LINE_CHARS)
                    .map(|(idx, _)| idx)
                    .unwrap_or(line.len());
                format!("{}...", &line[..trunc])
            } else {
                line.to_string()
            };
            let hit = format!("{}:{}:{}", path.display(), line_num, display);
            if total_matches <= max_results {
                inline.push(hit);
            } else {
                // CAP HIT: lazily open the spill sink once and stream every
                // hit (including the inline head) so the spill is complete.
                if !spill_attempted {
                    spill_attempted = true;
                    match GrepSpill::create(cfg, path.to_str().unwrap_or("grep")) {
                        Ok(mut s) => {
                            for acc in &inline {
                                s.push(acc);
                            }
                            s.push(&hit);
                            spill = Some(s);
                        }
                        Err(_) => {
                            // Spill unavailable: keep counting matches and
                            // drop the overflow (reported in the response).
                        }
                    }
                } else if let Some(s) = spill.as_mut() {
                    s.push(&hit);
                }
            }
        }
    }
    if let Some(s) = spill.as_mut() {
        s.finish();
    }
    let spill_path = spill.as_ref().map(|s| s.path.display().to_string());
    let spill_stopped = spill.as_ref().map(|s| s.stopped).unwrap_or(false);
    let inline_n = inline.len();

    let output = if total_matches == 0 && budget_exhausted {
        format!(
            "No matches for pattern '{}' in {} ({} files checked, grep budget exhausted)",
            pattern, safe_base, files_checked
        )
    } else if total_matches == 0 {
        format!(
            "No matches for pattern '{}' in {} ({} files checked)",
            pattern, safe_base, files_checked
        )
    } else {
        let mut out = format!(
            "Found {} match(es) for pattern '{}' in {} ({} files checked):\n",
            total_matches, pattern, safe_base, files_checked
        );
        out.push_str(&inline.join("\n"));
        let mut notes: Vec<String> = Vec::new();
        if let Some(spath) = spill_path {
            notes.push(format!(
                "{} more match(es) beyond the {} shown inline - FULL result list written to {}",
                total_matches - inline_n,
                inline_n,
                spath
            ));
            if spill_stopped {
                notes.push(
                    "spill file truncated at the 64 MiB cap - matches beyond it were not recorded"
                        .to_string(),
                );
            }
        } else if total_matches > inline_n {
            notes.push(format!(
                "{} more match(es) exist beyond the {} shown inline but the spill file could not be created - results truncated",
                total_matches - inline_n,
                inline_n
            ));
        }
        if budget_exhausted {
            notes.push(
                "grep budget exhausted (dir/file/time walk bounds) - result set may be incomplete"
                    .to_string(),
            );
        }
        if !notes.is_empty() {
            out.push_str(&format!("\n[{}]", notes.join(" | ")));
        }
        out
    };

    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Plugin config - received via MCP configure message, not from env vars
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Config {
    workspace_dir: String,
    omni_dir: String,
    write_profiles: bool,
    write_data: bool,
    write_plugins: bool,
    write_omni_all: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workspace_dir: String::new(),
            omni_dir: String::new(),
            write_profiles: true,
            write_data: true,
            write_plugins: true,
            write_omni_all: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let config: Arc<Mutex<Config>> = Arc::new(Mutex::new(Config::default()));

    // on_configure: called when omniagent sends the resolved plugin config
    let on_configure = {
        let config = config.clone();
        Some(move |params: Value| {
            let mut cfg = config.lock();
            if let Some(dir) = params.get("workspace_dir").and_then(|v| v.as_str()) {
                if !dir.is_empty() {
                    cfg.workspace_dir = dir.to_string();
                }
            }
            if let Some(dir) = params.get("omni_dir").and_then(|v| v.as_str()) {
                if !dir.is_empty() {
                    cfg.omni_dir = dir.to_string();
                }
            }
            if let Some(v) = params.get("write_profiles").and_then(|v| v.as_bool()) {
                cfg.write_profiles = v;
            }
            if let Some(v) = params.get("write_data").and_then(|v| v.as_bool()) {
                cfg.write_data = v;
            }
            if let Some(v) = params.get("write_plugins").and_then(|v| v.as_bool()) {
                cfg.write_plugins = v;
            }
            if let Some(v) = params.get("write_omni_all").and_then(|v| v.as_bool()) {
                cfg.write_omni_all = v;
            }
        })
    };

    // Resolve the effective workspace dir from the (possibly empty) config.
    let c1 = config.clone();
    let read_handler = soft_error(move |args: Value| {
        let cfg = c1.lock();
        let wd = resolve_workspace_dir(&cfg.workspace_dir);
        handle_read(args, &wd)
    });

    let c2 = config.clone();
    let write_handler = soft_error(move |args: Value| {
        let cfg = c2.lock();
        let write_cfg = Config {
            workspace_dir: cfg.workspace_dir.clone(),
            omni_dir: cfg.omni_dir.clone(),
            write_profiles: cfg.write_profiles,
            write_data: cfg.write_data,
            write_plugins: cfg.write_plugins,
            write_omni_all: cfg.write_omni_all,
        };
        handle_write(args, &write_cfg)
    });

    let c3 = config.clone();
    let list_handler = soft_error(move |args: Value| {
        let cfg = c3.lock();
        let wd = resolve_workspace_dir(&cfg.workspace_dir);
        handle_list(args, &wd)
    });

    let c4 = config.clone();
    let search_handler = soft_error(move |args: Value| {
        let cfg = c4.lock();
        let wd = resolve_workspace_dir(&cfg.workspace_dir);
        handle_search(args, &wd)
    });

    let c5 = config.clone();
    let info_handler = soft_error(move |args: Value| {
        let cfg = c5.lock();
        let wd = resolve_workspace_dir(&cfg.workspace_dir);
        handle_info(args, &wd)
    });

    let c6 = config.clone();
    let grep_handler = soft_error(move |args: Value| {
        let write_cfg = snapshot_write_cfg(&c6.lock());
        handle_grep(args, &write_cfg)
    });

    let c7 = config.clone();
    let str_replace_handler = soft_error(move |args: Value| {
        let write_cfg = snapshot_write_cfg(&c7.lock());
        handle_str_replace(args, &write_cfg)
    });

    let c8 = config.clone();
    let insert_handler = soft_error(move |args: Value| {
        let write_cfg = snapshot_write_cfg(&c8.lock());
        handle_insert(args, &write_cfg)
    });

    let c9 = config.clone();
    let apply_patch_handler = soft_error(move |args: Value| {
        let write_cfg = snapshot_write_cfg(&c9.lock());
        handle_apply_patch(args, &write_cfg)
    });

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "filesystem_read".to_string(),
                description:
                    "READ A LOCAL FILE from disk. Use this to read any file on the filesystem (markdown, text files, config files, code files, research documents). This is the ONLY tool for reading existing file content. Do NOT use search_messages for file reading. \
                    READS ARE UNRESTRICTED: any path on the filesystem can be read (only WRITES are confined to the workspace dir). \
                    LARGE FILES: reads are CHAR-BASED SLICES. 'offset' (default 0) is the starting char position; 'limit' (default 50000) is the max chars returned. The response reports the slice returned, e.g. \"[showing chars 50000-100000 of 250000 total chars]\", so you can page deterministically. No args = first 50000 chars, with a truncation note when the file is bigger. LINE-NUMBERED READS (lines=true): use when you need to reference specific lines. Every shown line is prefixed with its 1-based line number ('N:line'; the numbers match filesystem__insert/filesystem__apply_patch line numbering). 'offset' (default 1) is the 1-based first line to show; 'limit' (default 500) is the max number of lines. The response always ends with a bracket note, e.g. \"[showing lines 1-500 of 1200 total lines]\" or \"[... truncated: showing lines 1-500 of 1200 total lines]\", so you know exactly which lines you saw and can page forward deterministically without re-reading."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute path to the file to read"
                        },
                        "lines": {
                            "type": "boolean",
                            "description": "Read in line-numbered mode (default false): paging in lines and every shown line prefixed with its 1-based number, e.g. '12:let x = 1;'"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Char mode: starting char position, 0-based (default 0). Lines mode: first line to show, 1-based (default 1)."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Char mode: maximum chars to return (default 50000). Lines mode: maximum lines to return (default 500)."
                        }
                    },
                    "required": ["path"]
                }),
            },
            handler: read_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "filesystem_write".to_string(),
                description:
                    "WRITE/CREATE A LOCAL FILE on disk. Use this to save content to a new or existing file. Creates parent directories automatically. This is the ONLY tool for writing file content. \
                    For very large files that exceed your output token limit, split the content across multiple calls: first call with append=false, then subsequent calls with append=true to add the rest. \
                    SANDBOX: writes are allowed inside the workspace dir (/opt/workspace by default) AND inside OMNI_DIR subdirectories per plugin config: omni_dir/profiles (write_profiles), omni_dir/data (write_data), omni_dir/plugins (write_plugins); write_omni_all=true allows the entire OMNI_DIR. Writes anywhere else are rejected."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute path to the file to write"
                        },
                        "content": {
                            "type": "string",
                            "description": "The content to write to the file"
                        },
                        "append": {
                            "type": "boolean",
                            "description": "If true, append content to the end of the file instead of overwriting it (default: false). Use for writing very large files in chunks."
                        }
                    },
                    "required": ["path", "content"]
                }),
            },
            handler: write_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "filesystem_list".to_string(),
                description:
                    "LIST FILES AND DIRECTORIES at a given path. Use this to explore a directory and see what files exist before reading them. Returns names and types (file vs directory). \
                    LISTS ARE UNRESTRICTED: any path on the filesystem can be listed (only WRITES are confined to the workspace dir)."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute path to list"
                        }
                    },
                    "required": ["path"]
                }),
            },
            handler: list_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "filesystem_search".to_string(),
                description:
                    "SEARCH FOR FILES BY NAME matching a glob pattern (e.g. '*.md', '**/*.rs'). Searches recursively from the given path. Use this when you need to find files with specific names or extensions. \
                    SEARCHES ARE UNRESTRICTED: any base path on the filesystem can be searched (defaults to the workspace root; only WRITES are confined to the workspace dir)."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Base directory to search from"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern to match (e.g. '*.md', '**/*.rs')"
                        }
                    },
                    "required": ["path", "pattern"]
                }),
            },
            handler: search_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "filesystem_info".to_string(),
                description:
                    "GET FILE/DIRECTORY METADATA. Returns size, type (file or directory), modification time, and permissions. Use this to check if a path exists and get details about it before reading. \
                    INFO IS UNRESTRICTED: any path on the filesystem can be inspected (only WRITES are confined to the workspace dir)."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute path to the file or directory"
                        }
                    },
                    "required": ["path"]
                }),
            },
            handler: info_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                                name: "filesystem_grep".to_string(),
                description:
                    "SEARCH FILE CONTENTS recursively for lines matching a REGEX pattern (like grep -rn). \
                    Use this to find where a symbol, string or pattern appears in code or configs. \
                    The recursive walk is RIPGREP-BACKED (ignore::WalkBuilder, the same traversal engine \
                    ripgrep is built on), so large trees are searched cheaply and safely under hard caps; \
                    dotfile and .gitignore handling are opt-in flags. \
                    SEARCHES ARE UNRESTRICTED: any base path can be searched (defaults to the workspace root; \
                    only WRITES are confined to the workspace dir). \
                    Returns 'path:line: content' matches, capped at max_results (default 200). \
                    CAPS + SPILL: when more than max_results matches exist the FULL result list is spilled \
                    verbatim to a file under OMNI_DIR/data/spill (or the workspace) and its path is reported \
                    in the response - read that file with filesystem__read for the remaining hits, nothing is lost. \
                    Prefer this over filesystem__search (names only): content discovery belongs in filesystem__grep."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regular expression to match against file contents (e.g. 'update_thread_progress', 'fn handle_', 'TODO|FIXME')"
                        },
                        "path": {
                            "type": "string",
                            "description": "Base directory to search recursively (default: workspace root)"
                        },
                        "glob": {
                            "type": "string",
                            "description": "Optional file-name filter (rg-style: without '/' it matches the file name at any depth, e.g. '*.rs'; with '/' it matches the full path, e.g. '**/tests/*.rs')"
                        },
                        "case_sensitive": {
                            "type": "boolean",
                            "description": "Match case-sensitively (default false)",
                            "default": false
                        },
                        "hidden": {
                            "type": "boolean",
                            "description": "Include hidden files and dot-directories in the walk (default true; false = ripgrep default of skipping hidden)",
                            "default": true
                        },
                        "git_ignore": {
                            "type": "boolean",
                            "description": "Respect .gitignore/.ignore files like ripgrep (default false: everything is searched except .git/target/node_modules)",
                            "default": false
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Max matches returned inline (default 200, max 1000); overflow is spilled in full to a spill file",
                            "default": 200
                        }
                    },
                    "required": ["pattern"]
                }),
            },
handler: grep_handler,
        },

        McpToolEntry {
            def: McpToolDef {
                name: "filesystem_str_replace".to_string(),
                description:
                    "REPLACE AN EXACT STRING INSIDE A FILE (surgical edit). Use for precise, reviewable edits instead of rewriting the whole file with filesystem__write: only the matched text changes, the rest of the file is untouched. 'old_string' must appear in the file - when it appears several times the call fails unless you pass occurrence=N (1-based) to pick the Nth match or extend old_string with surrounding context to make it unique. Pass new_string = \"\" (empty string) to delete the matched text. The file must already exist, and the path must be inside the same write sandbox as filesystem__write (workspace dir or enabled OMNI_DIR subdirs). Returns a confirmation with the affected line number, occurrence, previews and the new file size."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute path to the existing file to edit (same write sandbox as filesystem__write)"
                        },
                        "old_string": {
                            "type": "string",
                            "description": "Exact text to replace. Must appear in the file; if it appears several times, include surrounding context to make it unique or pass occurrence=N."
                        },
                        "new_string": {
                            "type": "string",
                            "description": "Replacement text. Pass an empty string to delete the matched text."
                        },
                        "occurrence": {
                            "type": "integer",
                            "description": "Optional 1-based index of the match to replace when old_string occurs several times (omit when the match is unique)"
                        }
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
            handler: str_replace_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "filesystem_insert".to_string(),
                description:
                    "INSERT LINES INTO AN EXISTING FILE at a 1-based line number (surgical edit). 'content' is inserted BEFORE 'line': line 1 inserts at the top of the file, line = last_line+1 appends at the end. The inserted content always occupies its own whole lines (newlines are added automatically where needed). Use for precise, reviewable edits instead of rewriting the whole file with filesystem__write. The file must already exist, and the path must be inside the same write sandbox as filesystem__write. Returns a confirmation with the inserted position/line count and the new file size."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute path to the existing file to edit (same write sandbox as filesystem__write)"
                        },
                        "line": {
                            "type": "integer",
                            "description": "1-based line number before which content is inserted (last_line + 1 appends at the end of the file)"
                        },
                        "content": {
                            "type": "string",
                            "description": "Lines to insert (may be multi-line; newline handling is automatic)"
                        }
                    },
                    "required": ["path", "line", "content"]
                }),
            },
            handler: insert_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "filesystem_apply_patch".to_string(),
                description:
                    "APPLY A BATCH OF PRECISE EDITS TO A FILE, ATOMICALLY. 'edits' is an array of operations applied in order to an in-memory copy: if ANY operation fails to match, NOTHING is written and the file is left exactly as it was. Each operation: {\"op\": \"replace\", \"old_string\": ..., \"new_string\": ..., \"occurrence\": N?} replaces an exact string (new_string omitted or \"\" deletes; occurrence is an optional 1-based index for repeated matches), or {\"op\": \"insert\", \"line\": N, \"content\": ...} inserts content before the 1-based line N. Use apply_patch for multi-hunk edits in one reviewable call instead of several whole-file rewrites. Same rules as filesystem__str_replace/filesystem__insert: the file must exist and the path must be inside the write sandbox."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute path to the existing file to edit (same write sandbox as filesystem__write)"
                        },
                        "edits": {
                            "type": "array",
                            "items": {
                                "type": "object"
                            },
                            "description": "Non-empty array of edit operations applied in order and atomically. replace: {\"op\":\"replace\",\"old_string\":...,\"new_string\":...(\"\" or omitted deletes),\"occurrence\":N?} | insert: {\"op\":\"insert\",\"line\":N,\"content\":...}"
                        }
                    },
                    "required": ["path", "edits"]
                }),
            },
            handler: apply_patch_handler,
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-filesystem".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server_with_config(server_info, tools, on_configure).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_policy_workspace_always_allowed() {
        let cfg = Config {
            workspace_dir: "/opt/workspace".into(),
            ..Config::default()
        };
        assert!(restrict_write_path("/opt/workspace/a.txt", &cfg).is_ok());
        assert!(restrict_write_path("/opt/workspace", &cfg).is_ok());
        assert!(restrict_write_path("/opt/workspace/sub/dir/f.txt", &cfg).is_ok());
        // relative paths resolve against the workspace root
        assert!(restrict_write_path("a.txt", &cfg).is_ok());
        let err = restrict_write_path("/etc/passwd", &cfg).unwrap_err();
        assert!(err.contains("allowed write roots"), "err: {err}");
    }

    #[test]
    fn write_policy_rejects_traversal() {
        let cfg = Config {
            workspace_dir: "/opt/workspace".into(),
            ..Config::default()
        };
        assert!(restrict_write_path("/opt/workspace/../etc/passwd", &cfg).is_err());
        assert!(restrict_write_path("../../etc/passwd", &cfg).is_err());
        assert!(restrict_write_path("/opt/workspace/../../etc/passwd", &cfg).is_err());
    }

    #[test]
    fn write_policy_omni_subdir_toggles() {
        let cfg = Config {
            workspace_dir: "/opt/workspace".into(),
            omni_dir: "/opt/omni".into(),
            ..Config::default()
        };
        // defaults: all three subdir toggles on
        assert!(restrict_write_path("/opt/omni/data/threads/5/notes.md", &cfg).is_ok());
        assert!(restrict_write_path("/opt/omni/profiles/omni/wiki/a.md", &cfg).is_ok());
        assert!(restrict_write_path("/opt/omni/plugins/x/main.rs", &cfg).is_ok());
        // but the omni root itself is NOT allowed without write_omni_all
        assert!(restrict_write_path("/opt/omni/other.txt", &cfg).is_err());
        // write_data off
        let mut c2 = cfg.clone();
        c2.write_data = false;
        assert!(restrict_write_path("/opt/omni/data/x", &c2).is_err());
        assert!(restrict_write_path("/opt/omni/profiles/x", &c2).is_ok());
        assert!(restrict_write_path("/opt/omni/plugins/x", &c2).is_ok());
        // write_profiles off
        let mut c3 = cfg.clone();
        c3.write_profiles = false;
        assert!(restrict_write_path("/opt/omni/profiles/x", &c3).is_err());
        assert!(restrict_write_path("/opt/omni/data/x", &c3).is_ok());
        // write_plugins off
        let mut c4 = cfg.clone();
        c4.write_plugins = false;
        assert!(restrict_write_path("/opt/omni/plugins/x", &c4).is_err());
        assert!(restrict_write_path("/opt/omni/data/x", &c4).is_ok());
    }

    #[test]
    fn write_policy_omni_all_overrides_subdir_toggles() {
        let mut cfg = Config {
            workspace_dir: "/opt/workspace".into(),
            omni_dir: "/opt/omni".into(),
            ..Config::default()
        };
        cfg.write_omni_all = true;
        cfg.write_data = false;
        cfg.write_profiles = false;
        cfg.write_plugins = false;
        assert!(restrict_write_path("/opt/omni/anywhere.txt", &cfg).is_ok());
        assert!(restrict_write_path("/opt/omni/data/x", &cfg).is_ok());
        // .. traversal still rejected even with omni_all
        assert!(restrict_write_path("/opt/omni/../etc/passwd", &cfg).is_err());
        assert!(restrict_write_path("/etc/passwd", &cfg).is_err());
        // workspace always allowed
        assert!(restrict_write_path("/opt/workspace/f.rs", &cfg).is_ok());
    }

    #[test]
    fn write_policy_all_disabled_still_allows_workspace() {
        let mut cfg = Config {
            workspace_dir: "/opt/workspace".into(),
            omni_dir: "/opt/omni".into(),
            ..Config::default()
        };
        cfg.write_data = false;
        cfg.write_profiles = false;
        cfg.write_plugins = false;
        assert!(restrict_write_path("/opt/workspace/a", &cfg).is_ok());
        let err = restrict_write_path("/opt/omni/data/a", &cfg).unwrap_err();
        assert!(
            err.contains("/opt/workspace"),
            "error must list allowed roots: {err}"
        );
    }

    #[test]
    fn write_outside_sandbox_rejected() {
        // The raw handler returns Err; soft_error at the MCP boundary converts
        // it to Ok((msg, true)) so the circuit breaker never trips.
        let err = handle_write(
            serde_json::json!({
                "path": "/opt/omni/evil.txt",
                "content": "boom",
            }),
            &Config {
                workspace_dir: "/opt/workspace".to_string(),
                ..Config::default()
            },
        )
        .expect_err("write outside sandbox must be rejected");
        assert!(err.to_string().contains("outside allowed write roots"));
    }

    #[tokio::test]
    async fn write_outside_sandbox_soft_error_does_not_trip() {
        // Through soft_error the rejection arrives as Ok((msg, true)) - NOT a
        // handler Err - so the MCP circuit breaker stays closed.
        let (msg, is_error) = soft_error(|args: Value| {
            handle_write(
                args,
                &Config {
                    workspace_dir: "/opt/workspace".to_string(),
                    ..Config::default()
                },
            )
        })(
            serde_json::json!({
                "path": "/opt/omni/evil.txt",
                "content": "boom",
            }),
            None,
        )
        .await
        .expect("soft_error always returns Ok");
        assert!(is_error);
        assert!(msg.contains("outside allowed write roots"));
    }

    #[test]
    fn write_inside_sandbox_succeeds() {
        let dir = std::env::temp_dir().join("fs-sandbox-test");
        let _ = fs::remove_dir_all(&dir);
        let (msg, is_error) = handle_write(
            serde_json::json!({
                "path": dir.join("sub/deep/file.txt").to_string_lossy(),
                "content": "hello",
            }),
            &Config {
                workspace_dir: dir.to_string_lossy().to_string(),
                ..Config::default()
            },
        )
        .expect("write inside sandbox succeeds");
        assert!(!is_error, "msg: {}", msg);
        let content = fs::read_to_string(dir.join("sub/deep/file.txt")).unwrap();
        assert_eq!(content, "hello");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_outside_workspace_allowed() {
        // Reads are UNRESTRICTED - only writes are sandboxed. Reading
        // /etc/hostname (outside /opt/workspace) must succeed.
        let (msg, is_error) = handle_read(
            serde_json::json!({"path": "/etc/hostname"}),
            "/opt/workspace",
        )
        .expect("read outside workspace must succeed");
        assert!(!is_error, "msg: {}", msg);
        assert!(msg.contains("hostname") || !msg.trim().is_empty());
    }

    #[test]
    fn read_relative_path_resolves_to_workspace() {
        // Relative reads still resolve against the workspace root. Uses a
        // temp dir (like the other sandbox tests) so it also passes inside
        // the Docker build context, where /opt/workspace does not exist.
        let dir = std::env::temp_dir().join("fs-sandbox-rel-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/file.txt"), "hello").unwrap();
        let (msg, is_error) = handle_read(
            serde_json::json!({"path": "sub/file.txt"}),
            &dir.to_string_lossy(),
        )
        .expect("relative read must succeed");
        assert!(!is_error, "msg: {msg}");
        assert!(msg.contains("hello"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_and_info_outside_workspace_allowed() {
        // Listing /etc is a read - allowed.
        let (msg, is_error) = handle_list(serde_json::json!({"path": "/etc"}), "/opt/workspace")
            .expect("list outside workspace must succeed");
        assert!(!is_error, "msg: {}", msg);
        // info on a file outside the workspace is allowed too.
        let (msg2, is_error2) = handle_info(
            serde_json::json!({"path": "/etc/hostname"}),
            "/opt/workspace",
        )
        .expect("info outside workspace must succeed");
        assert!(!is_error2, "msg: {}", msg2);
        assert!(msg2.contains("Type: file"));
    }

    #[test]
    fn grep_matches_lines_recursively() {
        let dir = std::env::temp_dir().join(format!("fs-grep-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.rs"), "fn alpha() {\n    let x = 1;\n}\n").unwrap();
        fs::write(dir.join("sub/b.txt"), "hello world\nalpha beta\n").unwrap();
        fs::write(dir.join("sub/c.md"), "nothing here\n").unwrap();
        let cfg = tmp_grep_cfg(&dir);
        let (msg, is_error) = handle_grep(
            serde_json::json!({
                "pattern": "alpha",
                "path": dir.to_string_lossy(),
                "glob": "*.rs",
            }),
            &cfg,
        )
        .expect("grep must succeed");
        assert!(!is_error, "msg: {msg}");
        assert!(msg.contains("a.rs:1:fn alpha()"), "msg: {msg}");
        assert!(
            !msg.contains("b.txt"),
            "glob filter must exclude b.txt: {msg}"
        );
        let (msg2, _) = handle_grep(
            serde_json::json!({
                "pattern": "ALPHA",
                "path": dir.to_string_lossy(),
            }),
            &cfg,
        )
        .expect("case-insensitive grep must succeed");
        assert!(
            msg2.contains("alpha beta"),
            "case-insensitive match: {msg2}"
        );
        // case_sensitive=true must NOT match lowercase content.
        let (msg3, _) = handle_grep(
            serde_json::json!({
                "pattern": "ALPHA",
                "path": dir.to_string_lossy(),
                "case_sensitive": true,
            }),
            &cfg,
        )
        .expect("case-sensitive grep must succeed");
        assert!(
            msg3.starts_with("No matches"),
            "case-sensitive search must not match lowercase: {msg3}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_invalid_regex_is_soft_error() {
        let dir = std::env::temp_dir().join(format!("fs-grep-bad-regex-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cfg = tmp_grep_cfg(&dir);
        let err = handle_grep(
            serde_json::json!({"pattern": "[", "path": dir.to_string_lossy()}),
            &cfg,
        )
        .expect_err("invalid regex must be rejected by the handler");
        assert!(err.to_string().contains("Invalid regex"), "err: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Test helper: Config whose workspace AND omni root point at a throwaway
    /// temp dir, so grep spill files land under {dir}/data/spill and never
    /// touch real OMNI_DIR paths.
    fn tmp_grep_cfg(dir: &std::path::Path) -> Config {
        Config {
            workspace_dir: dir.to_string_lossy().to_string(),
            omni_dir: dir.to_string_lossy().to_string(),
            ..Config::default()
        }
    }

    /// Test helper: throwaway temp dir + file with `content`, and a Config
    /// whose workspace is that dir so file-edit tools are allowed to write.
    fn tmp_edit_env(name: &str, content: &str) -> (std::path::PathBuf, Config) {
        let dir = std::env::temp_dir().join(format!("fs-edit-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("file.txt");
        fs::write(&p, content).unwrap();
        let cfg = Config {
            workspace_dir: dir.to_string_lossy().to_string(),
            ..Config::default()
        };
        (p, cfg)
    }

    #[test]
    fn str_replace_unique_single_match() {
        let (p, cfg) = tmp_edit_env(
            "sr-unique",
            "fn main() {\n    let x = 1;\n    let y = 2;\n}\n",
        );
        let (msg, is_error) = handle_str_replace(
            serde_json::json!({
                "path": p.to_string_lossy(),
                "old_string": "let x = 1;",
                "new_string": "let x = 10;"
            }),
            &cfg,
        )
        .expect("replace must succeed");
        assert!(!is_error, "msg: {msg}");
        let content = fs::read_to_string(&p).unwrap();
        assert!(content.contains("let x = 10;"), "content: {content}");
        assert!(!content.contains("let x = 1;"), "content: {content}");
        assert!(content.contains("let y = 2;"), "content: {content}");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn str_replace_not_found_is_an_error() {
        let (p, cfg) = tmp_edit_env("sr-notfound", "hello world\n");
        let err = handle_str_replace(
            serde_json::json!({
                "path": p.to_string_lossy(),
                "old_string": "does not exist",
                "new_string": "x"
            }),
            &cfg,
        )
        .expect_err("missing old_string must be rejected");
        assert!(err.to_string().contains("not found"), "err: {err}");
        assert_eq!(fs::read_to_string(&p).unwrap(), "hello world\n");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn str_replace_multiple_occurrences_need_disambiguation() {
        let (p, cfg) = tmp_edit_env("sr-multi", "a\nb\na\n");
        let err = handle_str_replace(
            serde_json::json!({"path": p.to_string_lossy(), "old_string": "a", "new_string": "X"}),
            &cfg,
        )
        .expect_err("ambiguous old_string must be rejected");
        assert!(err.to_string().contains("occurs 2 times"), "err: {err}");
        // occurrence=N picks exactly the Nth match.
        let (msg, is_error) = handle_str_replace(
            serde_json::json!({
                "path": p.to_string_lossy(),
                "old_string": "a",
                "new_string": "X",
                "occurrence": 2
            }),
            &cfg,
        )
        .expect("occurrence must succeed");
        assert!(!is_error, "msg: {msg}");
        assert_eq!(fs::read_to_string(&p).unwrap(), "a\nb\nX\n");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn str_replace_out_of_range_occurrence_errors() {
        let (p, cfg) = tmp_edit_env("sr-oob", "only-one\n");
        let err = handle_str_replace(
            serde_json::json!({
                "path": p.to_string_lossy(),
                "old_string": "only-one",
                "new_string": "x",
                "occurrence": 3
            }),
            &cfg,
        )
        .expect_err("occurrence beyond count must be rejected");
        assert!(err.to_string().contains("out of range"), "err: {err}");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn str_replace_empty_new_string_deletes() {
        let (p, cfg) = tmp_edit_env("sr-del", "keep me, DELETE please\n");
        let (msg, is_error) = handle_str_replace(
            serde_json::json!({
                "path": p.to_string_lossy(),
                "old_string": "DELETE",
                "new_string": ""
            }),
            &cfg,
        )
        .expect("delete must succeed");
        assert!(!is_error, "msg: {msg}");
        let content = fs::read_to_string(&p).unwrap();
        assert!(!content.contains("DELETE"), "content: {content}");
        assert!(content.contains("keep me"), "content: {content}");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn str_replace_multiline_match() {
        let (p, cfg) = tmp_edit_env("sr-ml", "fn main() {\n    let x = 1;\n    let y = 2;\n}\n");
        let (msg, is_error) = handle_str_replace(
            serde_json::json!({
                "path": p.to_string_lossy(),
                "old_string": "    let x = 1;\n    let y = 2;",
                "new_string": "    let x = 1;\n    let y = 2;\n    let z = 3;"
            }),
            &cfg,
        )
        .expect("multiline replace must succeed");
        assert!(!is_error, "msg: {msg}");
        let content = fs::read_to_string(&p).unwrap();
        assert!(content.contains("let z = 3;"), "content: {content}");
        assert_eq!(
            content,
            "fn main() {\n    let x = 1;\n    let y = 2;\n    let z = 3;\n}\n"
        );
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn str_replace_outside_sandbox_rejected() {
        let cfg = Config {
            workspace_dir: "/opt/workspace".to_string(),
            ..Config::default()
        };
        let err = handle_str_replace(
            serde_json::json!({
                "path": "/etc/hostname",
                "old_string": "x",
                "new_string": "y"
            }),
            &cfg,
        )
        .expect_err("edit outside sandbox must be rejected");
        assert!(
            err.to_string().contains("allowed write roots"),
            "err: {err}"
        );
    }

    #[test]
    fn str_replace_missing_file_errors() {
        let dir = std::env::temp_dir().join(format!("fs-edit-missing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cfg = Config {
            workspace_dir: dir.to_string_lossy().to_string(),
            ..Config::default()
        };
        let err = handle_str_replace(
            serde_json::json!({
                "path": dir.join("nope.txt").to_string_lossy(),
                "old_string": "x",
                "new_string": "y"
            }),
            &cfg,
        )
        .expect_err("editing a missing file must be rejected");
        assert!(err.to_string().contains("does not exist"), "err: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn insert_before_line_mid_file() {
        let (p, cfg) = tmp_edit_env("ins-mid", "line1\nline3\n");
        let (msg, is_error) = handle_insert(
            serde_json::json!({
                "path": p.to_string_lossy(),
                "line": 2,
                "content": "line2"
            }),
            &cfg,
        )
        .expect("insert must succeed");
        assert!(!is_error, "msg: {msg}");
        assert_eq!(fs::read_to_string(&p).unwrap(), "line1\nline2\nline3\n");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn insert_at_top_and_multiline() {
        let (p, cfg) = tmp_edit_env("ins-top", "b\nc\n");
        let (msg, is_error) = handle_insert(
            serde_json::json!({
                "path": p.to_string_lossy(),
                "line": 1,
                "content": "x\ny"
            }),
            &cfg,
        )
        .expect("insert must succeed");
        assert!(!is_error, "msg: {msg}");
        assert_eq!(fs::read_to_string(&p).unwrap(), "x\ny\nb\nc\n");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn insert_appends_at_end_of_file() {
        // File with a trailing newline: appending adds a new last line.
        let (p, cfg) = tmp_edit_env("ins-end-nl", "x\ny\n");
        let (msg, is_error) = handle_insert(
            serde_json::json!({"path": p.to_string_lossy(), "line": 3, "content": "z"}),
            &cfg,
        )
        .expect("insert must succeed");
        assert!(!is_error, "msg: {msg}");
        assert_eq!(fs::read_to_string(&p).unwrap(), "x\ny\nz");
        let _ = fs::remove_dir_all(p.parent().unwrap());

        // File WITHOUT a trailing newline: appending must still start a new line.
        let (p2, cfg2) = tmp_edit_env("ins-end-nonl", "x\ny");
        let (msg2, is_error2) = handle_insert(
            serde_json::json!({"path": p2.to_string_lossy(), "line": 3, "content": "z"}),
            &cfg2,
        )
        .expect("insert must succeed");
        assert!(!is_error2, "msg: {msg2}");
        assert_eq!(fs::read_to_string(&p2).unwrap(), "x\ny\nz");
        let _ = fs::remove_dir_all(p2.parent().unwrap());
    }

    #[test]
    fn insert_into_empty_file() {
        let (p, cfg) = tmp_edit_env("ins-empty", "");
        let (msg, is_error) = handle_insert(
            serde_json::json!({"path": p.to_string_lossy(), "line": 1, "content": "hello"}),
            &cfg,
        )
        .expect("insert into empty file must succeed");
        assert!(!is_error, "msg: {msg}");
        assert_eq!(fs::read_to_string(&p).unwrap(), "hello");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn insert_invalid_line_errors() {
        let (p, cfg) = tmp_edit_env("ins-bad", "a\nb\n");
        let err = handle_insert(
            serde_json::json!({"path": p.to_string_lossy(), "line": 5, "content": "z"}),
            &cfg,
        )
        .expect_err("line beyond last+1 must be rejected");
        assert!(err.to_string().contains("out of range"), "err: {err}");
        let err0 = handle_insert(
            serde_json::json!({"path": p.to_string_lossy(), "line": 0, "content": "z"}),
            &cfg,
        )
        .expect_err("line 0 must be rejected");
        assert!(err0.to_string().contains(">= 1"), "err: {err0}");
        let errempty = handle_insert(
            serde_json::json!({"path": p.to_string_lossy(), "line": 1, "content": ""}),
            &cfg,
        )
        .expect_err("empty content must be rejected");
        assert!(
            errempty.to_string().contains("not be empty"),
            "err: {errempty}"
        );
        assert_eq!(fs::read_to_string(&p).unwrap(), "a\nb\n");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn apply_patch_applies_ops_in_order() {
        let (p, cfg) = tmp_edit_env("ap-ok", "fn main() {\n    let a = 1;\n    let b = 2;\n}\n");
        let (msg, is_error) = handle_apply_patch(
            serde_json::json!({
                "path": p.to_string_lossy(),
                "edits": [
                    {"op": "replace", "old_string": "let a = 1;", "new_string": "let a = 10;"},
                    {"op": "replace", "old_string": "let b = 2;", "new_string": "let b = 20;"},
                    {"op": "insert", "line": 4, "content": "    println!(\"hi\");"}
                ]
            }),
            &cfg,
        )
        .expect("apply_patch must succeed");
        assert!(!is_error, "msg: {msg}");
        assert!(msg.contains("3 edit(s)"), "msg: {msg}");
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            "fn main() {\n    let a = 10;\n    let b = 20;\n    println!(\"hi\");\n}\n"
        );
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn apply_patch_delete_op_and_occurrence() {
        let (p, cfg) = tmp_edit_env("ap-del", "keep this\nDROP\nkeep that\nDROP\n");
        let (msg, is_error) = handle_apply_patch(
            serde_json::json!({
                "path": p.to_string_lossy(),
                "edits": [
                    {"op": "replace", "old_string": "DROP", "occurrence": 2},
                    {"op": "replace", "old_string": "this", "new_string": "THIS"}
                ]
            }),
            &cfg,
        )
        .expect("apply_patch with occurrence/delete must succeed");
        assert!(!is_error, "msg: {msg}");
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            "keep THIS\nDROP\nkeep that\n\n"
        );
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn apply_patch_failure_is_atomic_file_unchanged() {
        let (p, cfg) = tmp_edit_env("ap-atomic", "keep me\noriginal\n");
        let err = handle_apply_patch(
            serde_json::json!({
                "path": p.to_string_lossy(),
                "edits": [
                    {"op": "replace", "old_string": "original", "new_string": "changed"},
                    {"op": "replace", "old_string": "not present anywhere", "new_string": "x"}
                ]
            }),
            &cfg,
        )
        .expect_err("a failing op must abort the whole patch");
        assert!(err.to_string().contains("edits[1] failed"), "err: {err}");
        assert!(
            err.to_string().contains("no changes were written"),
            "err: {err}"
        );
        assert_eq!(fs::read_to_string(&p).unwrap(), "keep me\noriginal\n");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn apply_patch_bad_inputs_error() {
        let (p, cfg) = tmp_edit_env("ap-bad", "content\n");
        let err_empty = handle_apply_patch(
            serde_json::json!({"path": p.to_string_lossy(), "edits": []}),
            &cfg,
        )
        .expect_err("empty edits must be rejected");
        assert!(
            err_empty.to_string().contains("non-empty"),
            "err: {err_empty}"
        );
        let err_op = handle_apply_patch(
            serde_json::json!({
                "path": p.to_string_lossy(),
                "edits": [{"op": "frobnicate", "old_string": "content", "new_string": "x"}]
            }),
            &cfg,
        )
        .expect_err("unknown op must be rejected");
        assert!(err_op.to_string().contains("unknown op"), "err: {err_op}");
        assert_eq!(fs::read_to_string(&p).unwrap(), "content\n");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn search_outside_workspace_allowed() {
        // Searching /usr/share is a read - allowed.
        let (msg, is_error) = handle_search(
            serde_json::json!({"path": "/usr/share", "pattern": "*.md"}),
            "/opt/workspace",
        )
        .expect("search outside workspace must succeed");
        assert!(!is_error, "msg: {}", msg);
    }

    #[test]
    fn grep_hidden_and_git_ignore_flags() {
        let dir = std::env::temp_dir().join(format!("fs-grep-flags-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".hidden")).unwrap();
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::write(dir.join("top.txt"), "needle in top\n").unwrap();
        fs::write(dir.join(".hidden/h.txt"), "needle hidden\n").unwrap();
        fs::write(dir.join(".git/config"), "needle git\n").unwrap();
        fs::write(dir.join("target/out.txt"), "needle target\n").unwrap();
        let cfg = tmp_grep_cfg(&dir);
        // Default (hidden=true): dot directories ARE searched; .git/target are
        // always pruned regardless of flags.
        let (msg, _) = handle_grep(
            serde_json::json!({"pattern": "needle", "path": dir.to_string_lossy()}),
            &cfg,
        )
        .expect("grep must succeed");
        assert!(
            msg.contains("h.txt"),
            "hidden must be searched by default: {msg}"
        );
        assert!(!msg.contains(".git"), ".git must always be pruned: {msg}");
        assert!(
            !msg.contains("target"),
            "target must always be pruned: {msg}"
        );
        assert!(msg.contains("top.txt"), "msg: {msg}");
        // hidden=false: dot entries skipped.
        let (msg2, _) = handle_grep(
            serde_json::json!({"pattern": "needle", "path": dir.to_string_lossy(), "hidden": false}),
            &cfg,
        )
        .expect("grep must succeed");
        assert!(
            !msg2.contains("h.txt"),
            "hidden=false must skip dotfiles: {msg2}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_spills_overflow_to_file() {
        let dir = std::env::temp_dir().join(format!("fs-grep-spill-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // 300 matching lines in one file (plus a header line that must not match).
        let mut content = String::from("no match here\n");
        for i in 0..300 {
            content.push_str(&format!("match line number {}\n", i));
        }
        fs::write(dir.join("big.txt"), &content).unwrap();
        let cfg = tmp_grep_cfg(&dir);
        let (msg, is_error) = handle_grep(
            serde_json::json!({
                "pattern": "^match line number",
                "path": dir.to_string_lossy(),
                "max_results": 50,
            }),
            &cfg,
        )
        .expect("grep must succeed");
        assert!(!is_error, "msg: {msg}");
        assert!(msg.starts_with("Found 300 match(es)"), "msg: {msg}");
        assert!(
            msg.contains("50 shown inline"),
            "cap note must mention the inline count: {msg}"
        );
        assert!(
            msg.contains("written to"),
            "spill path must be reported: {msg}"
        );
        // The spill file under {dir}/data/spill must hold ALL 300 hits verbatim.
        let spill_dir = dir.join("data").join("spill");
        let entries: Vec<_> = fs::read_dir(&spill_dir)
            .expect("spill dir must exist")
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "exactly one spill file expected");
        let spill_content = fs::read_to_string(&entries[0]).unwrap();
        let hits = spill_content
            .lines()
            .filter(|l| l.contains("match line number"))
            .count();
        assert_eq!(
            hits, 300,
            "spill must contain ALL 300 hits, not just overflow"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_lines_mode_numbers_and_pages() {
        let dir = std::env::temp_dir().join(format!("fs-read-lines-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ten.txt");
        let body: Vec<String> = (1..=10).map(|i| format!("line {}", i)).collect();
        fs::write(&p, body.join("\n")).unwrap();
        let ws = dir.to_string_lossy().to_string();
        // Page 1: lines 1-3 with 1-based numbers and a truncation note.
        let (msg, is_error) = handle_read(
            serde_json::json!({"path": p.to_string_lossy(), "lines": true, "limit": 3}),
            &ws,
        )
        .expect("read must succeed");
        assert!(!is_error, "msg: {msg}");
        assert!(
            msg.starts_with("1:line 1\n2:line 2\n3:line 3\n\n[... truncated: showing lines 1-3 of 10 total lines]"),
            "msg: {msg}"
        );
        // Page 2: deterministic resume at line 4.
        let (msg2, _) = handle_read(
            serde_json::json!({"path": p.to_string_lossy(), "lines": true, "offset": 4, "limit": 3}),
            &ws,
        )
        .expect("read must succeed");
        assert!(
            msg2.starts_with("4:line 4\n5:line 5\n6:line 6\n\n[... truncated: showing lines 4-6 of 10 total lines]"),
            "msg: {msg2}"
        );
        // Tail page: complete note without 'truncated'.
        let (msg3, _) = handle_read(
            serde_json::json!({"path": p.to_string_lossy(), "lines": true, "offset": 9, "limit": 5}),
            &ws,
        )
        .expect("read must succeed");
        assert!(
            msg3.starts_with("9:line 9\n10:line 10\n\n[showing lines 9-10 of 10 total lines]"),
            "msg: {msg3}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_lines_mode_conventions_match_edit_tools() {
        // A trailing newline does not open an extra empty line: 'a\nb\n' has
        // exactly 2 lines (the same convention filesystem__insert uses).
        let dir = std::env::temp_dir().join(format!("fs-read-lines-c-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ab.txt");
        fs::write(&p, "a\nb\n").unwrap();
        let ws = dir.to_string_lossy().to_string();
        let (msg, is_error) = handle_read(
            serde_json::json!({"path": p.to_string_lossy(), "lines": true}),
            &ws,
        )
        .expect("read must succeed");
        assert!(!is_error, "msg: {msg}");
        assert!(
            msg.starts_with("1:a\n2:b\n\n[showing lines 1-2 of 2 total lines]"),
            "msg: {msg}"
        );
        // Empty file: self-describing note.
        let pe = dir.join("empty.txt");
        fs::write(&pe, "").unwrap();
        let (msge, _) = handle_read(
            serde_json::json!({"path": pe.to_string_lossy(), "lines": true}),
            &ws,
        )
        .expect("read must succeed");
        assert!(msge.contains("[file is empty (0 lines)]"), "msg: {msge}");
        // Offset past the end: nothing shown, note explains why.
        let (msgp, _) = handle_read(
            serde_json::json!({"path": p.to_string_lossy(), "lines": true, "offset": 5}),
            &ws,
        )
        .expect("read must succeed");
        assert!(msgp.contains("offset 5 is past the end"), "msg: {msgp}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_char_mode_is_unchanged_default() {
        // Default (no 'lines') keeps the char-based behavior: raw content,
        // no line-number prefixes, char-offset truncation note when partial.
        let dir = std::env::temp_dir().join(format!("fs-read-char-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("multi.txt");
        fs::write(&p, "hello world\nsecond line\n").unwrap();
        let ws = dir.to_string_lossy().to_string();
        let (msg, is_error) = handle_read(serde_json::json!({"path": p.to_string_lossy()}), &ws)
            .expect("read must succeed");
        assert!(!is_error, "msg: {msg}");
        assert_eq!(msg, "hello world\nsecond line\n", "msg: {msg}");
        // A truncated char read reports chars (not lines) and adds no prefixes.
        let pb = dir.join("big.txt");
        fs::write(&pb, "x".repeat(60000)).unwrap();
        let (msgb, _) = handle_read(serde_json::json!({"path": pb.to_string_lossy()}), &ws)
            .expect("read must succeed");
        assert!(
            msgb.ends_with("[... truncated: showing chars 0-50000 of 60000 total chars]"),
            "msg tail: {}",
            &msgb[msgb.len().saturating_sub(120)..]
        );
        assert!(
            !msgb.contains(":x"),
            "char mode must not prefix line numbers"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
