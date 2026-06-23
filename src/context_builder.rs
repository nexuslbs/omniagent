//! ContextBuilder — selective, ranked prompt assembly with token budgeting.
//!
//! Builds the LLM prompt context from ordered blocks, respecting per-block
//! character budgets. Priority order (highest → lowest):
//!
//! 1. **Never-trim** — System/profile instructions, MEMORY.md
//! 2. **High** — Pinned user messages, active tool definitions
//! 3. **Normal** — Recent thread messages (recency window)
//! 4. **Low** — Retrieved past messages, retrieved wiki snippets
//!
//! When total exceeds the context budget, lower-priority blocks are trimmed
//! first (truncated to their per-block budget, then if still over budget,
//! the entire block is dropped).

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Block priority
// ---------------------------------------------------------------------------

/// Priority level for a context block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BlockPriority {
    /// Never trimmed. Only system/profile instructions and MEMORY.md.
    NeverTrim,
    /// High priority — pinned messages, tool definitions.
    High,
    /// Normal priority — recent thread messages.
    Normal,
    /// Low priority — retrieved messages, wiki snippets.
    Low,
}

// ---------------------------------------------------------------------------
// Block definition
// ---------------------------------------------------------------------------

/// A single context block with its priority, content, and budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextBlock {
    /// Human-readable label for diagnostics.
    pub label: String,
    /// Priority level (determines trim order).
    pub priority: BlockPriority,
    /// Content of this block.
    pub content: String,
    /// Maximum characters for this block (0 = unlimited for never-trim).
    pub max_chars: usize,
}

impl ContextBlock {
    /// Create a new context block with the given label, priority, and max chars.
    pub fn new(label: &str, priority: BlockPriority, content: &str, max_chars: usize) -> Self {
        Self {
            label: label.to_string(),
            priority,
            content: content.to_string(),
            max_chars,
        }
    }

    /// Create a never-trim block (content is always included in full).
    pub fn never_trim(label: &str, content: &str) -> Self {
        Self {
            label: label.to_string(),
            priority: BlockPriority::NeverTrim,
            content: content.to_string(),
            max_chars: 0, // unlimited
        }
    }

    /// Get the rendered content, respecting the character budget.
    pub fn render(&self) -> String {
        if self.max_chars == 0 || self.content.len() <= self.max_chars {
            return self.content.clone();
        }
        // Truncate with a notification
        let truncate_at = self
            .content
            .char_indices()
            .nth(self.max_chars)
            .map(|(i, _)| i)
            .unwrap_or(self.content.len());
        format!(
            "{}\n\n[... {:?} truncated from {} to {} chars]",
            &self.content[..truncate_at],
            self.label,
            self.content.len(),
            self.max_chars
        )
    }

    /// Get the rendered length (after trimming).
    pub fn rendered_len(&self) -> usize {
        if self.max_chars == 0 {
            return self.content.len();
        }
        self.content.len().min(self.max_chars)
    }
}

// ---------------------------------------------------------------------------
// Context assembly metadata
// ---------------------------------------------------------------------------

/// Diagnostics about how the context was assembled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextAssemblyMeta {
    /// Which message IDs were selected for the prompt.
    pub selected_message_ids: Vec<i64>,
    /// Which wiki files were referenced.
    pub wiki_files: Vec<String>,
    /// Token/char counts per block label.
    pub block_counts: HashMap<String, usize>,
    /// Whether any blocks were dropped entirely.
    pub dropped_blocks: Vec<String>,
    /// Total assembled character count.
    pub total_chars: usize,
}

// ---------------------------------------------------------------------------
// Builder struct
// ---------------------------------------------------------------------------

