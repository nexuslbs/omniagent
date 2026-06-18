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
/// Builder struct
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
    ///
    /// Assembly algorithm:
    /// 1. Always include all NeverTrim blocks in full.
    /// 2. Sort remaining blocks by priority (high → normal → low).
    /// 3. Fill blocks in priority order, respecting per-block budgets.
    /// 4. If total exceeds budget, drop lowest-priority blocks first.
    pub fn assemble(&self) -> (String, ContextAssemblyMeta) {
        let selected_message_ids: Vec<i64> = Vec::new();
        let wiki_files: Vec<String> = Vec::new();
        let mut block_counts: HashMap<String, usize> = HashMap::new();
        let mut dropped_blocks: Vec<String> = Vec::new();

        let mut result_parts: Vec<String> = Vec::new();
        let effective_budget = self.effective_budget();
        let mut used = 0usize;

        // Phase 1: Always include NeverTrim blocks
        for block in &self.blocks {
            if block.priority != BlockPriority::NeverTrim {
                continue;
            }
            let rendered = block.render();
            let len = rendered.len();
            result_parts.push(rendered);
            used += len;
            block_counts.insert(block.label.clone(), len);
        }

        // Phase 2: Sort remaining by priority (High > Normal > Low)
        let mut remaining: Vec<&ContextBlock> = self
            .blocks
            .iter()
            .filter(|b| b.priority != BlockPriority::NeverTrim)
            .collect();
        remaining.sort_by_key(|b| b.priority);

        // Phase 3: Fill blocks in priority order, respecting budget
        for block in &remaining {
            if used >= effective_budget {
                dropped_blocks.push(block.label.clone());
                continue;
            }

            let block_budget = if block.max_chars > 0 {
                effective_budget
                    .saturating_sub(used)
                    .min(block.max_chars)
            } else {
                effective_budget.saturating_sub(used)
            };

            if block_budget == 0 {
                dropped_blocks.push(block.label.clone());
                continue;
            }

            // Temporarily create a copy with the reduced budget for rendering
            let adjusted = ContextBlock {
                label: block.label.clone(),
                priority: block.priority,
                content: block.content.clone(),
                max_chars: block_budget,
            };
            let rendered = adjusted.render();
            let len = rendered.len();
            result_parts.push(rendered);
            used += len;
            block_counts.insert(block.label.clone(), len);
        }

        let total_chars = used;
        let joined = result_parts.join("\n\n");

        let meta = ContextAssemblyMeta {
            selected_message_ids,
            wiki_files,
            block_counts,
            dropped_blocks,
            total_chars,
        };

        (joined, meta)
    }

    /// Parse message IDs from a block's label or content (utility).
    /// This is a no-op for now — IDs are tracked explicitly via add_message_id.
    pub fn track_message_id(&mut self, id: i64, _label: &str) {
        // In a future iteration this could store per-block IDs.
        // Currently metadata is collected at assemble time.
        let _ = id;
    }

    /// Track a wiki file reference.
    pub fn track_wiki_file(&mut self, path: &str) {
        // Currently metadata is collected at assemble time.
        let _ = path;
    }

    /// Get a reference to the blocks for inspection.
    pub fn blocks(&self) -> &[ContextBlock] {
        &self.blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_assembly() {
        let mut builder = ContextBuilder::new().with_budget(5000).with_output_reserve(0);
        builder.add_block(ContextBlock::never_trim("identity", "You are a helpful AI."));
        builder.add_block(ContextBlock::new(
            "memory",
            BlockPriority::NeverTrim,
            "User prefers concise responses.\nKey facts: Rust expert.",
            3000,
        ));
        builder.add_block(ContextBlock::new(
            "recent_thread",
            BlockPriority::Normal,
            "User: what's the weather?\nAssistant: It's sunny.",
            2000,
        ));
        builder.add_block(ContextBlock::new(
            "retrieved_wiki",
            BlockPriority::Low,
            "# Project Info\nThe project uses tokio.",
            5000,
        ));

        let (prompt, meta) = builder.assemble();
        assert!(prompt.contains("You are a helpful AI."));
        assert!(prompt.contains("User prefers concise responses."));
        assert!(meta.total_chars > 0);
        assert_eq!(meta.dropped_blocks.len(), 0);
    }

    #[test]
    fn test_budget_trimming() {
        let mut builder = ContextBuilder::new().with_budget(100).with_output_reserve(0);
        builder.add_block(ContextBlock::never_trim("identity", "I am an AI."));
        builder.add_block(ContextBlock::new(
            "long_normal",
            BlockPriority::Normal,
            &"A".repeat(200),
            200,
        ));
        builder.add_block(ContextBlock::new(
            "long_low",
            BlockPriority::Low,
            &"B".repeat(200),
            200,
        ));

        let (_prompt, meta) = builder.assemble();
        // The never-trim block takes ~20 chars, normal takes 80 more = ~100
        // Low priority should be dropped
        assert!(meta.dropped_blocks.contains(&"long_low".to_string()));
    }

    #[test]
    fn test_never_trim_is_always_included() {
        let mut builder = ContextBuilder::new().with_budget(10).with_output_reserve(0);
        builder.add_block(ContextBlock::never_trim("critical", "This must always be here."));
        builder.add_block(ContextBlock::new(
            "other",
            BlockPriority::Low,
            "This might get dropped.",
            500,
        ));

        let (prompt, meta) = builder.assemble();
        assert!(prompt.contains("This must always be here."));
        assert!(meta.dropped_blocks.contains(&"other".to_string()));
    }

    #[test]
    fn test_empty_builder() {
        let builder = ContextBuilder::new();
        let (prompt, meta) = builder.assemble();
        assert!(prompt.is_empty());
        assert_eq!(meta.total_chars, 0);
    }
}

