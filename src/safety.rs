//! Safety checks for outgoing agent responses.
//! Scans for secrets, API keys, tokens, and other sensitive data
//! that should never be sent to external platforms (Telegram, etc.).

use once_cell::sync::Lazy;
use regex::Regex;

/// Static secret patterns. Uses once_cell::sync::Lazy for zero-cost init.
static SECRET_PATTERNS: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    vec![
        // API keys: OpenAI, Anthropic, DeepSeek, etc.
        (
            Regex::new(r"(?i)\b(sk-[a-zA-Z0-9]{20,})\b").expect("built-in regex should be valid"),
            "API key",
        ),
        // JWT / Bearer tokens
        (
            Regex::new(r"(?i)\b(eyJ[a-zA-Z0-9_-]{10,}\.[a-zA-Z0-9_-]{10,}\.[a-zA-Z0-9_-]{10,})\b")
                .expect("built-in regex should be valid"),
            "JWT token",
        ),
        // Database connection strings
        (
            Regex::new(r"(?i)(postgres://[^@]+@)").expect("built-in regex should be valid"),
            "PostgreSQL connection string",
        ),
        (
            Regex::new(r"(?i)(mysql://[^:***@]+@)").expect("built-in regex should be valid"),
            "MySQL connection string",
        ),
        // AWS keys
        (
            Regex::new(r"(?i)\b(AKIA[0-9A-Z]{16})\b").expect("built-in regex should be valid"),
            "AWS access key",
        ),
        // Private keys
        (
            Regex::new(r"-----BEGIN\s?(RSA|EC|DSA|OPENSSH)?\s?PRIVATE KEY-----").expect("built-in regex should be valid"),
            "Private key",
        ),
        // Slack/Hub tokens
        (
            Regex::new(r"(?i)\b(xox[baprs]-[0-9a-z]{10,})\b").expect("built-in regex should be valid"),
            "Slack token",
        ),
        // Generic: long base64 strings that look like tokens
        (
            Regex::new(r"\b([a-zA-Z0-9+/]{40,}={0,2})\b").expect("built-in regex should be valid"),
            "Potential token",
        ),
    ]
});

/// Result of a secret scan.
#[derive(Debug)]
pub struct SecretMatch {
    pub pattern: &'static str,
    pub start: usize,
    pub end: usize,
}

/// Scan content for potential secrets. Returns a list of matches.
/// Each match includes the pattern name and byte positions for redaction.
pub fn scan_for_secrets(content: &str) -> Vec<SecretMatch> {
    let mut matches: Vec<SecretMatch> = Vec::new();

    for (regex, label) in SECRET_PATTERNS.iter() {
        for cap in regex.captures_iter(content) {
            if let Some(m) = cap.get(0) {
                // Skip very short pseudo-matches
                if m.len() < 8 {
                    continue;
                }
                matches.push(SecretMatch {
                    pattern: label,
                    start: m.start(),
                    end: m.end(),
                });
            }
        }
    }

    matches
}

/// Redact all detected secrets in content, replacing with `[REDACTED <type>]`.
pub fn redact_secrets(content: &str) -> String {
    let mut result = content.to_string();

    // Collect all matches sorted by start position (reverse order for safe replacement)
    let mut matches = scan_for_secrets(content);
    matches.sort_by(|a, b| b.start.cmp(&a.start)); // reverse: replace from end to preserve positions

    for m in &matches {
        let replacement = format!("[REDACTED {}]", m.pattern);
        // Since we iterate reverse, positions are still valid in the current result
        result.replace_range(m.start..m.end, &replacement);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_openai_key() {
        let matches = scan_for_secrets("My key is sk-abcd1234efgh5678ijkl9012mnop3456");
        assert!(!matches.is_empty());
        assert!(matches.iter().any(|m| m.pattern == "API key"));
    }

    #[test]
    fn test_detect_jwt() {
        let matches = scan_for_secrets(
            "token: eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3j6Z5NkQv7A",
        );
        assert!(!matches.is_empty());
        assert!(matches.iter().any(|m| m.pattern == "JWT token"));
    }

    #[test]
    fn test_detect_postgres_url() {
        let matches = scan_for_secrets("postgres://user:supersecret@localhost:5432/db");
        assert!(!matches.is_empty());
        assert!(matches
            .iter()
            .any(|m| m.pattern == "PostgreSQL connection string"));
    }

    #[test]
    fn test_redact_api_key() {
        let result = redact_secrets("Key: sk-test1234567890abcdefgh1234567890, ok");
        assert!(!result.contains("sk-test"));
        assert!(result.contains("[REDACTED API key]"));
    }

    #[test]
    fn test_clean_content_unchanged() {
        let result = redact_secrets("Hello, this is a normal message without secrets.");
        assert_eq!(result, "Hello, this is a normal message without secrets.");
    }
}
