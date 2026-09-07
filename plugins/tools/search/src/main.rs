//! mcp-server-search: standalone MCP server for searching messages, wiki,
//! database (read-only SQL), threads, channel prompts, channels and metrics.
//! Merged plugin: former `search` + `query` + `metrics` plugins consolidated
//! into one crate. Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools:
//! - search_messages: keyword (ILIKE) message search across channels
//! - search_wiki: keyword search over the active profile's wiki
//! - search_database: free-form SELECT SQL (read-only)
//! - search_thread_messages: all messages from a thread
//! - search_channel_prompts: all seq-0 (prompt) messages from a channel
//! - search_channels: list channels (id, name, platform)
//! - search_metrics: agent metrics (token usage, latency, groundedness, ...)

use anyhow::Result;
use mcp_server_util::*;
use parking_lot::Mutex;
use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::types::chrono::{DateTime, NaiveDate, Utc};
use sqlx::types::Uuid;
use sqlx::{Column, FromRow, PgPool, Row, TypeInfo};
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Shared row types
// ---------------------------------------------------------------------------

#[derive(Debug, FromRow)]
struct SearchResult {
    id: i64,
    role: String,
    content: String,
}

#[derive(Debug, FromRow)]
struct MessageResult {
    id: i64,
    role: String,
    content: String,
    msg_type: String,
    msg_subtype: Option<String>,
    thread_id: Option<i64>,
    thread_sequence: i32,
    created_at: Option<String>,
}

/// Query result for token usage aggregation (metrics).
#[derive(Debug, sqlx::FromRow)]
struct TokenAggRow {
    profile: String,
    provider: Option<String>,
    model: Option<String>,
    total_prompt_tokens: Option<i64>,
    total_completion_tokens: Option<i64>,
    total_processing_ms: Option<i64>,
    message_count: Option<i64>,
    avg_processing_ms: Option<f64>,
}

// ---------------------------------------------------------------------------
// Plugin config - received via MCP configure message, not from env vars
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Config {
    database_url: String,
    omni_dir: String,
}

// ---------------------------------------------------------------------------
// Tool: search_messages (keyword / full-text)
// ---------------------------------------------------------------------------

/// Escape a value for safe inclusion in a single-quoted PostgreSQL string
/// literal. standard_conforming_strings=on means only single quotes need
/// doubling.
fn sql_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// Build the search_messages SQL. Matching runs on the dedicated tsvector
/// column messages.search_tsv (GIN index idx_messages_search_tsv), so the
/// query is index-driven by construction: it never depends on ANALYZE /
/// planner statistics and never ILIKE-scans or detoasts the TOAST-heavy
/// `content` column. The user query is inlined as constant arguments of
/// plainto_tsquery()/to_tsquery() so the planner const-folds them and picks
/// the GIN bitmap path even with cold statistics.
///
/// Relevance ordering: results are ranked by descending ts_rank_cd (how well
/// each message matches, rewarding tight term co-occurrence), with newest as
/// the tiebreak - previously results were newest-first regardless of match
/// quality.
///
/// Identifier precision: the index (see db-migrations messages_identifier_words)
/// also stores underscore identifiers as single joined tokens. When the query
/// contains underscore-joined runs (task ids such as
/// `task_omnidev_threads_stuck_in_processing`, `parent_by_chat`, ...), the
/// match predicate ORs the plain word-level query with the exact joined-token
/// query, and messages that contain the identifier verbatim sort FIRST, ahead
/// of merely word-related matches.
fn identifier_whole_tokens(query: &str) -> Vec<String> {
    // Maximal underscore-joined alnum runs: [A-Za-z0-9]+(_[A-Za-z0-9]+)+
    // (mirrors messages_identifier_words on the index side). Returned tokens
    // are the runs with underscores removed, e.g. "parent_by_chat" ->
    // "parentbychat". The query is scanned char-by-char so no regex dep is
    // needed and punctuation around runs (quotes, dots, commas) is skipped.
    let b = query.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_alphanumeric() {
            i += 1;
        }
        let mut groups = 1usize;
        loop {
            let mut j = i;
            if j < b.len() && b[j] == b'_' {
                j += 1;
                let k = j;
                while j < b.len() && b[j].is_ascii_alphanumeric() {
                    j += 1;
                }
                if j > k {
                    groups += 1;
                    i = j;
                    continue;
                }
            }
            break;
        }
        if groups >= 2 {
            let token: String = query[start..i]
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect();
            out.push(token);
        }
    }
    out
}

fn build_search_messages_sql(query: &str, channel_id: Option<&str>, limit: i64) -> String {
    let q = sql_quote(query);
    // Word-level english query (recall semantics unchanged from the plain FTS
    // path when the query has no underscore identifiers).
    let english_tsq = format!("plainto_tsquery('english', '{q}')");
    // Exact joined-token query for underscore identifiers (empty when the
    // query has none).
    let wholes = identifier_whole_tokens(query);
    let exact_tsq = if wholes.is_empty() {
        String::new()
    } else {
        let terms: Vec<String> = wholes
            .iter()
            .map(|w| format!("to_tsquery('english', '{}')", sql_quote(w)))
            .collect();
        terms.join(" && ")
    };
    let match_tsq = if exact_tsq.is_empty() {
        english_tsq.clone()
    } else {
        format!("({english_tsq} || ({exact_tsq}))")
    };
    // ORDER BY fragment: exact identifier matches first (when the query has
    // identifier runs), then descending relevance (ts_rank_cd), then newest
    // (created_at DESC, id DESC) as tiebreak. `alias` is the alias of the
    // messages table in that query level ("" when unaliased). created_at and
    // id MUST be alias-qualified: the channel-variant candidate stage joins
    // threads, which also has created_at/id, and an unqualified reference
    // would be ambiguous and fail the query.
    let order_clause = |alias: &str| -> String {
        let dot = if alias.is_empty() {
            String::new()
        } else {
            format!("{alias}.")
        };
        let ts_col = format!("{dot}search_tsv");
        if exact_tsq.is_empty() {
            format!("ts_rank_cd({ts_col}, {english_tsq}) DESC, {dot}created_at DESC, {dot}id DESC")
        } else {
            format!(
                "({ts_col} @@ {exact_tsq}) DESC, ts_rank_cd({ts_col}, {english_tsq}) DESC, {dot}created_at DESC, {dot}id DESC"
            )
        }
    };
    // Two-stage, bounded-by-construction shape:
    //  stage 1 (inner): find the top `limit` matching message IDs using the
    //    tsvector GIN index (idx_messages_search_tsv) - reads only search_tsv
    //    (a generated tsvector of the lean, capped searchable content), never
    //    the TOAST-heavy messages.content column, so cost is proportional to
    //    the number of matching postings, not the table size, and it works
    //    with cold/stale planner stats.
    //  stage 2 (outer): fetch full rows (content for the preview) only for
    //    those <= `limit` winners, so giant tool outputs are detoasted at most
    //    `limit` times per search. Both stages apply the same ordering so the
    //    final list stays relevance-ordered.
    match channel_id {
        Some(cid) => {
            let cid = sql_quote(cid);
            format!(
                "SELECT m.id, m.role, m.content FROM messages m              WHERE m.id IN (                  SELECT ms.id FROM messages ms                  JOIN threads t ON t.id = ms.thread_id                  WHERE t.channel_id = '{cid}'                    AND ms.search_tsv @@ {match_tsq}                  ORDER BY {}                  LIMIT {limit}              )              ORDER BY {}              LIMIT {limit}",
                order_clause("ms"),
                order_clause("m")
            )
        }
        None => format!(
            "SELECT m.id, m.role, m.content FROM messages m              WHERE m.id IN (                  SELECT im.id FROM messages im                  WHERE im.search_tsv @@ {match_tsq}                  ORDER BY {}                  LIMIT {limit}              )              ORDER BY {}              LIMIT {limit}",
            order_clause("im"),
            order_clause("m")
        ),
    }
}

