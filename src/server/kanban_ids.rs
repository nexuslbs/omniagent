//! Human-readable kanban task IDs: `task_<board_slug>_<title_slug>`.
//!
//! Instead of a bare random hex (`task_18cfbec91f6e104a`), new task IDs are
//! derived from the board name + task title. The derivation is deterministic
//! and pure (unit-tested below); collision handling appends the original
//! random hex suffix (`task_omnidev_fix_husky_18cfbec91f6e104a`).
//!
//! Slug rules (applied to BOTH the board name and the task title):
//! 1. Remove diacritics (NFD-decompose, drop combining marks).
//! 2. Lowercase the result.
//! 3. Replace every run of special/non-alphanumeric chars with a single `_`.
//! 4. Collapse multiple consecutive `_` into a single `_` (covered by rule 3).
//! 5. Trim leading and trailing `_`.
//! 6. Abbreviate if too long: drop whole words from the END of the title slug
//!    (board slug kept intact) until the ID fits `MAX_TASK_ID_LEN`.
//! 7. Uniqueness: on collision append `_<hex>` (single `_` separator).

use unicode_normalization::UnicodeNormalization;

/// Maximum total length of a generated task ID (`task_<board>_<title>`).
///
/// Chosen limit (documented): **50** chars total. The task spec allows
/// ~60-80 (or a shorter max at the implementer's discretion); 50 reproduces
/// the canonical example `task_omnidev_fix_omni_dashboard_husky_pre_commit`
/// (45 chars) and keeps IDs short enough for Mattermost/URL contexts. Longer
/// titles are abbreviated by dropping whole words from the end of the title
/// slug; the board slug is always kept intact.
pub(crate) const MAX_TASK_ID_LEN: usize = 50;

/// Combining diacritical-mark code point ranges (same ranges used in
/// `server::plugins_reload::sanitize_plugin_name`).
fn is_combining_mark(ch: char) -> bool {
    matches!(
        ch as u32,
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F
    )
}

/// Slugify: diacritics removed, lowercased, every run of non-alphanumeric
/// chars collapsed to a single `_`, leading/trailing `_` trimmed. Returns ""
/// when no alphanumeric content remains (e.g. a title of only punctuation).
pub(crate) fn slugify(input: &str) -> String {
    // Rules 1 + 2: NFD-decompose (diacritics become base + combining mark),
    // drop combining marks, then lowercase.
    let normalized = input
        .nfd()
        .filter(|ch| !is_combining_mark(*ch))
        .collect::<String>()
        .to_lowercase();

    // Rules 3 + 4 + 5: non-alphanumeric runs -> single `_` (collapsing),
    // never at the start (trim leading) or end (trim trailing).
    let mut out = String::with_capacity(normalized.len());
    let mut pending_underscore = false;
    for ch in normalized.chars() {
        if ch.is_alphanumeric() {
            if pending_underscore && !out.is_empty() {
                out.push('_');
            }
            pending_underscore = false;
            out.push(ch);
        } else {
            pending_underscore = true;
        }
    }
    out
}

/// Abbreviate `title_slug` to fit `budget` chars: drop whole words from the
/// END (keeping the board slug intact). Only when even a single title word
/// cannot fit is the slug hard-truncated mid-word with a trailing `...`.
fn abbreviate_title_slug(title_slug: &str, budget: usize) -> String {
    if title_slug.len() <= budget {
        return title_slug.to_string();
    }
    let words: Vec<&str> = title_slug.split('_').filter(|w| !w.is_empty()).collect();
    let mut keep: Vec<&str> = Vec::new();
    let mut len = 0usize;
    for w in &words {
        let add = if keep.is_empty() {
            w.len()
        } else {
            w.len() + 1
        };
        if len + add > budget {
            break;
        }
        len += add;
        keep.push(w);
    }
    let joined = keep.join("_");
    if joined.is_empty() {
        // Even the first word alone cannot fit: hard-truncate mid-word and
        // mark the cut with an ellipsis, e.g. `..._fix_omni_dashboard_hus...`.
        let mut t: String = title_slug.chars().take(budget.saturating_sub(3)).collect();
        t.push_str("...");
        return t;
    }
    joined
}

/// Build the base task ID `task_<board_slug>_<title_slug>` (no uniqueness
/// suffix yet). Returns "" only when neither slug has any alphanumeric
/// content (caller falls back to the plain hex id).
pub(crate) fn build_task_id(board: &str, title: &str, max_len: usize) -> String {
    let board_slug = slugify(board);
    let title_slug = slugify(title);
    match (board_slug.is_empty(), title_slug.is_empty()) {
        (false, false) => {
            let budget = max_len.saturating_sub("task_".len() + board_slug.len() + 1);
            format!(
                "task_{board_slug}_{}",
                abbreviate_title_slug(&title_slug, budget)
            )
        }
        (false, true) => format!("task_{board_slug}"),
        (true, false) => {
            let budget = max_len.saturating_sub("task_".len());
            format!("task_{}", abbreviate_title_slug(&title_slug, budget))
        }
        (true, true) => String::new(),
    }
}

