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
            let c = g.read();
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
        .map(|g| g.read().planning_complexity_keywords.clone())
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

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Simple: short greetings ───

    #[test]
    fn test_simple_short_greetings() {
        for greeting in &["hi", "ok", "hey", "thanks", "ty", "thx", "done", "yes", "no", "good", "great"] {
            assert_eq!(
                classify_complexity(greeting, "user", None),
                Complexity::Simple,
                "expected '{:?}' to be Simple",
                greeting,
            );
        }
    }

    #[test]
    fn test_simple_two_word_greeting() {
        assert_eq!(classify_complexity("hi there", "user", None), Complexity::Simple);
        assert_eq!(classify_complexity("thank you", "user", None), Complexity::Simple);
        assert_eq!(classify_complexity("good morning", "user", None), Complexity::Simple);
    }

    #[test]
    fn test_simple_three_word_greeting() {
        // 3-words containing a greeting word
        assert_eq!(classify_complexity("thanks a lot", "user", None), Complexity::Simple);
        assert_eq!(classify_complexity("good morning all", "user", None), Complexity::Simple);
    }

    #[test]
    fn test_simple_three_words_no_greeting_short() {
        // 3 words, no greeting, but < 60 chars: enters simple block due to word_count<=3,
        // but inner condition fails (word_count>2 and no greeting) => falls through
        // Then checked for complex: no keywords, not structured, not >200 => Standard
        assert_eq!(classify_complexity("this is a test", "user", None), Complexity::Standard);
    }

    #[test]
    fn test_simple_single_word_no_greeting() {
        // Single word not in greetings list, short (<60) => word_count <= 2 => Simple
        assert_eq!(classify_complexity("foo", "user", None), Complexity::Simple);
    }

    #[test]
    fn test_simple_two_word_no_greeting() {
        assert_eq!(classify_complexity("foo bar", "user", None), Complexity::Simple);
    }

    // ─── Simple: message ≤ 3 words even if longer than 60 chars ───

    #[test]
    fn test_simple_three_words_longer_than_60() {
        let long_greeting = "hello".to_string() + &"a".repeat(60);
        assert_eq!(classify_complexity(&long_greeting, "user", None), Complexity::Simple);
    }

    #[test]
    fn test_simple_three_words_no_greeting_long() {
        // 3 words, >60 chars, no greeting word
        // enters simple block due to word_count<=3, inner condition fails
        // then checked: is_complex_keyword? no. is_structured_task? no (user msg_type). has_substantive_length? yes if >200
        let long = "foo".to_string() + &"b".repeat(200) + " bar";
        // If length > 200 => Complex. Check if it's < 200.
        let medium = "foo".to_string() + &"b".repeat(100) + " bar";
        assert_eq!(classify_complexity(&medium, "user", None), Complexity::Standard);
    }

    // ─── Complex: action keywords ───

    #[test]
    fn test_complex_action_keyword_implement() {
        assert_eq!(classify_complexity("implement a new feature", "user", None), Complexity::Complex);
    }

    #[test]
    fn test_complex_action_keyword_refactor() {
        assert_eq!(classify_complexity("refactor the codebase", "user", None), Complexity::Complex);
    }

    #[test]
    fn test_complex_action_keyword_create() {
        assert_eq!(classify_complexity("create a new module", "user", None), Complexity::Complex);
    }

    #[test]
    fn test_complex_action_keyword_build() {
        assert_eq!(classify_complexity("build the application", "user", None), Complexity::Complex);
    }

    #[test]
    fn test_complex_action_keyword_design() {
        assert_eq!(classify_complexity("design the architecture", "user", None), Complexity::Complex);
    }

    #[test]
    fn test_complex_action_keyword_complex() {
        assert_eq!(classify_complexity("complex multi-step task", "user", None), Complexity::Complex);
    }

    #[test]
    fn test_complex_action_keyword_multi_step() {
        assert_eq!(classify_complexity("multi-step process", "user", None), Complexity::Complex);
    }

    // ─── Complex: structured msg_type ───

    #[test]
    fn test_complex_kanban_with_metadata() {
        assert_eq!(classify_complexity("task title", "kanban", Some(15)), Complexity::Complex);
    }

    #[test]
    fn test_complex_cron_with_metadata() {
        assert_eq!(classify_complexity("cron job description", "cron", Some(20)), Complexity::Complex);
    }

    #[test]
    fn test_complex_cause_with_metadata() {
        assert_eq!(classify_complexity("cause message", "Cause", Some(12)), Complexity::Complex);
    }

    #[test]
    fn test_structured_task_low_word_count_not_complex() {
        // metadata_word_count <= 10 => not complex via structured task path
        assert_eq!(classify_complexity("task", "kanban", Some(5)), Complexity::Standard);
    }

    #[test]
    fn test_structured_task_no_metadata_not_complex() {
        assert_eq!(classify_complexity("task", "kanban", None), Complexity::Standard);
    }

    // ─── Complex: length > standard_max (200) ───

    #[test]
    fn test_complex_long_content() {
        let long = "a".repeat(201);
        assert_eq!(classify_complexity(&long, "user", None), Complexity::Complex);
    }

    #[test]
    fn test_standard_at_200_chars() {
        let s = "a".repeat(200);
        assert_eq!(classify_complexity(&s, "user", None), Complexity::Standard);
    }

    #[test]
    fn test_complex_just_over_200() {
        let s = "a".repeat(201);
        assert_eq!(classify_complexity(&s, "user", None), Complexity::Complex);
    }

    // ─── Standard: everything else ───

    #[test]
    fn test_standard_medium_message() {
        assert_eq!(classify_complexity("hello world how are you today", "user", None), Complexity::Simple);
        // "hello world how are you today" has word_count=6, <60 chars, contains "hello" greeting => Simple
    }

    #[test]
    fn test_standard_no_keywords() {
        // 4 words, no greeting, <60 chars => enters simple block, inner condition fails => Standard
        assert_eq!(classify_complexity("this is a test", "user", None), Complexity::Standard);
    }

    #[test]
    fn test_standard_short_no_keyword() {
        assert_eq!(classify_complexity("please do this thing", "user", None), Complexity::Standard);
    }

    // ─── Edge cases ───

    #[test]
    fn test_empty_content() {
        assert_eq!(classify_complexity("", "user", None), Complexity::Simple);
    }

    #[test]
    fn test_single_word() {
        assert_eq!(classify_complexity("hello", "user", None), Complexity::Simple);
    }

    #[test]
    fn test_exact_60_chars_greeting_word() {
        let s = "hi".to_string() + &"x".repeat(58);
        assert_eq!(s.len(), 60);
        // 60 chars, char_len=60, not < 60, word_count=2 <= 3 => enters simple block
        // word_count=2 <= 2 => Simple
        assert_eq!(classify_complexity(&s, "user", None), Complexity::Simple);
    }

    #[test]
    fn test_exact_60_chars_no_greeting() {
        let s = "a".repeat(60);
        assert_eq!(s.len(), 60);
        // 60 chars, char_len=60, not < 60, word_count=1 <= 3 => enters simple block
        // word_count=1 <= 2 => Simple (single word)
        assert_eq!(classify_complexity(&s, "user", None), Complexity::Simple);
    }

    #[test]
    fn test_unicode_content() {
        assert_eq!(classify_complexity("héllo wörld", "user", None), Complexity::Simple);
        // word_count=2 <= 2 => Simple
    }

    #[test]
    fn test_unicode_long() {
        let s = "héllo wörld " .repeat(20);
        assert!(s.len() > 200);
        assert_eq!(classify_complexity(&s, "user", None), Complexity::Complex);
    }

    #[test]
    fn test_unicode_greeting_edge() {
        assert_eq!(classify_complexity("café", "user", None), Complexity::Simple);
        // single word, <= 2 => Simple
    }

    #[test]
    fn test_whitespace_trimmed() {
        assert_eq!(classify_complexity("  hi  ", "user", None), Complexity::Simple);
        assert_eq!(classify_complexity("  \n  \t  ", "user", None), Complexity::Simple);
    }
}