/// The ContextBuilder assembles an LLM prompt from ordered blocks with
/// priority-based trimming.
#[derive(Debug, Clone)]
pub struct ContextBuilder {
    /// Ordered list of context blocks.
    blocks: Vec<ContextBlock>,
    /// Total character budget for all non-never-trim blocks combined.
    /// If 0, no budget limit is applied.
    budget: usize,
    /// Reserved characters for output tokens (subtracted from budget).
    output_reserve: usize,
}

impl Default for ContextBuilder {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            budget: 8_000, // default context budget ~8K chars
            output_reserve: 2_000, // reserve ~2K chars for output
        }
    }
}

impl ContextBuilder {
    /// Create a new ContextBuilder with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the total character budget (across all non-never-trim blocks).
    pub fn with_budget(mut self, budget: usize) -> Self {
        self.budget = budget;
        self
    }

    /// Set output token reserve.
    pub fn with_output_reserve(mut self, reserve: usize) -> Self {
        self.output_reserve = reserve;
        self
    }

    /// Add a context block to the assembly.
    pub fn add_block(&mut self, block: ContextBlock) {
        self.blocks.push(block);
    }

    /// Add multiple blocks.
    pub fn add_blocks(&mut self, blocks: Vec<ContextBlock>) {
        self.blocks.extend(blocks);
    }

    /// Get the effective budget after reserving output tokens.
    fn effective_budget(&self) -> usize {
        if self.budget == 0 {
            return usize::MAX;
        }
        self.budget.saturating_sub(self.output_reserve)
    }

    /// Assemble the final prompt string and return metadata.
    pub fn assemble(&self) -> (String, ContextAssemblyMeta) {
        let selected_message_ids: Vec<i64> = Vec::new();
        let wiki_files: Vec<String> = Vec::new();
        let mut block_counts: HashMap<String, usize> = HashMap::new();
        let mut dropped_blocks: Vec<String> = Vec::new();

        let mut result_parts: Vec<String> = Vec::new();

        // 1. Collect never-trim blocks in insertion order
        let mut never_trim_parts: Vec<String> = Vec::new();
        for block in &self.blocks {
            if block.priority == BlockPriority::NeverTrim {
                let rendered = block.render();
                never_trim_parts.push(rendered.clone());
                block_counts.insert(block.label.clone(), rendered.len());
            }
        }

        // 2. Collect remaining blocks sorted by priority (High → Normal → Low)
        let mut remaining: Vec<&ContextBlock> = self
            .blocks
            .iter()
            .filter(|b| b.priority != BlockPriority::NeverTrim)
            .collect();
        remaining.sort_by_key(|b| match b.priority {
            BlockPriority::High => 0,
            BlockPriority::Normal => 1,
            BlockPriority::Low => 2,
            _ => 3,
        });

        // 3. Fill within budget
        let budget = self.effective_budget();
        let never_trim_len: usize = never_trim_parts.iter().map(|p| p.len()).sum();
        let mut used = never_trim_len;

        for block in &remaining {
            let rendered = block.render();
            if used + rendered.len() <= budget || budget == usize::MAX {
                result_parts.push(rendered.clone());
                block_counts.insert(block.label.clone(), rendered.len());
                used += rendered.len();
            } else if !result_parts.is_empty() {
                // Can't fit — try truncating the block to remaining budget
                let remaining_budget = budget.saturating_sub(used);
                if remaining_budget > 50 {
                    let truncated = ContextBlock::new(
                        &block.label,
                        block.priority,
                        &block.content,
                        remaining_budget.saturating_sub(50),
                    )
                    .render();
                    if truncated.len() > 10 {
                        result_parts.push(truncated.clone());
                        block_counts.insert(block.label.clone(), truncated.len());
                        used += truncated.len();
                        continue;
                    }
                }
                // Still can't fit — drop this and all lower priority
                dropped_blocks.push(block.label.clone());
            } else {
                dropped_blocks.push(block.label.clone());
            }
        }

        // 4. Join everything
        let mut all_parts = never_trim_parts;
        all_parts.extend(result_parts);
        let result = all_parts.join("\n\n");

        let meta = ContextAssemblyMeta {
            selected_message_ids,
            wiki_files,
            block_counts,
            dropped_blocks,
            total_chars: result.len(),
        };

        (result, meta)
    }
}