// ---------------------------------------------------------------------------
// Question classifier

/// Classification result for a user message.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QueryClass {
    /// Simple greeting/chit-chat — no retrieval needed.
    Greeting,
    /// Factual question about the system, project, or past conversations.
    Factual,
    /// Command or instruction to perform an action.
    Command,
    /// Follow-up that references previous context.
    FollowUp,
    /// Question that needs real-time/external data.
    ExternalQuery,
}

/// Heuristic classifier that determines whether a message needs retrieval.
///
/// Returns a tuple of (classification, retrieval_should_run).
pub fn classify_query(content: &str) -> (QueryClass, bool) {
    let trimmed = content.trim();

    // Empty or very short
    if trimmed.len() < 3 {
        return (QueryClass::Greeting, false);
    }

    // Commands (start with / or are clear imperatives)
    if trimmed.starts_with('/') {
        return (QueryClass::Command, false);
    }

    let lower = trimmed.to_lowercase();

    // Simple greetings / acknowledgments
    let greetings = [
        "hi", "hello", "hey", "thanks", "ok", "okay", "yes", "no", "done",
        "good", "great", "nice", "cool", "bye", "👍", "✅", "🎉",
    ];
    if greetings.contains(&lower.as_str()) {
        return (QueryClass::Greeting, false);
    }

    // Follow-ups (short, referencing previous context)
    if trimmed.len() < 60 && lower.starts_with("what about")
        || lower.starts_with("how about")
        || lower.starts_with("and the")
        || lower.starts_with("continue")
        || trimmed.len() < 15
    {
        return (QueryClass::FollowUp, false);
    }

    // External queries (weather, time, news, web)
    let external_keywords = [
        "weather", "news", "forecast", "stock", "price",
    ];
    // Check with word boundaries to avoid false positives (e.g. "implement" matching "time")
    let word_boundary_match = |lower: &str, keyword: &str| -> bool {
        lower.contains(&format!(" {} ", keyword))
            || lower.starts_with(&format!("{} ", keyword))
            || lower.ends_with(&format!(" {}", keyword))
            || lower == keyword
    };
    if external_keywords.iter().any(|k| word_boundary_match(&lower, k)) {
        return (QueryClass::ExternalQuery, true);
    }

    // Factual questions — likely need retrieval
    if lower.starts_with("what") || lower.starts_with("who")
        || lower.starts_with("where") || lower.starts_with("when")
        || lower.starts_with("why") || lower.starts_with("how")
        || lower.starts_with("is ") || lower.starts_with("are ")
        || lower.starts_with("does") || lower.starts_with("do ")
        || lower.starts_with("can ") || lower.starts_with("could")
        || lower.starts_with("did ") || lower.starts_with("was ")
        || lower.starts_with("were ") || lower.starts_with("has ")
        || lower.starts_with("have ") || lower.starts_with("tell ")
        || lower.starts_with("explain") || lower.starts_with("show ")
        || lower.contains("?")
    {
        return (QueryClass::Factual, true);
    }

    // Long messages with substantial content — likely a task
    if trimmed.len() > 100 {
        // Still may need retrieval for context
        return (QueryClass::Command, true);
    }

    // Default: brief commands, no retrieval
    (QueryClass::Command, false)
}

