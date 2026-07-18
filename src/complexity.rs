//! Shared complexity classification: used by context building and planning mode resolution.
//!
//! Thresholds are configurable via environment variables:
//! - `PLANNING_COMPLEXITY_SIMPLE_MAX_CHARS` (default 60)
//! - `PLANNING_COMPLEXITY_STANDARD_MAX_CHARS` (default 200)
//! - `PLANNING_COMPLEXITY_KEYWORDS` (default comma-separated list)

/// Complexity tier for a user message: determines planning depth and tooling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Complexity {
    /// Greeting, acknowledgment, simple command: skip planning, execute directly.
    Simple,
    /// Standard request: plan as configured.
    Standard,
    /// Complex multi-step task (implement, refactor, design, kanban/cron): plan + auto-subtasks.
    Complex,
}

/// Classify a message into a complexity tier.
///
/// Simple: < `simple_max` chars, greeting words, acknowledgment.
/// Complex: contains action keywords (implement/refactor/design/etc.),
///           or is a kanban/cron task with substantive content,
///           or length > `standard_max` chars.
/// Standard: everything else.
pub fn classify_complexity(
    content: &str,
    msg_type: &str,
    metadata_word_count: Option<usize>,
) -> Complexity {
    let trimmed = content.trim();
    let char_len = trimmed.len();
    let word_count = trimmed.split_whitespace().count();

    // Read thresholds from global config, used after hot-reload
    let (simple_max, standard_max) = crate::agent::config::get_global()
        .map(|g| {
            let c = g.read().expect("GlobalConfig lock poisoned");
            (c.planning_complexity_simple_max_chars, c.planning_complexity_standard_max_chars)
        })
        .unwrap_or((60, 200));

    // Simple: short messages, greetings, acknowledgments
    if char_len < simple_max || word_count <= 3 {
        let lower = trimmed.to_lowercase();
        let greetings = [
            "hi",
            "hello",
            "hey",
            "ok",
            "okay",
            "k",
            "thanks",
            "ty",
            "thx",
            "\u{1f44d}",
            "\u{1f64f}",
            "done",
            "yes",
            "no",
            "good",
            "great",
        ];
        if word_count <= 2 || greetings.iter().any(|g| lower.contains(g)) {
            return Complexity::Simple;
        }
    }

    // Complex: specific action keywords or kanban/cron tasks with content
    let lower = trimmed.to_lowercase();
    let keywords_raw = crate::agent::config::get_global()
        .map(|g| g.read().expect("GlobalConfig lock poisoned").planning_complexity_keywords.clone())
        .unwrap_or_else(|| {
            "implement,refactor,redesign,architecture,create,build,design,develop,\
             migrate,restructure,overhaul,rewrite,configure,set up,deploy,integrate,\
             add feature,fix bug,resolve issue,multi-step,complex"
            .to_string()
    });
    let complex_keywords: Vec<&str> = keywords_raw.split(',').map(|s| s.trim()).collect();

    let is_complex_keyword = complex_keywords.iter().any(|kw| lower.contains(kw));

    // Structured tasks (kanban/cron) with a body longer than a title
    let is_structured_task = crate::agent::helpers::is_structured_msg_type(msg_type)
        && metadata_word_count.map(|c| c > 10).unwrap_or(false);

    let has_substantive_length = char_len > standard_max;

    if is_complex_keyword || is_structured_task || has_substantive_length {
        return Complexity::Complex;
    }

    Complexity::Standard
}