async fn handle_search_messages(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'query'"))?;
    let limit = args["limit"].as_i64().unwrap_or(10).min(50);
    let channel_id = args["channel_id"].as_str().map(|s| s.to_string());

    let sql = build_search_messages_sql(query, channel_id.as_deref(), limit);
    let results: Vec<SearchResult> =
        sqlx::query_as::<_, SearchResult>(sqlx::AssertSqlSafe(sql.as_str()))
            .fetch_all(pool)
            .await
            .map_err(|e: sqlx::Error| anyhow::anyhow!("Database query failed: {e}"))?;

    if results.is_empty() {
        return Ok(("No matching messages found.".to_string(), false));
    }

    let mut lines = Vec::new();
    for r in &results {
        let preview = if r.content.len() > 200 {
            let truncate_to = r
                .content
                .char_indices()
                .nth(200)
                .map(|(i, _)| i)
                .unwrap_or(r.content.len());
            format!("{}...", &r.content[..truncate_to])
        } else {
            r.content.clone()
        };
        lines.push(format!("#{} [{}]: {}", r.id, r.role, preview));
    }

    let output = format!("Found {} result(s):\n{}", results.len(), lines.join("\n\n"));
    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Tool: search_wiki
// ---------------------------------------------------------------------------

fn handle_search_wiki(args: &Value, omni_dir: &str, profile_name: &str) -> Result<(String, bool)> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'query'"))?;
    let limit = args["limit"].as_i64().unwrap_or(10).clamp(1, 30) as usize;
    // Profile comes from the AGENT's runtime context (_meta.profile_name,
    // injected by the MCP client on every tool call) - NOT from a tool
    // argument. Only fall back to the active profile when meta is absent
    // (e.g. manual testing outside the agent).
    let profile = if profile_name.trim().is_empty() {
        omniagent::profile::default_profile_name()
    } else {
        profile_name.trim().to_string()
    };

    let wiki_dir = format!("{}/profiles/{}/wiki", omni_dir, profile);
    let wiki_dir_path = std::path::Path::new(&wiki_dir);

    if !wiki_dir_path.exists() {
        return Ok((
            format!(
                "Wiki directory not found: {}. Is the profile correct? (active profile: {})",
                wiki_dir, profile
            ),
            false,
        ));
    }

    let phrase = normalize_wiki_text(query);
    if phrase.is_empty() {
        return Ok(("No matching wiki results found.".to_string(), false));
    }
    let terms = wiki_query_terms(&phrase);

    // Scan the whole wiki once, score every page, then rank (keyword only).
    let mut hits: Vec<WikiHit> = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![wiki_dir_path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().map(|e| e == "md").unwrap_or(false) {
                let Ok(content) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let rel = path.strip_prefix(wiki_dir_path).unwrap_or(&path);
                let rel_stem = rel.with_extension("").to_string_lossy().to_string();
                if let Some(hit) = score_wiki_page(&rel_stem, &content, &terms, &phrase) {
                    hits.push(hit);
                }
            }
        }
    }

    if hits.is_empty() {
        return Ok(("No matching wiki results found.".to_string(), false));
    }

    // Rank: filename match > title/frontmatter match > body keyword frequency
    // (weights in score_wiki_page). Deterministic tie-break: page path.
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.rel_stem.cmp(&b.rel_stem))
    });
    hits.truncate(limit);

    let output = hits
        .iter()
        .map(render_wiki_hit)
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok((output, false))
}

/// Lowercase free text and collapse every run of non-alphanumeric characters
/// into a single space. Hyphen/underscore identifiers stay searchable
/// ("task_a_b" -> "task a b"); punctuation never joins words.
fn normalize_wiki_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

/// Unique query words in first-appearance order, capped at 10 so a very long
/// query cannot skew scoring or cost.
fn wiki_query_terms(phrase: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tok in phrase.split(' ') {
        if tok.is_empty() || out.iter().any(|t| t == tok) {
            continue;
        }
        out.push(tok.to_string());
        if out.len() >= 10 {
            break;
        }
    }
    out
}

struct WikiPageMeta<'a> {
    description: Option<String>,
    title: Option<String>,
    body: &'a str,
}

/// Minimal frontmatter reader for wiki pages. A page may start with a `---`
/// fence followed by `key: value` lines until the closing `---` fence. Only
/// `description`, `title` and `name` are read (name doubles as title only
/// when no explicit title exists). Returns the body after the closing fence.
fn read_wiki_frontmatter(content: &str) -> WikiPageMeta<'_> {
    let mut out = WikiPageMeta {
        description: None,
        title: None,
        body: content,
    };
    if content.is_empty() {
        return out;
    }
    // First line must be the opening fence "---".
    let first_nl = content.find('\n').unwrap_or(content.len());
    if content[..first_nl].trim_end_matches('\r').trim() != "---" {
        return out;
    }
    let mut scan = first_nl + 1;
    let total = content.len();
    while scan < total {
        let nl = match content[scan..].find('\n') {
            Some(rel) => scan + rel + 1,
            None => total,
        };
        let line = content[scan..nl].trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed == "---" {
            out.body = &content[nl..];
            return out;
        }
        if let Some(colon) = trimmed.find(':') {
            let key = trimmed[..colon].trim().to_ascii_lowercase();
            let raw = trimmed[colon + 1..].trim();
            if !raw.is_empty() {
                let value = if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
                    raw[1..raw.len() - 1].trim().to_string()
                } else {
                    raw.to_string()
                };
                if !value.is_empty() {
                    match key.as_str() {
                        "description" if out.description.is_none() => out.description = Some(value),
                        "title" if out.title.is_none() => out.title = Some(value),
                        "name" if out.title.is_none() => out.title = Some(value),
                        _ => {}
                    }
                }
            }
        }
        scan = nl;
    }
    // No closing fence found: treat the whole file as frontmatter.
    out.body = "";
    out
}

