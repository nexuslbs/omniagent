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
// Query classification
// ---------------------------------------------------------------------------

/// Determine if a user message is likely a query/request that would benefit
/// from retrieval. Returns (label, needs_retrieval).
pub fn classify_query(_content: &str) -> (&'static str, bool) {
    // Simple heuristic: queries longer than 15 words likely need retrieval
    let word_count = _content.split_whitespace().count();
    if word_count > 15 {
        ("complex_query", true)
    } else {
        ("simple_message", false)
    }
}

// ---------------------------------------------------------------------------
// Full context assembly for a thread
// ---------------------------------------------------------------------------

/// Assemble the [3] Context section — all context blocks that would be injected
/// into the prompt for a given thread. This is the same logic used by the agent
/// when processing a message, extracted for reuse by the API preview endpoint.
///
/// Parameters mirror what the agent has at its disposal during processing.
/// No messages are written; this is purely a read-only preview.
#[allow(clippy::too_many_arguments)]
pub async fn build_thread_context(
    pool: &PgPool,
    thread_id: i64,
    channel_id: i64,
    cause_msg_id: i64,
    cause_content: &str,
    profile_name: &str,
    data_dir: &str,
    qdrant_url: Option<&str>,
    prompt_budget: usize,
    auto_retrieval_enabled: bool,
    retrieval_aggressiveness: u8,
) -> (String, ContextAssemblyMeta) {
    use crate::db::types as queries;
    use crate::vectorizer::{vector_to_string, HashVectorizer, Vectorizer};

    let mut builder = ContextBuilder::new().with_budget(prompt_budget);

    // Classify the user message to determine retrieval needs
    let (_query_class, needs_retrieval) = classify_query(cause_content);

    // Determine retrieval aggressiveness
    let use_retrieval = needs_retrieval && auto_retrieval_enabled;
    let aggressiveness = if use_retrieval {
        retrieval_aggressiveness
    } else {
        0u8
    };

    // Add recent thread messages as a high-priority context block
    match queries::get_recent_thread_messages(pool, thread_id, 10).await {
        Ok(recent_msgs) => {
            if !recent_msgs.is_empty() {
                let thread_content: String = recent_msgs
                    .iter()
                    .rev() // oldest first
                    .filter(|m| m.id != cause_msg_id) // exclude the current cause message
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
    match queries::get_latest_summary(pool, channel_id).await {
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
            match queries::get_completed_seq0_threads_since(pool, channel_id, summary.next_thread_id, 5).await {
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
    let skills_dir = format!("{}/profiles/{}/skills", data_dir, profile_name);
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

    // Add retrieved past messages + wiki if retrieval is indicated
    if aggressiveness > 0 {
        let search_terms: Vec<&str> = cause_content
            .split_whitespace()
            .filter(|w| w.len() > 4)
            .take(5)
            .collect();

        if !search_terms.is_empty() {
            let search_query = search_terms.join(" ");

            // ILIKE text search in messages
            match queries::search_messages_text(pool, &search_query, channel_id, 5).await {
                Ok(matched_msgs) => {
                    if !matched_msgs.is_empty() {
                        let retrieved: String = matched_msgs
                            .iter()
                            .map(|m| {
                                format!(
                                    "[{} msg_id={}]: {}",
                                    m.role,
                                    m.id,
                                    m.content.chars().take(300).collect::<String>()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n---\n");
                        builder.add_block(ContextBlock::new(
                            "retrieved_past_messages",
                            BlockPriority::Low,
                            &format!("Retrieved from past conversations:\n{}", retrieved),
                            3_000,
                        ));
                    }
                }
                Err(e) => tracing::warn!("Failed to search past messages: {:?}", e),
            }

            // Wiki text search
            let wiki_dir = format!("{}/profiles/{}/wiki", data_dir, profile_name);
            let wiki_results = queries::search_wiki_text(&wiki_dir, &search_query, 3);
            if !wiki_results.is_empty() {
                let wiki_text: String = wiki_results
                    .iter()
                    .map(|(path, title, snippet)| format!("[{}] {}:\n{}", title, path, snippet))
                    .collect::<Vec<_>>()
                    .join("\n---\n");
                builder.add_block(ContextBlock::new(
                    "retrieved_wiki_text",
                    BlockPriority::Low,
                    &format!("Wiki references:\n{}", wiki_text),
                    2_000,
                ));
            }

            // Aggressiveness >= 2: add semantic search too
            if aggressiveness >= 2 {
                let hash_vec = HashVectorizer;
                let query_embedding = hash_vec.generate_embedding(&search_query).await;
                let emb_str = vector_to_string(&query_embedding);

                // Pgvector semantic search over messages
                match queries::search_messages_semantic(pool, &emb_str, channel_id, 3).await {
                    Ok(semantic_msgs) => {
                        if !semantic_msgs.is_empty() {
                            let semantic: String = semantic_msgs
                                .iter()
                                .map(|m| {
                                    format!(
                                        "[{} msg_id={}]: {}",
                                        m.role,
                                        m.id,
                                        m.content.chars().take(300).collect::<String>()
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n---\n");
                            builder.add_block(ContextBlock::new(
                                "semantically_similar_messages",
                                BlockPriority::Low,
                                &format!("Semantically similar messages:\n{}", semantic),
                                2_000,
                            ));
                        }
                    }
                    Err(e) => tracing::warn!("Failed semantic search: {:?}", e),
                }

                // Qdrant wiki search
                if let Some(qdrant) = qdrant_url {
                    let wiki_embedding = hash_vec.generate_embedding(&search_query).await;
                    match queries::search_wiki_qdrant(qdrant, &wiki_embedding, 3).await {
                        Ok(qdrant_results) => {
                            if !qdrant_results.is_empty() {
                                let qdrant_text: String = qdrant_results
                                    .iter()
                                    .map(|(path, title, score)| {
                                        format!("[{} (score={:.2})] {}", title, score, path)
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                builder.add_block(ContextBlock::new(
                                    "semantically_similar_wiki",
                                    BlockPriority::Low,
                                    &format!("Wiki docs (semantic similarity):\n{}", qdrant_text),
                                    1_500,
                                ));
                            }
                        }
                        Err(e) => tracing::warn!("Qdrant wiki search failed: {:?}", e),
                    }
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