// ---------------------------------------------------------------------------
// Full context assembly for a thread
// ---------------------------------------------------------------------------

/// Identifiers for the thread whose context is being assembled.
#[derive(Debug, Clone)]
pub struct ThreadContextIdentifiers {
    pub thread_id: i64,
    pub channel_id: i64,
    pub cause_msg_id: i64,
}

/// Configuration parameters for context assembly.
///
/// Groups the content, profile, runtime paths, retrieval flags, and budget
/// that were previously individual parameters on `build_thread_context`.
#[derive(Debug, Clone)]
pub struct ThreadContextConfig<'a> {
    /// The user message that caused this thread to be processed.
    pub cause_content: &'a str,
    /// Name of the active profile (used to locate skills / wiki dirs).
    pub profile_name: &'a str,
    /// Base data directory (e.g. `ctx.data_dir` in the agent).
    pub data_dir: &'a str,
    /// Optional Qdrant URL for semantic wiki search.
    pub qdrant_url: Option<&'a str>,
    /// Character budget for the assembled context (prompt part only).
    pub prompt_budget: usize,
    /// Whether automatic retrieval (text/semantic search) is enabled.
    pub auto_retrieval_enabled: bool,
    /// How aggressively to perform retrieval (0 = disabled, 1 = text only, 2+ = text + semantic).
    pub retrieval_aggressiveness: u8,
}