/// First markdown heading (`# ...` or deeper) in the body, trimmed.
fn first_heading(body: &str) -> Option<String> {
    body.lines().find_map(|l| {
        let t = l.trim();
        if t.starts_with('#') {
            let title = t.trim_start_matches('#').trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
        None
    })
}

/// Clip `s` to at most `max_chars` characters on a char boundary, appending
/// "..." when it was clipped.
fn clip_wiki_text(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut = s
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    format!("{}...", &s[..cut])
}

/// Up to two body lines that best explain the match: lines containing the
/// most query terms first, earliest line on ties. Trimmed and clipped.
fn wiki_match_lines(body: &str, terms: &[String]) -> Vec<String> {
    let mut candidates: Vec<(i32, usize, String)> = Vec::new();
    for (idx, raw) in body.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_lowercase();
        let hits = terms.iter().filter(|t| lower.contains(t.as_str())).count() as i32;
        if hits > 0 {
            candidates.push((hits, idx, line.to_string()));
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    candidates
        .into_iter()
        .take(2)
        .map(|(_, _, line)| clip_wiki_text(&line, 120))
        .collect()
}

/// Number of query terms present as whole words in a normalized field.
fn field_term_matches(field: &str, terms: &[String]) -> usize {
    terms
        .iter()
        .filter(|t| field.split(' ').any(|w| w == t.as_str()))
        .count()
}

struct WikiHit {
    rel_stem: String,
    description: Option<String>,
    match_lines: Vec<String>,
    score: i64,
}

/// Score one wiki page against the query; None when nothing matched.
///
/// Ranking (keyword only, no semantic layer): filename match > title /
/// frontmatter match > body keyword frequency. The score is a weighted sum
/// whose tiers are spaced so a full-phrase hit in a LOWER field (e.g. the
/// exact query as a page title) still outranks a partial generic-token hit in
/// a HIGHER field (a filename sharing one common word).
fn score_wiki_page(
    rel_stem: &str,
    content: &str,
    terms: &[String],
    phrase: &str,
) -> Option<WikiHit> {
    let fm = read_wiki_frontmatter(content);
    let title = fm
        .title
        .clone()
        .or_else(|| first_heading(fm.body))
        .unwrap_or_default();
    let f_norm = normalize_wiki_text(rel_stem);
    let t_norm = normalize_wiki_text(&title);
    let d_norm = fm
        .description
        .as_deref()
        .map(normalize_wiki_text)
        .unwrap_or_default();
    let body_lower = fm.body.to_lowercase();

    let f_phrase = f_norm.contains(phrase);
    let t_phrase = t_norm.contains(phrase);
    let d_phrase = d_norm.contains(phrase);
    let f_matched = field_term_matches(&f_norm, terms);
    let t_matched = field_term_matches(&t_norm, terms);
    let d_matched = field_term_matches(&d_norm, terms);

    let body_occ: i64 = terms
        .iter()
        .map(|t| count_substring_ci(&body_lower, t).min(250))
        .sum::<i64>()
        .min(500);

    let mut score: i64 = 0;
    if f_phrase {
        score += 10_000_000;
    } else if f_matched >= terms.len() {
        // Every query word present in the filename (but not as one phrase).
        score += 5_000_000;
    }
    score += (f_matched.min(10) as i64) * 100_000;
    if t_phrase {
        score += 500_000;
    }
    score += (t_matched.min(10) as i64) * 20_000;
    if d_phrase {
        score += 100_000;
    }
    score += (d_matched.min(10) as i64) * 10_000;
    score += body_occ * 20;

    if score == 0 {
        return None;
    }
    let match_lines = wiki_match_lines(fm.body, terms);
    Some(WikiHit {
        rel_stem: rel_stem.to_string(),
        description: fm.description,
        match_lines,
        score,
    })
}

/// Count case-insensitive substring occurrences of `needle` in `hay_lower`
/// (which must already be lowercase). Plain substring counting keeps stems
/// searchable ("budget" hits "budget-cap").
fn count_substring_ci(hay_lower: &str, needle: &str) -> i64 {
    if needle.is_empty() || hay_lower.is_empty() {
        return 0;
    }
    let mut count: i64 = 0;
    let mut start = 0;
    while let Some(rel) = hay_lower[start..].find(needle) {
        count += 1;
        start += rel + needle.len();
    }
    count
}

/// Render one hit: `[[path]]: description` plus up to two indented
/// `...matching line...` snippets. Output stays bounded: description 220
/// chars, each line 120 chars.
fn render_wiki_hit(hit: &WikiHit) -> String {
    let mut out = format!("[[{}]]", hit.rel_stem);
    if let Some(desc) = &hit.description {
        out.push_str(": ");
        out.push_str(&clip_wiki_text(desc, 220));
    } else if !hit.match_lines.is_empty() {
        out.push(':');
    }
    for line in &hit.match_lines {
        out.push_str("\n    ...");
        out.push_str(line);
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod search_wiki_tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestWiki {
        root: PathBuf,
    }

    impl TestWiki {
        fn new() -> Self {
            let uniq = format!(
                "s4_wiki_{}_{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let root = std::env::temp_dir().join(uniq);
            fs::create_dir_all(root.join("profiles/test/wiki")).expect("create test wiki dir");
            TestWiki { root }
        }

        fn wiki_dir(&self) -> PathBuf {
            self.root.join("profiles/test/wiki")
        }

        fn write(&self, rel: &str, content: &str) {
            let path = self.wiki_dir().join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent dir");
            }
            fs::write(&path, content).expect("write wiki page");
        }

        fn search(&self, query: &str, limit: i64) -> String {
            let args = json!({ "query": query, "limit": limit });
            let omni = self.root.to_str().expect("utf8 temp path").to_string();
            handle_search_wiki(&args, &omni, "test")
                .expect("search_wiki runs")
                .0
        }
    }

    impl Drop for TestWiki {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// `[[...]]` result markers in output order.
    fn result_stems(output: &str) -> Vec<String> {
        output
            .split("\n\n")
            .filter_map(|block| {
                let block = block.trim_start();
                if block.starts_with("[[") {
                    let end = block.find("]]").map(|i| i + 2).unwrap_or(0);
                    Some(block[..end].to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn filename_phrase_ranks_first() {
        let w = TestWiki::new();
        w.write(
            "Alpha/Telemetry-Fix.md",
            "# Telemetry Fix\n\nSample the sampler less often.\n",
        );
        w.write(
            "Alpha/Other.md",
            "# Telemetry Fix Overview\n\nDeep dive into every telemetry fix idea.\n",
        );
        let out = w.search("telemetry fix", 10);
        let stems = result_stems(&out);
        assert_eq!(stems.len(), 2, "both pages match: {out}");
        assert!(
            stems[0].contains("Telemetry-Fix"),
            "filename phrase match ranks first, got: {stems:?}\n{out}"
        );
        assert!(stems[1].contains("Other"), "stems: {stems:?}");
    }

    #[test]
    fn title_phrase_outranks_body_frequency() {
        let w = TestWiki::new();
        w.write(
            "a/Guide.md",
            "---\ntitle: Release rollback runbook\n---\n# Guide\n\nOne paragraph.\n",
        );
        let mut scratch = String::from("# Scratch\n\n");
        for _ in 0..40 {
            scratch.push_str("rollback runbook drill notes.\n");
        }
        w.write("b/Scratch.md", &scratch);
        let out = w.search("rollback runbook", 10);
        let stems = result_stems(&out);
        assert_eq!(stems.len(), 2, "got: {out}");
        assert!(
            stems[0].contains("Guide"),
            "title phrase beats body frequency: {stems:?}\n{out}"
        );
        assert!(stems[1].contains("Scratch"), "stems: {stems:?}");
    }

    #[test]
    fn frontmatter_description_matches_and_is_rendered() {
        let w = TestWiki::new();
        w.write(
            "Memory/Deepseek.md",
            "---\nname: deepseek\nconfidence: high\ndescription: \"High confidence memory about the deepseek prefix cache setup\"\n---\n# Memory\n\nBody text with unrelated words.\n",
        );
        let out = w.search("prefix cache", 10);
        assert!(
            out.contains("[[Memory/Deepseek]]"),
            "desc match returned: {out}"
        );
        assert!(
            out.contains("deepseek prefix cache setup"),
            "frontmatter description rendered in snippet: {out}"
        );
    }

    #[test]
    fn no_match_returns_not_found() {
        let w = TestWiki::new();
        w.write("x.md", "# Anything\n\nNothing to see here.\n");
        let out = w.search("zzqqxxyy", 10);
        assert!(
            out.contains("No matching wiki results found."),
            "got: {out}"
        );
    }

    #[test]
    fn limit_is_respected_with_stable_order() {
        let w = TestWiki::new();
        for i in 1..=3 {
            let content = format!("# Notes {i}\n\nBudget cap increase for everyone.\n");
            w.write(&format!("f{i}.md"), &content);
        }
        let out = w.search("budget cap", 2);
        let stems = result_stems(&out);
        assert_eq!(stems.len(), 2, "limit=2 respected: {out}");
        assert!(
            stems[0].contains("f1") && stems[1].contains("f2"),
            "stable alphabetical tie-break: {stems:?}"
        );
    }

    #[test]
    fn case_and_punctuation_insensitive() {
        let w = TestWiki::new();
        w.write("Budgeting.md", "# Budgets\n\nRaise the BUDGET-CAP to 5k.\n");
        let out = w.search("BUDGET-CAP!", 10);
        assert!(
            out.contains("[[Budgeting]]"),
            "punctuation/case tolerant: {out}"
        );
        assert!(
            out.contains("BUDGET-CAP"),
            "matching line in snippet: {out}"
        );
    }
}

// ---------------------------------------------------------------------------
// Tool: search_thread_messages
// ---------------------------------------------------------------------------

async fn handle_search_thread_messages(
    pool: &PgPool,
    args: &Value,
    meta: Option<&McpMeta>,
) -> Result<(String, bool)> {
    let thread_id = args["thread_id"]
        .as_i64()
        .or_else(|| meta.and_then(|m| m.thread_id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "'thread_id' is required for search_thread_messages (no current thread in \
                 context). Pass thread_id explicitly."
            )
        })?;
    let limit = args["limit"].as_i64().unwrap_or(100).min(200);

    let rows: Vec<MessageResult> = {
        sql_forge!(
            MessageResult,
            r#"
            SELECT
                id, role, content, msg_type, msg_subtype,
                thread_id, thread_sequence,
                COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
            FROM messages
            WHERE thread_id = :thread_id
            ORDER BY thread_sequence ASC, created_at ASC
            LIMIT :limit
            "#,
            ( :thread_id = thread_id, :limit = limit )
        )
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!(e))?
    };

    Ok(format_results(
        "search_thread_messages",
        &rows,
        rows.len() as i64,
    ))
}

// ---------------------------------------------------------------------------
// Tool: search_channel_prompts
// ---------------------------------------------------------------------------

async fn handle_search_channel_prompts(
    pool: &PgPool,
    args: &Value,
    meta: Option<&McpMeta>,
) -> Result<(String, bool)> {
    let channel_id = args["channel_id"]
        .as_str()
        .map(String::from)
        .or_else(|| meta.and_then(|m| m.channel_id.clone()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "'channel_id' is required for search_channel_prompts (no current channel in \
                 context). Pass channel_id explicitly or use search_channels to find it."
            )
        })?;
    let limit = args["limit"].as_i64().unwrap_or(10).min(50);

    let results: Vec<MessageResult> = {
        sql_forge!(
            MessageResult,
            r#"
            SELECT
                m.id, m.role, m.content, m.msg_type, m.msg_subtype,
                m.thread_id, m.thread_sequence,
                COALESCE(TO_CHAR(m.created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
            FROM messages m
            JOIN threads t ON t.id = m.thread_id
            WHERE t.channel_id = :channel_id
              AND m.thread_sequence = 0
            ORDER BY id DESC
            LIMIT :limit
            "#,
            ( :channel_id = &channel_id, :limit = limit )
        )
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!(e))?
    };

    Ok(format_results(
        "search_channel_prompts",
        &results,
        results.len() as i64,
    ))
}

// ---------------------------------------------------------------------------
// Tool: search_database (free-form read-only SELECT) - SQL safety helpers
// ---------------------------------------------------------------------------

/// Write/DDL SQL keywords that are forbidden in read-only queries. Matching is
/// done on whole tokens after stripping comments and string literals.
const WRITE_KEYWORDS: &[&str] = &[
    "INSERT",
    "UPDATE",
    "DELETE",
    "DROP",
    "ALTER",
    "CREATE",
    "TRUNCATE",
    "GRANT",
    "REVOKE",
    "MERGE",
    "CALL",
    "COPY",
    "LOCK",
    "COMMENT",
    "SET",
    "RESET",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "END",
    "DO",
    "VACUUM",
    "ANALYZE",
    "REINDEX",
    "CLUSTER",
    "NOTIFY",
    "LISTEN",
    "UNLISTEN",
    "PREPARE",
    "EXECUTE",
    "DEALLOCATE",
    "SECURITY",
    "IMPORT",
    "REFRESH",
    "DISCARD",
    "CHECKPOINT",
    "DECLARE",
    "FETCH",
    "MOVE",
    "CLOSE",
    "OPEN",
    "LOAD",
];

/// Strips SQL comments and string/identifier literals, replacing their contents
/// with spaces so keywords inside them can't create false positives (or hide).
fn strip_sql_literals_and_comments(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    while i < bytes.len() {
        if !in_line_comment
            && !in_block_comment
            && bytes[i] == b'-'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'-'
        {
            in_line_comment = true;
            out.extend_from_slice(b"  ");
            i += 2;
            continue;
        }
        if !in_line_comment
            && !in_block_comment
            && bytes[i] == b'/'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'*'
        {
            in_block_comment = true;
            out.extend_from_slice(b"  ");
            i += 2;
            continue;
        }
        if in_line_comment {
            if bytes[i] == b'\n' {
                in_line_comment = false;
                out.push(b'\n');
            } else {
                out.push(b' ');
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                out.extend_from_slice(b"  ");
                i += 2;
            } else {
                out.push(b' ');
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'\'' {
            out.push(b' ');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        out.extend_from_slice(b"  ");
                        i += 2;
                        continue;
                    }
                    out.push(b' ');
                    i += 1;
                    break;
                }
                out.push(b' ');
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'"' {
            out.push(b' ');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                        out.extend_from_slice(b"  ");
                        i += 2;
                        continue;
                    }
                    out.push(b' ');
                    i += 1;
                    break;
                }
                out.push(b' ');
                i += 1;
            }
            continue;
        }
        // Dollar-quoted string: $tag$ ... $tag$
        if bytes[i] == b'$' {
            if let Some(rel) = sql[i + 1..].find('$') {
                let tag_end = i + 1 + rel;
                let end_tag = format!("${}$", &sql[i + 1..tag_end]);
                if let Some(body_rel) = sql[tag_end + 1..].find(&end_tag) {
                    let abs_end = tag_end + 1 + body_rel + end_tag.len();
                    out.extend(std::iter::repeat_n(b' ', abs_end - i));
                    i = abs_end;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| sql.to_string())
}

/// Returns the first forbidden keyword found in the cleaned SQL, if any.
fn find_write_keyword(cleaned: &str) -> Option<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in cleaned.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cur.push(ch.to_ascii_uppercase());
        } else if !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
        .into_iter()
        .find(|t| WRITE_KEYWORDS.contains(&t.as_str()))
}

/// Max rows returned by search_database.
const MAX_QUERY_ROWS: usize = 1000;

/// Statement timeout (ms) applied to every search_database statement via SET
/// LOCAL. search_database is for structured aggregations only: message-content
/// lookups belong to search_messages (tsvector over messages.search_tsv). An
/// ILIKE '%term%' scan over messages.content has no usable index and costs
/// ~30 s per call, so this 8 s cap makes that mistake fail fast with a hint
/// instead of blocking the thread.
const SEARCH_DB_STATEMENT_TIMEOUT_MS: i64 = 8000;

/// Static SET LOCAL statement run inside the read-only transaction before the
/// user query. Static on purpose: sqlx only accepts literal-safe SQL here, and
/// the value is a compile-time constant (never user input).
const SEARCH_DB_TIMEOUT_SQL: &str = "SET LOCAL statement_timeout = 8000";

/// Slow-query log threshold (ms): any search_database statement slower than
/// this is logged (with its SQL) so costly scans stay visible.
const SEARCH_DB_SLOW_QUERY_LOG_MS: u128 = 2000;

/// True when a sqlx error text reports a PostgreSQL statement-timeout
/// cancellation (SQLSTATE 57014, "canceling statement due to statement
/// timeout").
fn is_statement_timeout_error(err_text: &str) -> bool {
    let lower = err_text.to_lowercase();
    lower.contains("statement timeout") || lower.contains("57014")
}

/// Hint returned when the statement-timeout guard fires: tells the agent to
/// use search_messages (tsvector) for content lookups instead of running
/// ILIKE scans over messages.content.
fn search_db_timeout_hint() -> String {
    format!(
        "search_database query canceled after {SEARCH_DB_STATEMENT_TIMEOUT_MS} ms by the \
         statement-timeout guard (8 s). If you were searching for message CONTENT, do not \
         use ILIKE '%..%' over messages.content: that full-table scan has no usable index \
         and costs ~30 s per call. Use search_messages instead: it searches the \
         GIN-indexed messages.search_tsv tsvector column and returns in well under 3 s. For \
         structured aggregations, add a tighter WHERE clause and a LIMIT."
    )
}

/// Collapse a SQL statement to one bounded line for slow-query logs.
fn one_line_sql(sql: &str) -> String {
    let joined: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() > 200 {
        let head: String = joined.chars().take(200).collect();
        format!("{head}...")
    } else {
        joined
    }
}

/// Decode a single result cell by its PostgreSQL column type so timestamps,
/// UUIDs, JSONB, bytea and arrays serialize as real values instead of NULL.
fn decode_column_value(row: &sqlx::postgres::PgRow, i: usize) -> serde_json::Value {
    let type_name = row.column(i).type_info().name();
    match type_name {
        "TIMESTAMPTZ" | "TIMESTAMP" => match row.try_get::<Option<DateTime<Utc>>, _>(i) {
            Ok(Some(dt)) => serde_json::Value::String(dt.to_rfc3339()),
            _ => serde_json::Value::Null,
        },
        "DATE" => match row.try_get::<Option<NaiveDate>, _>(i) {
            Ok(Some(d)) => serde_json::Value::String(d.to_string()),
            _ => serde_json::Value::Null,
        },
        "UUID" => match row.try_get::<Option<Uuid>, _>(i) {
            Ok(Some(u)) => serde_json::Value::String(u.to_string()),
            _ => serde_json::Value::Null,
        },
        "JSONB" | "JSON" => match row.try_get::<Option<serde_json::Value>, _>(i) {
            Ok(Some(v)) => v,
            _ => serde_json::Value::Null,
        },
        "BYTEA" => match row.try_get::<Option<Vec<u8>>, _>(i) {
            Ok(Some(b)) => serde_json::Value::String(
                b.iter()
                    .map(|byte| format!("{:02x}", byte))
                    .collect::<String>(),
            ),
            _ => serde_json::Value::Null,
        },
        // Arrays: PostgreSQL type names start with an underscore.
        _ if type_name.starts_with('_') => decode_array_value(row, i),
        // Everything else: try scalar decodes in order of likelihood.
        _ => {
            if let Ok(s) = row.try_get::<&str, _>(i) {
                serde_json::Value::String(s.to_string())
            } else if let Ok(n) = row.try_get::<i64, _>(i) {
                serde_json::json!(n)
            } else if let Ok(n) = row.try_get::<f64, _>(i) {
                serde_json::json!(n)
            } else if let Ok(b) = row.try_get::<bool, _>(i) {
                serde_json::json!(b)
            } else {
                row.try_get::<Option<String>, _>(i)
                    .ok()
                    .flatten()
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null)
            }
        }
    }
}

/// Decode a PostgreSQL array column into a JSON array. Tries the common
/// element types in order; falls back to NULL for exotic element types.
fn decode_array_value(row: &sqlx::postgres::PgRow, i: usize) -> serde_json::Value {
    if let Ok(Some(v)) = row.try_get::<Option<Vec<String>>, _>(i) {
        return serde_json::Value::Array(v.into_iter().map(serde_json::Value::String).collect());
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<i64>>, _>(i) {
        return serde_json::Value::Array(v.into_iter().map(|n| serde_json::json!(n)).collect());
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<f64>>, _>(i) {
        return serde_json::Value::Array(v.into_iter().map(|n| serde_json::json!(n)).collect());
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<bool>>, _>(i) {
        return serde_json::Value::Array(v.into_iter().map(|b| serde_json::json!(b)).collect());
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<Uuid>>, _>(i) {
        return serde_json::Value::Array(
            v.into_iter()
                .map(|u| serde_json::Value::String(u.to_string()))
                .collect(),
        );
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<DateTime<Utc>>>, _>(i) {
        return serde_json::Value::Array(
            v.into_iter()
                .map(|dt| serde_json::Value::String(dt.to_rfc3339()))
                .collect(),
        );
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<serde_json::Value>>, _>(i) {
        return serde_json::Value::Array(v);
    }
    serde_json::Value::Null
}

async fn handle_search_database(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let sql_owned = args["sql"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("'sql' is required for search_database operation"))?
        .to_string();

    // ── Read-only enforcement (defense in depth) ──────────────────────────
    // 1) The statement must START with SELECT or WITH (token-level check).
    // 2) Write/DDL keywords are rejected ANYWHERE in the statement, after
    //    stripping comments and string literals. This blocks data-modifying
    //    CTEs such as `WITH x AS (DELETE FROM messages RETURNING *) SELECT ...`.
    // 3) `AssertSqlSafe` is a sqlx MARKER type, not a semicolon validator;
    //    multi-statement SQL is rejected by the extended query protocol.
    // 4) The statement runs inside an explicit `BEGIN TRANSACTION READ ONLY`.
    let plain = strip_sql_literals_and_comments(&sql_owned);
    let first = plain.split_whitespace().next().unwrap_or("").to_uppercase();
    if first != "SELECT" && first != "WITH" {
        anyhow::bail!(
            "Only SELECT (or WITH) statements are allowed (statement must start with SELECT or WITH)."
        );
    }
    if let Some(bad) = find_write_keyword(&plain) {
        anyhow::bail!(
            "Query rejected: write/DDL keyword '{bad}' is not allowed in read-only queries."
        );
    }

    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to acquire connection: {e}"))?;
    sqlx::query("BEGIN TRANSACTION READ ONLY")
        .execute(&mut *conn)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to begin read-only transaction: {e}"))?;

    // Latency guard (slowness fix #3): SET LOCAL statement_timeout bounds every
    // single statement to 8 s (transaction-scoped, rolled back with it). An
    // ILIKE '%term%' scan over messages.content has no usable index and costs
    // ~30 s per call (verified 2026-09-07); the guard makes that mistake fail
    // fast with a search_messages hint instead of blocking the thread.
    sqlx::query(SEARCH_DB_TIMEOUT_SQL)
        .execute(&mut *conn)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to set statement timeout: {e}"))?;

    let query_started = std::time::Instant::now();

    let results: Vec<serde_json::Value> = {
        let rows = match sqlx::query(sqlx::AssertSqlSafe(sql_owned.as_str()))
            .fetch_all(&mut *conn)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                let err_text = e.to_string();
                let elapsed_ms = query_started.elapsed().as_millis();
                if is_statement_timeout_error(&err_text) {
                    eprintln!(
                        "[search_database] statement timeout after {elapsed_ms} ms: {}",
                        one_line_sql(&sql_owned)
                    );
                    return Err(anyhow::anyhow!(
                        "{}\n\nOriginal error: {}",
                        search_db_timeout_hint(),
                        err_text
                    ));
                }
                if elapsed_ms > SEARCH_DB_SLOW_QUERY_LOG_MS {
                    eprintln!(
                        "[search_database] failed slow query ({elapsed_ms} ms): {}",
                        one_line_sql(&sql_owned)
                    );
                }
                return Err(anyhow::anyhow!("Query failed: {err_text}"));
            }
        };

        let mut json_rows: Vec<serde_json::Value> = Vec::new();
        for row in &rows {
            let mut map = serde_json::Map::new();
            for (i, col) in row.columns().iter().enumerate() {
                let name = col.name();
                let value = decode_column_value(row, i);
                map.insert(name.to_string(), value);
            }
            json_rows.push(serde_json::Value::Object(map));
        }
        json_rows
    };

    // Defense in depth: cap the result set regardless of the caller's LIMIT.
    let results = results.into_iter().take(MAX_QUERY_ROWS).collect::<Vec<_>>();

    sqlx::query("COMMIT")
        .execute(&mut *conn)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to commit read-only transaction: {e}"))?;

    let elapsed_ms = query_started.elapsed().as_millis();
    if elapsed_ms > SEARCH_DB_SLOW_QUERY_LOG_MS {
        eprintln!(
            "[search_database] slow query ({elapsed_ms} ms): {}",
            one_line_sql(&sql_owned)
        );
    }

    let output = serde_json::to_string_pretty(&results)?;
    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Tool: search_channels
// ---------------------------------------------------------------------------

async fn handle_search_channels(_pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let limit = args["limit"].as_i64().unwrap_or(50).min(200) as usize;

    // Channels live in {data_dir}/config/channels.yml now (id == name).
    let channels = omniagent::channels_yaml::find_all()
        .map_err(|e| anyhow::anyhow!("Failed to load channels.yml: {e}"))?;

    if channels.is_empty() {
        return Ok(("[search_channels] No channels found.".to_string(), false));
    }

    let mut lines = vec![format!("[search_channels] {} channel(s):", channels.len())];
    for (name, def) in channels.into_iter().take(limit) {
        lines.push(format!(
            "#{} {} (platform: {})",
            name,
            name,
            def.platform.as_deref().unwrap_or("")
        ));
    }
    Ok((lines.join("\n"), false))
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Format a list of MessageResult into a readable string.
fn format_results(operation: &str, results: &[MessageResult], total_count: i64) -> (String, bool) {
    if results.is_empty() {
        return (format!("[{}] No results found.", operation), false);
    }

    let mut lines = Vec::new();
    lines.push(format!(
        "[{}] {} result(s) (showing {}):",
        operation,
        total_count,
        results.len()
    ));
    lines.push(String::new());

    for r in results {
        let preview = if r.content.len() > 300 {
            let truncate_to = r
                .content
                .char_indices()
                .nth(300)
                .map(|(i, _)| i)
                .unwrap_or(r.content.len());
            format!("{}...", &r.content[..truncate_to])
        } else {
            r.content.clone()
        };

        let thread_info = match (r.thread_id, r.thread_sequence) {
            (Some(tid), seq) => format!(" thread={} seq={}", tid, seq),
            (None, 0) => " root".to_string(),
            (None, seq) => format!(" seq={}", seq),
        };

        let type_info = match r.msg_subtype.as_deref() {
            Some(sub) if r.msg_type == "tool" => format!(" [tool:{}]", sub),
            Some(sub) if r.msg_type == "tool_result" => format!(" [result:{}]", sub),
            _ if r.msg_type == "reasoning" => " [reasoning]".to_string(),
            _ if r.msg_type == "summary" => " [summary]".to_string(),
            _ => String::new(),
        };

        let created_display = r
            .created_at
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|t| format!(" @{}", t))
            .unwrap_or_default();

        lines.push(format!(
            "#{} [{}]{} {}{}: {}",
            r.id, r.role, type_info, thread_info, created_display, preview
        ));
    }

    let output = lines.join("\n");
    (output, false)
}

// ---------------------------------------------------------------------------
// Tool: search_metrics
// ---------------------------------------------------------------------------

/// Aggregate metrics from the messages table.
async fn aggregate_metrics(
    pool: &PgPool,
    hours: i64,
    profile_filter: &str,
) -> Result<Vec<TokenAggRow>> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours);

    let rows: Vec<TokenAggRow> = sql_forge!(
        TokenAggRow,
        r#"
        SELECT
            t.profile,
            t.provider,
            t.model,
            SUM(t.input_tokens)::bigint AS total_prompt_tokens,
            SUM(t.output_tokens)::bigint AS total_completion_tokens,
            SUM(t.duration_ms)::bigint AS total_processing_ms,
            COUNT(*)::bigint AS message_count,
            AVG(t.duration_ms)::float AS avg_processing_ms
        FROM threads t
        JOIN messages m ON m.thread_id = t.id
        WHERE m.role = 'agent'
          AND m.msg_type IN ('message', 'summary')
          AND m.created_at >= :cutoff
          AND (:profile_filter = '' OR t.profile = :profile_filter)
        GROUP BY t.profile, t.provider, t.model
        ORDER BY total_processing_ms DESC
        "#,
        ( :cutoff = cutoff, :profile_filter = profile_filter )
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Count how many agent responses have evidence/grounding metadata.
async fn count_grounded_responses(
    pool: &PgPool,
    hours: i64,
    profile_filter: &str,
) -> Result<(i64, i64)> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours);

    let total: Option<i64> = sql_forge!(
        scalar Option<i64>,
        r#"
        SELECT COUNT(*)::bigint
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        WHERE m.role = 'agent'
          AND m.msg_type IN ('message', 'summary')
          AND m.created_at >= :cutoff
          AND (:profile_filter = '' OR t.profile = :profile_filter)
        "#,
        ( :cutoff = cutoff, :profile_filter = profile_filter )
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten();

    let grounded: Option<i64> = sql_forge!(
        scalar Option<i64>,
        r#"
        SELECT COUNT(*)::bigint
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        WHERE m.role = 'agent'
          AND m.msg_type IN ('message', 'summary')
          AND m.created_at >= :cutoff
          AND (m.metadata->'context'->>'total_chars') IS NOT NULL
          AND (:profile_filter = '' OR t.profile = :profile_filter)
        "#,
        ( :cutoff = cutoff, :profile_filter = profile_filter )
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten();

    Ok((total.unwrap_or(0), grounded.unwrap_or(0)))
}

/// Count retrieval events (how often search tools were called).
async fn count_retrieval_events(pool: &PgPool, hours: i64, profile_filter: &str) -> Result<i64> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours);

    let count: Option<i64> = sql_forge!(
        scalar Option<i64>,
        r#"
        SELECT COUNT(*)::bigint
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        WHERE m.role = 'agent'
          AND m.msg_type = 'tool'
          AND (m.content LIKE 'search_messages:%' OR m.content LIKE 'search_wiki:%')
          AND m.created_at >= :cutoff
          AND (:profile_filter = '' OR t.profile = :profile_filter)
        "#,
        ( :cutoff = cutoff, :profile_filter = profile_filter )
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten();

    Ok(count.unwrap_or(0))
}

/// Count user corrections (proxies for hallucination).
async fn count_corrections(pool: &PgPool, hours: i64, profile_filter: &str) -> Result<i64> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours);

    let count: Option<i64> = sql_forge!(
        scalar Option<i64>,
        r#"
        WITH agent_responses AS (
            SELECT m.id, t.channel_id, m.thread_id, m.created_at
            FROM messages m
            JOIN threads t ON t.id = m.thread_id
            WHERE m.role = 'agent'
              AND m.msg_type IN ('message', 'summary')
              AND m.created_at >= :cutoff
              AND (:profile_filter = '' OR t.profile = :profile_filter)
        )
        SELECT COUNT(DISTINCT m.id)::bigint
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        INNER JOIN agent_responses a
            ON t.channel_id = a.channel_id
            AND m.thread_id = a.thread_id
            AND m.created_at > a.created_at
            AND m.created_at <= a.created_at + INTERVAL '5 minutes'
        WHERE m.role = 'user'
          AND (
              LOWER(m.content) LIKE '%wrong%'
              OR LOWER(m.content) LIKE '%incorrect%'
              OR LOWER(m.content) LIKE '%that''s not%'
              OR LOWER(m.content) LIKE '%actually%'
              OR LOWER(m.content) LIKE '%no,%'
              OR LOWER(m.content) LIKE '%not what%'
              OR LOWER(m.content) LIKE '%try again%'
          )
        "#,
        ( :cutoff = cutoff, :profile_filter = profile_filter )
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten();

    Ok(count.unwrap_or(0))
}

async fn handle_search_metrics(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let hours = args.get("hours").and_then(|v| v.as_i64()).unwrap_or(24);
    let profile = args.get("profile").and_then(|v| v.as_str());
    let profile_owned = profile.map(|s| s.to_string()).unwrap_or_default();

    let usage = aggregate_metrics(pool, hours, &profile_owned).await?;
    let (total_responses, grounded_responses) =
        count_grounded_responses(pool, hours, &profile_owned).await?;
    let retrieval_count = count_retrieval_events(pool, hours, &profile_owned).await?;
    let correction_count = count_corrections(pool, hours, &profile_owned).await?;

    let mut report = format!(
        "# Agent Metrics Report\n\nPeriod: **last {} hour(s)**\n\n",
        hours
    );

    if let Some(p) = profile {
        report.push_str(&format!("Profile filter: **{}**\n\n", p));
    }

    // Summary
    let grounded_pct = if total_responses > 0 {
        (grounded_responses as f64 / total_responses as f64 * 100.0) as u32
    } else {
        0
    };

    report.push_str("## Summary\n\n");
    report.push_str(&format!(
        "- **Total agent responses**: {}\n",
        total_responses
    ));
    report.push_str(&format!(
        "- **Grounded response rate**: {}% ({} / {})\n",
        grounded_pct, grounded_responses, total_responses
    ));
    report.push_str(&format!(
        "- **Retrieval tool calls**: {}\n",
        retrieval_count
    ));
    report.push_str(&format!(
        "- **User corrections (proxy)**: {}\n\n",
        correction_count
    ));

    if usage.is_empty() {
        report.push_str("No metrics data found for this period.\n\n");
    } else {
        report.push_str("## By Profile / Provider / Model\n\n");
        report.push_str(
            "| Profile | Provider | Model | Messages | Prompt Tokens | Completion Tokens \
             | Total Time (ms) | Avg Time (ms) |\n",
        );
        report.push_str(
            "|---------|----------|-------|----------|---------------|-------------------\
             |-----------------|---------------|\n",
        );

        for row in &usage {
            report.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {:.0} |\n",
                row.profile,
                row.provider.as_deref().unwrap_or("-"),
                row.model.as_deref().unwrap_or("-"),
                row.message_count.unwrap_or(0),
                row.total_prompt_tokens.unwrap_or(0),
                row.total_completion_tokens.unwrap_or(0),
                row.total_processing_ms.unwrap_or(0),
                row.avg_processing_ms.unwrap_or(0.0),
            ));
        }

        // Totals
        let total_prompt: i64 = usage
            .iter()
            .map(|r| r.total_prompt_tokens.unwrap_or(0))
            .sum();
        let total_completion: i64 = usage
            .iter()
            .map(|r| r.total_completion_tokens.unwrap_or(0))
            .sum();
        let total_time: i64 = usage
            .iter()
            .map(|r| r.total_processing_ms.unwrap_or(0))
            .sum();

        report.push_str(&format!(
            "\n**Totals**: {} prompts | {} completion tokens | {} ms processing time\n\n",
            total_prompt, total_completion, total_time
        ));
    }

    // Hallucination metric explanation
    report.push_str("## Metrics Notes\n\n");
    report.push_str(
        "- **Grounded response rate**: Percentage of agent responses that include context \
         assembly metadata (evidence tracking)\n",
    );
    report.push_str(
        "- **Retrieval tool calls**: Number of times search_messages or search_wiki tools \
         were invoked\n",
    );
    report.push_str(
        "- **User corrections (proxy)**: Count of user messages containing correction \
         keywords (wrong, incorrect, etc.) within 5 minutes of an agent response: a proxy \
         for hallucination/quality issues\n",
    );

    Ok((report, false))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Plugin config - received via MCP configure message
    let config: Arc<Mutex<Config>> = Arc::new(Mutex::new(Config::default()));

    // Shared database pool - populated by configure callback before any tool call
    let pool: Arc<RwLock<Option<PgPool>>> = Arc::new(RwLock::new(None));

    // on_configure: called when omniagent sends the resolved plugin config
    let on_configure = {
        let config = config.clone();
        let pool = pool.clone();
        Some(move |params: Value| {
            let mut cfg = config.lock();
            if let Some(url) = params.get("database_url").and_then(|v| v.as_str()) {
                if !url.is_empty() {
                    cfg.database_url = url.to_string();

                    // Also initialize the database pool
                    let url_clone = url.to_string();
                    tokio::task::block_in_place(|| {
                        let rt = tokio::runtime::Handle::current();
                        let new_pool = rt
                            .block_on(omniagent::db::connect(&url_clone))
                            .expect("Failed to connect to database");
                        *pool.blocking_write() = Some(new_pool);
                    });
                }
            }
            if let Some(dir) = params.get("omni_dir").and_then(|v| v.as_str()) {
                if !dir.is_empty() {
                    cfg.omni_dir = dir.to_string();
                }
            }
            // Channels.yml data dir - needed by search_channels
            omniagent::channels_yaml::set_data_dir(&cfg.omni_dir);
            tracing::info!("Search plugin configured");
        })
    };

    let default_omni_dir = "/opt/omni".to_string();

    // ── search_messages (keyword) ─────────────────────────────────────────
    let p_search = pool.clone();
    let search_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_search.clone();
        Box::pin(async move {
            let guard = p.read().await;
            let pool = guard
                .as_ref()
                .ok_or_else(|| {
                    anyhow::anyhow!("Database pool not initialized. Configure plugin first.")
                })?
                .clone();
            handle_search_messages(&pool, &args).await
        })
    });

    // ── search_wiki ───────────────────────────────────────────────────────
    let c1 = config.clone();
    let d1 = default_omni_dir.clone();
    let wiki_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let c = c1.clone();
        let d = d1.clone();
        // Agent's profile from _meta (injected by the MCP client) - same
        // pattern as the skills plugin. Never requires a profile argument.
        let profile = meta
            .as_ref()
            .and_then(|m| m.profile_name.clone())
            .unwrap_or_default();
        Box::pin(async move {
            let cfg = c.lock();
            let omni_dir = if cfg.omni_dir.is_empty() {
                &d
            } else {
                &cfg.omni_dir
            };
            handle_search_wiki(&args, omni_dir, &profile)
        })
    });

    // Helper to fetch the shared pool; returns a soft error if not configured.
    fn pool_or_err(pool: &Arc<RwLock<Option<PgPool>>>) -> Result<PgPool, (String, bool)> {
        let guard = pool.try_read();
        match guard {
            Ok(g) => match g.as_ref() {
                Some(p) => Ok(p.clone()),
                None => Err((
                    "Search database pool not configured. The plugin may need a database_url in its config."
                        .to_string(),
                    true,
                )),
            },
            Err(_) => Err((
                "Search database pool lock poisoned or busy. Retry.".to_string(),
                true,
            )),
        }
    }

    // ── search_database: free-form read-only SELECT ───────────────────────
    let p_db = pool.clone();
    let db_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_db.clone();
        Box::pin(async move {
            let pool = match pool_or_err(&p) {
                Ok(pool) => pool,
                Err(e) => return Ok(e),
            };
            handle_search_database(&pool, &args).await
        })
    });

    // ── search_thread_messages: full thread retrieval ─────────────────────
    let p_tm = pool.clone();
    let thread_messages_handler: ToolHandler =
        Box::new(move |args: Value, meta: Option<McpMeta>| {
            let p = p_tm.clone();
            Box::pin(async move {
                let pool = match pool_or_err(&p) {
                    Ok(pool) => pool,
                    Err(e) => return Ok(e),
                };
                handle_search_thread_messages(&pool, &args, meta.as_ref()).await
            })
        });

    // ── search_channel_prompts: channel prompt history ────────────────────
    let p_cp = pool.clone();
    let channel_prompts_handler: ToolHandler =
        Box::new(move |args: Value, meta: Option<McpMeta>| {
            let p = p_cp.clone();
            Box::pin(async move {
                let pool = match pool_or_err(&p) {
                    Ok(pool) => pool,
                    Err(e) => return Ok(e),
                };
                handle_search_channel_prompts(&pool, &args, meta.as_ref()).await
            })
        });

    // ── search_channels: list channels ────────────────────────────────────
    let p_ch = pool.clone();
    let channels_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_ch.clone();
        Box::pin(async move {
            let pool = match pool_or_err(&p) {
                Ok(pool) => pool,
                Err(e) => return Ok(e),
            };
            handle_search_channels(&pool, &args).await
        })
    });

    // ── search_metrics: agent metrics ─────────────────────────────────────
    let p_m = pool.clone();
    let metrics_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_m.clone();
        Box::pin(async move {
            let pool = match pool_or_err(&p) {
                Ok(pool) => pool,
                Err(e) => return Ok(e),
            };
            handle_search_metrics(&pool, &args).await
        })
    });

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "search_messages".to_string(),
                description: "Search message history across all channels. Use this tool when the LLM needs to find information from past conversations. Use specific keywords and narrow the scope with channel_id when possible. Does NOT search wiki pages: use search_wiki for that.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query to find in messages"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max results (max 50)",
                            "default": 10
                        },
                        "channel_id": { "type": "string", "description": "Optional channel name filter" }
                    },
                    "required": ["query"]
                }),
            },
            handler: search_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "search_wiki".to_string(),
                description: "Search wiki pages for relevant documentation. Use this to find documentation, guides, and notes. Searches the ACTIVE PROFILE's wiki automatically (the profile is injected by the runtime, no profile argument needed). Does NOT search message history: use search_messages for that.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query to find in wiki content and filenames"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max results (max 30)",
                            "default": 10
                        }
                    },
                    "required": ["query"]
                }),
            },
            handler: wiki_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "search_database".to_string(),
                description: "Run any read-only SELECT SQL against the agent database. \
This is the FREE-QUERY tool: use it for custom aggregations (COUNT(*), GROUP BY, SUM, \
JOIN across tables) and structured lookups that the purpose-built search tools do not cover. \
The statement MUST start with SELECT or WITH; write/DDL keywords (INSERT/UPDATE/DELETE/DROP/\
ALTER/...) are rejected, and the query runs inside a read-only transaction, so writes are \
blocked at the database level.\n\n\
Available tables: messages, threads, summaries, kanban_tasks, \
profiles. Include the full table/column names in your SQL.\n\n\
Message-CONTENT lookups: ALWAYS use search_messages (tsvector over \
messages.search_tsv, returns in <3 s) - NEVER run ILIKE '%..%' over \
messages.content in search_database: that full-table scan has no usable \
index and costs ~30 s per call, so the 8 s statement timeout cancels it \
and returns a hint to switch tools.\n\nFor common lookups prefer the \
purpose-built tools: search_messages (keyword), \
search_thread-messages (thread contents), search_channel-prompts (channel prompt history), \
search_channels (channel ids)."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "sql": {
                            "type": "string",
                            "description": "Raw SELECT (or WITH) SQL statement to execute"
                        }
                    },
                    "required": ["sql"]
                }),
            },
            handler: db_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "search_thread_messages".to_string(),
                description: "Read all messages in a conversation thread (the prompt + its \
replies), ordered by sequence. Defaults to the CURRENT thread; pass thread_id to read a \
different one. Use to reconstruct a past conversation or inspect what a thread contained."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "thread_id": {
                            "type": "integer",
                            "description": "Thread ID (default: current thread)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max messages (max 200)",
                            "default": 100
                        }
                    }
                }),
            },
            handler: thread_messages_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "search_channel_prompts".to_string(),
                description: "List the first message (prompt / seq-0) of every thread in a \
channel, newest first. Use to review what has been asked or started in a channel. Defaults \
to the CURRENT channel; pass channel_id for a different one."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "channel_id": { "type": "string", "description": "Channel name (default: current channel)" },
                        "limit": {
                            "type": "integer",
                            "description": "Max results (max 50)",
                            "default": 10
                        }
                    }
                }),
            },
            handler: channel_prompts_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "search_channels".to_string(),
                description: "List all channels with their id, name, platform. \
Use to discover channel_id values needed by channel-scoped tools (search_channel-prompts, \
search_messages)."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "limit": {
                            "type": "integer",
                            "description": "Max channels (max 200)",
                            "default": 50
                        }
                    }
                }),
            },
            handler: channels_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "search_metrics".to_string(),
                description: "Report agent performance metrics: token usage, latency, message counts, \
                 groundedness rate, retrieval hit rate, and hallucination proxy metrics. \
                 All metrics are aggregated from the messages table and can be filtered \
                 by time window and profile."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "hours": {
                            "type": "integer",
                            "description": "Lookback window in hours (default: 24)"
                        },
                        "profile": {
                            "type": "string",
                            "description": "Filter by profile name (default: all profiles)"
                        }
                    }
                }),
            },
            handler: metrics_handler,
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-search".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };

    run_server_with_config(server_info, tools, on_configure).await
}