/// Uniqueness suffix (rule 7): `base_<hex>` for the first collision, then
/// `base_<hex>_<attempt>` (attempt as lowercase hex) for the astronomically
/// unlikely follow-up collisions.
pub(crate) fn append_unique_suffix(base: &str, hex: &str, attempt: u64) -> String {
    if attempt == 0 {
        format!("{base}_{hex}")
    } else {
        format!("{base}_{hex}_{attempt:x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_removes_diacritics() {
        assert_eq!(
            slugify("Coup d'État has Diacrítics"),
            "coup_d_etat_has_diacritics"
        );
    }

    #[test]
    fn slugify_special_chars_become_underscore() {
        assert_eq!(slugify("format:check"), "format_check");
        assert_eq!(slugify("d'État"), "d_etat");
        assert_eq!(
            slugify("Fix omni-dashboard husky pre-commit"),
            "fix_omni_dashboard_husky_pre_commit"
        );
    }

    #[test]
    fn slugify_collapses_special_char_runs() {
        // Spaces, dashes, parens, colons... consecutive specials -> one `_`.
        assert_eq!(slugify("a  -- b (c)"), "a_b_c");
        assert_eq!(slugify("x:::y__z"), "x_y_z");
        assert_eq!(slugify("  --hello--  "), "hello");
        assert_eq!(slugify("_fix_"), "fix");
    }

    #[test]
    fn slugify_empty_when_no_alphanumeric() {
        assert_eq!(slugify("!!!--  ..."), "");
    }

    #[test]
    fn build_task_id_diacritics_full_example() {
        let id = build_task_id("Board-Name", "Coup d'État has Diacrítics", MAX_TASK_ID_LEN);
        assert_eq!(id, "task_board_name_coup_d_etat_has_diacritics");
        assert!(id.len() <= MAX_TASK_ID_LEN);
    }

    #[test]
    fn build_task_id_abbreviates_long_title() {
        // Canonical spec example: board `omnidev`, the husky-pre-commit title.
        let id = build_task_id(
            "omnidev",
            "Fix omni-dashboard husky pre-commit format:check (repo not prettier-clean)",
            MAX_TASK_ID_LEN,
        );
        assert_eq!(id, "task_omnidev_fix_omni_dashboard_husky_pre_commit");
        assert!(id.len() <= MAX_TASK_ID_LEN);
        // The board slug is always kept intact.
        assert!(id.starts_with("task_omnidev_"));
    }

    #[test]
    fn build_task_id_hard_truncates_single_word() {
        // A single-word title that cannot fit is cut mid-word with "...".
        let id = build_task_id("b", "abcdefghijklmnopqrstuvwxyz", 12);
        assert_eq!(id, "task_b_ab...");
        assert!(id.len() <= 12);
    }

    #[test]
    fn build_task_id_board_only_and_title_only() {
        assert_eq!(
            build_task_id("My Board", "!!!", MAX_TASK_ID_LEN),
            "task_my_board"
        );
        assert_eq!(
            build_task_id("", "Hello World", MAX_TASK_ID_LEN),
            "task_hello_world"
        );
        assert_eq!(build_task_id("", "", MAX_TASK_ID_LEN), "");
    }

    #[test]
    fn duplicate_id_appends_hex_suffix() {
        // Emulates the handler loop: base exists -> append hex; suffixed id
        // exists too (astronomically unlikely) -> append attempt counter.
        let base = build_task_id(
            "omnidev",
            "Fix husky pre-commit format:check",
            MAX_TASK_ID_LEN,
        );
        assert_eq!(base, "task_omnidev_fix_husky_pre_commit_format_check");
        let hex = "18cfbec91f6e104a";

        let taken: std::collections::HashSet<String> =
            [base.clone(), append_unique_suffix(&base, hex, 0)]
                .into_iter()
                .collect();
        let mut id = base.clone();
        for attempt in 0..5u64 {
            if !taken.contains(&id) {
                break;
            }
            id = append_unique_suffix(&base, hex, attempt);
        }
        // First collision -> `base_hex`; second -> `base_hex_2` (attempt 1 hex).
        assert_eq!(id, format!("{base}_{hex}_1"));
        assert!(!taken.contains(&id));
        // The primary collision form (spec example shape):
        assert_eq!(append_unique_suffix(&base, hex, 0), format!("{base}_{hex}"));
    }
}