// ---------------------------------------------------------------------------
// Re-ranking utilities
// ---------------------------------------------------------------------------

/// A scored retrieval result with metadata for re-ranking.
#[derive(Debug, Clone)]
pub struct ScoredResult {
    /// Relevance score (higher = more relevant).
    pub score: f32,
    /// Message or wiki ID for dedup.
    pub id: String,
    /// Content snippet.
    pub snippet: String,
    /// Channel/thread info for recency boost.
    pub thread_id: Option<i64>,
    /// Whether this was confirmed by the user.
    pub user_confirmed: bool,
}

/// Apply re-ranking to a list of scored results.
/// Factors: recency, same-thread boost, user-confirmed boost.
pub fn rerank_results(results: &mut [ScoredResult], current_thread_id: Option<i64>) {
    for r in results.iter_mut() {
        // Same-thread boost: +0.2 if from the same conversation thread
        if let Some(current) = current_thread_id {
            if r.thread_id == Some(current) {
                r.score += 0.2;
            }
        }
        // User-confirmed boost: +0.3
        if r.user_confirmed {
            r.score += 0.3;
        }
    }
    // Sort by score descending
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
}

#[cfg(test)]
mod classifier_tests {
    use super::*;

    #[test]
    fn test_greeting() {
        let (cls, retrieve) = classify_query("hi");
        assert_eq!(cls, QueryClass::Greeting);
        assert!(!retrieve);
    }

    #[test]
    fn test_factual_question() {
        let (cls, retrieve) = classify_query("What is the project's architecture?");
        assert_eq!(cls, QueryClass::Factual);
        assert!(retrieve);
    }

    #[test]
    fn test_command() {
        let (cls, retrieve) = classify_query("/help");
        assert_eq!(cls, QueryClass::Command);
        assert!(!retrieve);
    }

    #[test]
    fn test_followup() {
        let (cls, retrieve) = classify_query("continue");
        assert_eq!(cls, QueryClass::FollowUp);
        assert!(!retrieve);
    }

    #[test]
    fn test_external_query() {
        let (cls, retrieve) = classify_query("Show me the weather forecast");
        assert_eq!(cls, QueryClass::ExternalQuery);
        assert!(retrieve);
    }

    #[test]
    fn test_long_task() {
        let msg = "I need you to implement a new feature in the omniagent project. \
                   It should add a way to search the wiki by text content. \
                   Please create the necessary files and update the documentation.";
        let (cls, retrieve) = classify_query(msg);
        assert_eq!(cls, QueryClass::Command);
        assert!(retrieve); // Long messages also trigger retrieval
    }
}