// ---------------------------------------------------------------------------
// Unit tests: search_messages SQL builder regression (DB-free)
// ---------------------------------------------------------------------------
// These tests pin the rearchitected query shape: the tool must search the
// dedicated tsvector column (messages.search_tsv, GIN index
// idx_messages_search_tsv) via a plainto_tsquery match - NEVER an ILIKE over
// the TOAST-heavy messages.content column. They fail on the pre-fix ILIKE
// implementation and protect the "index-driven by construction" property.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_quote_doubles_only_single_quotes() {
        assert_eq!(sql_quote("it's"), "it''s");
        assert_eq!(sql_quote("no quotes"), "no quotes");
        assert_eq!(sql_quote("a'b'c"), "a''b''c");
    }

    #[test]
    fn search_sql_uses_tsvector_not_content_ilike() {
        let sql = build_search_messages_sql("deploy telegram", None, 10);
        assert!(
            sql.contains("search_tsv @@ plainto_tsquery('english', 'deploy telegram')"),
            "SQL must match search_tsv with a plainto_tsquery literal: {sql}"
        );
        assert!(
            !sql.contains("content ILIKE"),
            "SQL must not ILIKE over messages.content: {sql}"
        );
        assert!(
            !sql.to_lowercase().contains(" idx_messages_content_trgm"),
            "must not reference the old content trigram path: {sql}"
        );
        assert!(
            sql.trim_end().ends_with("LIMIT 10"),
            "result must stay bounded by LIMIT: {sql}"
        );
        assert!(
            !sql.contains("threads"),
            "non-channel search must not join threads: {sql}"
        );
    }

    #[test]
    fn search_sql_channel_variant_joins_threads() {
        let sql = build_search_messages_sql("omnidev", Some("telegram"), 25);
        assert!(
            sql.contains("JOIN threads t ON t.id = ms.thread_id"),
            "channel search must join threads in the candidate stage: {sql}"
        );
        assert!(
            sql.contains("t.channel_id = 'telegram'"),
            "channel filter must be present: {sql}"
        );
        assert!(
            sql.contains("search_tsv @@ plainto_tsquery('english', 'omnidev')"),
            "tsvector match must be present: {sql}"
        );
        assert!(
            sql.trim_end().ends_with("LIMIT 25"),
            "outer fetch must stay bounded by LIMIT: {sql}"
        );
        assert!(
            sql.contains("ms.content") || sql.contains("m.content"),
            "content must be selected only for the bounded outer fetch: {sql}"
        );
    }

    #[test]
    fn search_sql_escapes_query_literal() {
        let sql = build_search_messages_sql("o'brien", None, 10);
        assert!(
            sql.contains("plainto_tsquery('english', 'o''brien')"),
            "single quotes in query must be escaped: {sql}"
        );
    }

    #[test]
    fn search_sql_escapes_channel_literal() {
        let sql = build_search_messages_sql("x", Some("it's"), 10);
        assert!(
            sql.contains("t.channel_id = 'it''s'"),
            "single quotes in channel must be escaped: {sql}"
        );
    }

    #[test]
    fn search_sql_caps_limit_at_call_site_default() {
        // handle_search_messages caps limit to 50; the builder receives the
        // already-capped value. Assert the builder honors whatever it gets.
        let sql = build_search_messages_sql("deploy", None, 50);
        assert!(sql.trim_end().ends_with("LIMIT 50"), "{sql}");
    }

    #[test]
    fn search_sql_orders_by_relevance_rank_desc() {
        let sql = build_search_messages_sql("deploy telegram", None, 10);
        assert!(
            sql.contains(
                "ts_rank_cd(im.search_tsv, plainto_tsquery('english', 'deploy telegram')) DESC"
            ),
            "results must be ordered by descending relevance: {sql}"
        );
    }

    #[test]
    fn search_sql_underscore_identifier_ors_exact_token_and_sorts_first() {
        let sql = build_search_messages_sql("parent_by_chat", None, 10);
        assert!(
            sql.contains("plainto_tsquery('english', 'parent_by_chat')"),
            "word-level recall term must stay: {sql}"
        );
        assert!(
            sql.contains("to_tsquery('english', 'parentbychat')"),
            "exact joined-token term must be added for snake_case queries: {sql}"
        );
        assert!(
            sql.contains("(im.search_tsv @@ to_tsquery('english', 'parentbychat')) DESC"),
            "exact identifier matches must sort first: {sql}"
        );
        assert!(
            sql.contains("plainto_tsquery('english', 'parent_by_chat') || (to_tsquery('english', 'parentbychat'))"),
            "match must OR the word query with the exact token: {sql}"
        );
    }

    #[test]
    fn search_sql_plain_query_has_no_exact_token_terms() {
        let sql = build_search_messages_sql("deploy telegram", None, 10);
        assert!(
            !sql.contains("(to_tsquery('english', '"),
            "plain queries must not gain identifier terms: {sql}"
        );
    }

    #[test]
    fn identifier_whole_tokens_extracts_underscore_runs() {
        assert_eq!(
            identifier_whole_tokens("parent_by_chat"),
            vec!["parentbychat"]
        );
        assert_eq!(
            identifier_whole_tokens("task_omnidev_threads_stuck_in_processing"),
            vec!["taskomnidevthreadsstuckinprocessing"]
        );
        assert_eq!(
            identifier_whole_tokens("fix parent_by_chat and search_messages"),
            vec!["parentbychat", "searchmessages"]
        );
        assert!(identifier_whole_tokens("plain words only").is_empty());
        assert!(
            identifier_whole_tokens("deepseek-v4-flash").is_empty(),
            "dash-only tokens are handled by the english host-token path"
        );
        assert!(identifier_whole_tokens("_leading and trailing_").is_empty());
    }

    #[test]
    fn search_sql_tiebreak_columns_are_alias_qualified() {
        let sql = build_search_messages_sql("omnidev", Some("telegram"), 10);
        assert!(
            sql.contains("ms.created_at DESC") && sql.contains("m.created_at DESC"),
            "channel-variant ORDER BY must qualify created_at/id (threads join would be ambiguous): {sql}"
        );
        let g = build_search_messages_sql("deploy telegram", None, 10);
        assert!(
            g.contains("im.created_at DESC") && g.contains("m.created_at DESC"),
            "global ORDER BY must qualify created_at/id: {g}"
        );
        assert!(
            !g.contains("ts_rank_cd(search_tsv, "),
            "global rank must use the im alias: {g}"
        );
    }
}