/// Assemble the [3] Context section — all context blocks that would be injected
/// into the prompt for a given thread. This is the same logic used by the agent
/// when processing a message, extracted for reuse by the API preview endpoint.
///
/// Parameters mirror what the agent has at its disposal during processing.
/// No messages are written; this is purely a read-only preview.
pub async fn build_thread_context(
    pool: &PgPool,
    ids: &ThreadContextIdentifiers,
    config: &ThreadContextConfig<'_>,
) -> (String, ContextAssemblyMeta) {
    use crate::db::types as queries;
    use crate::vectorizer::{vector_to_string, HashVectorizer, Vectorizer};

    let mut builder = ContextBuilder::new().with_budget(config.prompt_budget);

    // Classify the user message to determine retrieval needs
    let word_count = config.cause_content.split_whitespace().count();
    let needs_retrieval = word_count > 15;

    // Determine retrieval aggressiveness
    let use_retrieval = needs_retrieval && config.auto_retrieval_enabled;
    let aggressiveness = if use_retrieval {
        config.retrieval_aggressiveness
    } else {
        0u8
    };

    // Add recent thread messages as a high-priority context block
    match queries::get_recent_thread_messages(pool, ids.thread_id, 10).await {
        Ok(recent_msgs) => {
            if !recent_msgs.is_empty() {
                let thread_content: String = recent_msgs
                    .iter()
                    .rev() // oldest first
                    .filter(|m| m.id != ids.cause_msg_id) // exclude the current cause message
                    .map(|m| format!("[{}]: {}", m.role, m.content))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !thread_content.is_empty() {
                    builder.add_block(ContextBlock::new(
                        "recent_thread_messages",
                        BlockPriority::High,
                        &format!("Recent conversation history (current thread):\n{}", thread_content),
                        2_500,
                    ));
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to retrieve thread context: {:?}", e);
        }
    }

    // Add last summary for this channel as high-priority context
    match queries::get_latest_summary(pool, ids.channel_id).await {
        Ok(Some(summary)) => {
            builder.add_block(ContextBlock::new(
                "last_summary",
                BlockPriority::High,
                &format!(
                    "Previous channel summary (covers threads up to id={}):\n{}",
                    summary.next_thread_id, summary.content
                ),
                4_000,
            ));

            // Also include threads completed after the last summary, if any
            match queries::get_completed_seq0_threads_since(pool, ids.channel_id, summary.next_thread_id, 5).await {
                Ok(roots) if !roots.is_empty() => {
                    let roots_content: String = roots
                        .iter()
                        .map(|t| format!("[Thread #{} by {}]: cause message available", t.id, t.cause))
                        .collect::<Vec<_>>()
                        .join("\n---\n");
                    builder.add_block(ContextBlock::new(
                        "recent_thread_roots_since_summary",
                        BlockPriority::Normal,
                        &format!("Recent threads (after last summary):\n{}", roots_content),
                        2_000,
                    ));
                }
                _ => {}
            }
        }
        _ => {
            // No summary yet for this channel — OK, just skip
        }
    }

    // Add profile skills as context
    let skills_dir = format!("{}/profiles/{}/skills", config.data_dir, config.profile_name);
    match tokio::task::spawn_blocking(move || -> Vec<String> {
        let mut skills = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&skills_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
                        let first_line = content.lines().next().unwrap_or("").trim();
                        let desc = if first_line.starts_with('#') {
                            first_line.trim_start_matches('#').trim()
                        } else {
                            first_line
                        };
                        skills.push(format!("- {}: {}", name, desc));
                    }
                }
            }
        }
        skills
    })
    .await
    {
        Ok(skills) if !skills.is_empty() => {
            builder.add_block(ContextBlock::new(
                "profile_skills",
                BlockPriority::Normal,
                &format!("Available skills:\n{}", skills.join("\n")),
                3_000,
            ));
        }
        _ => {}
    }

    // ── RRF (Reciprocal Rank Fusion) helper ──
    // RRF combines multiple ranked result sets into a single fused ranking.
    // score(r) = Σ 1/(k + rank_i(r)) for each result set i
    const RRF_K: f64 = 60.0;
    const RRF_TEXT_WEIGHT: f64 = 1.0;
    const RRF_SEMANTIC_WEIGHT: f64 = 1.0;

    // Add retrieved past messages + wiki if retrieval is indicated
    if aggressiveness > 0 {
        let search_terms: Vec<&str> = config.cause_content
            .split_whitespace()
            .filter(|w| w.len() > 4)
            .take(5)
            .collect();

        if !search_terms.is_empty() {
            let search_query = search_terms.join(" ");

            // ── Hybrid message search (text + semantic fused via RRF) ──
            // Collect results from both sources, then fuse
            let text_msgs = queries::search_messages_text(pool, &search_query, ids.channel_id, 10).await
                .unwrap_or_default();
            let semantic_msgs = if aggressiveness >= 2 {
                let hash_vec = HashVectorizer;
                let query_embedding = hash_vec.generate_embedding(&search_query).await;
                let emb_str = vector_to_string(&query_embedding);
                queries::search_messages_semantic(pool, &emb_str, ids.channel_id, 10).await
                    .unwrap_or_default()
            } else {
                vec![]
            };

            // RRF fusion of text + semantic results
            let fused_msgs = if semantic_msgs.is_empty() {
                text_msgs
            } else {
                use std::collections::HashMap;
                // Build RRF scores: message_id → fused_score
                let mut scores: HashMap<i64, f64> = HashMap::new();
                for (rank, msg) in text_msgs.iter().enumerate() {
                    let score = RRF_TEXT_WEIGHT / (RRF_K + rank as f64 + 1.0);
                    scores.insert(msg.id, scores.get(&msg.id).copied().unwrap_or(0.0) + score);
                }
                for (rank, msg) in semantic_msgs.iter().enumerate() {
                    let score = RRF_SEMANTIC_WEIGHT / (RRF_K + rank as f64 + 1.0);
                    scores.insert(msg.id, scores.get(&msg.id).copied().unwrap_or(0.0) + score);
                }
                // Sort by score descending, then deduplicate by ID
                let mut scored_ids: Vec<(i64, f64)> = scores.into_iter().collect();
                scored_ids.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                let top_ids: std::collections::HashSet<i64> = scored_ids.into_iter()
                    .take(5)
                    .map(|(id, _)| id)
                    .collect();
                // Collect messages in RRF order, preferring text result when available
                let mut seen = std::collections::HashSet::new();
                let mut fused: Vec<crate::models::Message> = Vec::new();
                for msg in text_msgs.iter().chain(semantic_msgs.iter()) {
                    if top_ids.contains(&msg.id) && seen.insert(msg.id) {
                        fused.push(msg.clone());
                    }
                }
                fused
            };

            if !fused_msgs.is_empty() {
                let retrieved: String = fused_msgs
                    .iter()
                    .map(|m| {
                        let content = if m.msg_type == "tool" || m.msg_type == "tool_result" || m.msg_type == "multi-tool" {
                            let tool_name = m.msg_subtype.as_deref().unwrap_or("unknown");
                            let preview = m.content.chars().take(100).collect::<String>();
                            format!("[Tool: {}] {}", tool_name, preview)
                        } else {
                            m.content.chars().take(500).collect::<String>()
                        };
                        format!(
                            "[{} msg_id={}]: {}",
                            m.role,
                            m.id,
                            content,
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n---\n");
                builder.add_block(ContextBlock::new(
                    "retrieved_past_messages",
                    BlockPriority::Low,
                    &format!("Retrieved from past conversations (hybrid search):\n{}", retrieved),
                    4_000,  // increased budget for hybrid results
                ));
            }

            // ── Hybrid wiki search (text + Qdrant semantic fused via RRF) ──
            // Collect results from both wiki text search and Qdrant, then fuse
            let wiki_dir = format!("{}/profiles/{}/wiki", config.data_dir, config.profile_name);
            let wiki_text_results = queries::search_wiki_text(&wiki_dir, &search_query, 5);

            let qdrant_results = if aggressiveness >= 2 {
                if let Some(qdrant) = config.qdrant_url {
                    let hash_vec = HashVectorizer;
                    let wiki_embedding = hash_vec.generate_embedding(&search_query).await;
                    queries::search_wiki_qdrant(qdrant, &wiki_embedding, 5).await
                        .unwrap_or_default()
                } else {
                    vec![]
                }
            } else {
                vec![]
            };

            // Fuse wiki results via RRF (match by path)
            let fused_wiki: Vec<(String, String, String)> = if qdrant_results.is_empty() {
                wiki_text_results
            } else {
                let mut wiki_scores: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
                // Track which items we include (top N by fused score)
                for (rank, (path, _title, _snippet)) in wiki_text_results.iter().enumerate() {
                    let score = RRF_TEXT_WEIGHT / (RRF_K + rank as f64 + 1.0);
                    *wiki_scores.entry(path.clone()).or_insert(0.0) += score;
                }
                for (rank, (path, _title, _score)) in qdrant_results.iter().enumerate() {
                    let score = RRF_SEMANTIC_WEIGHT / (RRF_K + rank as f64 + 1.0);
                    *wiki_scores.entry(path.clone()).or_insert(0.0) += score;
                }
                // Sort by score descending
                let mut scored_wiki: Vec<(String, f64)> = wiki_scores.into_iter().collect();
                scored_wiki.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                let top_paths: std::collections::HashSet<String> = scored_wiki.into_iter()
                    .take(4)
                    .map(|(path, _)| path)
                    .collect();
                // Collect results in RRF order
                let mut seen = std::collections::HashSet::new();
                let mut fused: Vec<(String, String, String)> = Vec::new();
                // Build an iterator over both result sets as (path, title, snippet)
                let text_iter = wiki_text_results.iter()
                    .map(|(p, t, s)| (p.as_str(), t.as_str(), s.as_str()));
                let qdrant_iter = qdrant_results.iter()
                    .map(|(p, t, _)| (p.as_str(), t.as_str(), "semantic match"));
                for (path, title, snippet) in text_iter.chain(qdrant_iter) {
                    if top_paths.contains(path) && seen.insert(path.to_string()) {
                        fused.push((path.to_string(), title.to_string(), snippet.to_string()));
                    }
                }
                fused
            };

            if !fused_wiki.is_empty() {
                let wiki_text: String = fused_wiki
                    .iter()
                    .map(|(path, title, snippet)| format!("[{}] {}:\n{}", title, path, snippet))
                    .collect::<Vec<_>>()
                    .join("\n---\n");
                builder.add_block(ContextBlock::new(
                    "retrieved_wiki_text",
                    BlockPriority::Low,
                    &format!("Wiki references (hybrid search):\n{}", wiki_text),
                    4_000,  // increased from 2_000
                ));
            }
        }
    }

    // ── Hindsight memory recall ──
    // If hindsight is configured (via HINDSIGHT_URL env var), recall relevant past memories.
    if aggressiveness > 0 {
        let hindsight_url = std::env::var("HINDSIGHT_URL").ok();
        if let Some(ref url) = hindsight_url {
            let hindsight_bank = std::env::var("HINDSIGHT_BANK").unwrap_or_else(|_| "omniagent".to_string());
            let recall_url = format!(
                "{}/v1/default/banks/{}/memories/recall",
                url.trim_end_matches('/'),
                hindsight_bank
            );

            // Build the recall payload from the cause content
            let recall_payload = serde_json::json!({
                "query": config.cause_content,
                "limit": 5,
            });

            match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
            {
                Ok(client) => {
                    match client
                        .post(&recall_url)
                        .json(&recall_payload)
                        .send()
                        .await
                    {
                        Ok(resp) if resp.status().is_success() => {
                            match resp.json::<serde_json::Value>().await {
                                Ok(data) => {
                                    let memories = data.get("results")
                                        .or_else(|| data.get("memories"))
                                        .and_then(|v| v.as_array())
                                        .cloned()
                                        .unwrap_or_default();

                                    if !memories.is_empty() {
                                        let memory_text: String = memories
                                            .iter()
                                            .take(5)
                                            .filter_map(|m| {
                                                let text = m.get("text")
                                                    .or_else(|| m.get("content"))
                                                    .and_then(|v| v.as_str())?;
                                                let tags = m.get("tags")
                                                    .and_then(|v| v.as_array())
                                                    .map(|a| {
                                                        a.iter()
                                                            .filter_map(|t| t.as_str())
                                                            .collect::<Vec<_>>()
                                                            .join(", ")
                                                    })
                                                    .unwrap_or_default();
                                                let score = m.get("score")
                                                    .and_then(|v| v.as_f64())
                                                    .map(|s| format!(" ({:.2})", s))
                                                    .unwrap_or_default();
                                                Some(format!(
                                                    "[tags: {}]{} {:.200}",
                                                    tags,
                                                    score,
                                                    text
                                                ))
                                            })
                                            .collect::<Vec<_>>()
                                            .join("\n---\n");

                                        builder.add_block(ContextBlock::new(
                                            "hindsight_memories",
                                            BlockPriority::Low,
                                            &format!(
                                                "Relevant past memories (from omniagent-hindsight):\n{}",
                                                memory_text
                                            ),
                                            3_000,
                                        ));
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to parse hindsight recall response: {:?}", e);
                                }
                            }
                        }
                        Ok(resp) => {
                            tracing::warn!(
                                "Hindsight recall returned HTTP {} (may not be running)",
                                resp.status()
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Hindsight recall request failed: {:?} (may not be running)",
                                e
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to build hindsight HTTP client: {:?}", e);
                }
            }
        }
    }

    builder.assemble()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_channel_name() {
        // Not used currently; placeholder for CI
    }
}