#[cfg(test)]
mod search_db_latency_guard_tests {
    use super::*;

    #[test]
    fn timeout_error_text_is_detected() {
        assert!(is_statement_timeout_error(
            "error returned from database: canceling statement due to statement timeout"
        ));
        assert!(is_statement_timeout_error(
            "db error: ERROR: canceling statement due to statement timeout\nSQLSTATE 57014"
        ));
        assert!(!is_statement_timeout_error(
            "db error: ERROR: relation \"messages\" does not exist"
        ));
        assert!(!is_statement_timeout_error(""));
    }

    #[test]
    fn timeout_hint_directs_content_lookups_to_search_messages() {
        let hint = search_db_timeout_hint();
        assert!(hint.contains("search_messages"), "hint: {hint}");
        assert!(hint.contains("messages.content"), "hint: {hint}");
        assert!(hint.contains("ILIKE"), "hint: {hint}");
        assert!(hint.contains("tsvector"), "hint: {hint}");
    }

    #[test]
    fn timeout_guard_sits_in_5_to_10_second_band() {
        assert!(
            (5000..=10000).contains(&SEARCH_DB_STATEMENT_TIMEOUT_MS),
            "statement timeout must be 5-10 s, got {SEARCH_DB_STATEMENT_TIMEOUT_MS}"
        );
    }

    #[test]
    fn slow_query_log_threshold_is_about_2_seconds() {
        assert!(
            (1500..=3000).contains(&SEARCH_DB_SLOW_QUERY_LOG_MS),
            "slow threshold ~2 s, got {SEARCH_DB_SLOW_QUERY_LOG_MS}"
        );
    }

    #[test]
    fn one_line_sql_collapses_and_bounds() {
        let sql = "SELECT count(*)\nFROM messages m\nWHERE m.content ILIKE '%foo%'";
        assert_eq!(
            one_line_sql(sql),
            "SELECT count(*) FROM messages m WHERE m.content ILIKE '%foo%'"
        );
        let long = format!("SELECT '{}'", "x".repeat(500));
        let l2 = one_line_sql(&long);
        assert!(l2.chars().count() <= 204, "bounded: {}", l2.chars().count());
        assert!(l2.ends_with("..."));
    }

    /// End-to-end guard proof against a real PostgreSQL (omnidev dev DB):
    /// a statement that would take 30 s is canceled at the 8 s statement
    /// timeout and the tool answers with the search_messages hint. Skips
    /// cleanly when no DATABASE_URL is set (plain CI without a DB).
    #[tokio::test]
    async fn timeout_guard_cancels_long_statement_with_hint() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipped: no DATABASE_URL (no live DB in this environment)");
            return;
        };
        let pool = omniagent::db::connect(&url)
            .await
            .expect("connect to dev database for guard test");
        let args = serde_json::json!({ "sql": "SELECT pg_sleep(30)" });
        let started = std::time::Instant::now();
        let res = handle_search_database(&pool, &args).await;
        let elapsed_ms = started.elapsed().as_millis();
        let err = res.expect_err("pg_sleep(30) must be canceled by the 8 s guard");
        let text = err.to_string();
        assert!(
            text.contains("statement-timeout guard"),
            "error must mention the guard: {text}"
        );
        assert!(
            text.contains("search_messages"),
            "error must steer content lookups to search_messages: {text}"
        );
        assert!(
            elapsed_ms < 15_000,
            "guard must cut the 30 s statement short, took {elapsed_ms} ms"
        );
        eprintln!("guard live check: canceled after {elapsed_ms} ms with hint; ok");
        pool.close().await;
    }
}
