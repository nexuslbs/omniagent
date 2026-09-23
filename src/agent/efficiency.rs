//! EFF-1: engine-level efficiency contract (root-cause fix for the
//! re-read / no-progress / ignored-stop-signal loop class, incident 2874).
//!
//! This module is deliberately self-contained and side-effect free so it can be
//! unit-tested without an LLM or a database. The agent loop owns an
//! [`EfficiencyLedger`] per running thread and asks it three questions:
//!
//! 1. `observe(tool, args, iter)` - has this read *already* been performed in
//!    this thread? Unlike the WS-4b exact-arg-hash guard this compares the
//!    *effective read scope*, so re-reading the same file with a different
//!    `offset`/`limit` (or a reordered JSON argument object) is recognised as a
//!    duplicate, which is exactly the shape that burned thread 2874
//!    (`docker-compose.yml` at offset 262, 20x, varying limits).
//! 2. `steering(iter)` - an ESCALATING, purely behavioural signal appended to
//!    the running context when the agent keeps reading without producing a
//!    state change. There is no hard cap and no silent truncation: the agent is
//!    told, factually, that it is looping and can comply within the same round.
//! 3. `classify_fatal_provider_error(msg)` - 402 / insufficient balance /
//!    auth failures are terminal: retrying them three times only burns the
//!    remaining provider balance (incident 2874 ended on `402 Payment Required`).
//!
//! Counters (`duplicate_reads`, `readonly_streak`, `state_changes`) are
//! surfaced through `metrics_summary()` so a regression is visible without the
//! operator having to complain.


/// Paging mode of a `filesystem__read` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagingMode {
    Chars,
    Lines,
}

impl PagingMode {
    pub fn tag(self) -> &'static str {
        match self {
            PagingMode::Chars => "chars",
            PagingMode::Lines => "lines",
        }
    }
}

/// The *effective* scope of a read-only call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadScope {
    /// A paged file read: same path + same paging mode, half-open interval.
    FilePage {
        path: String,
        mode: PagingMode,
        start: usize,
        end: usize,
    },
    /// A content search: search kind + pattern + base path.
    Search {
        kind: String,
        pattern: String,
        base: String,
    },
    /// An opaque read-only invocation: normalised identity string.
    Exact { key: String },
}

impl ReadScope {
    /// Stable identity of the *target* (path / query), ignoring the page window.
    pub fn identity(&self) -> String {
        match self {
            ReadScope::FilePage { path, mode, .. } => format!("file:{}:{}", mode.tag(), path),
            ReadScope::Search { kind, pattern, base } => {
                format!("search:{}:{}:{}", kind, pattern, base)
            }
            ReadScope::Exact { key } => format!("exact:{}", key),
        }
    }

    /// Human readable description used in the duplicate stub.
    pub fn describe(&self) -> String {
        match self {
            ReadScope::FilePage {
                path,
                mode,
                start,
                end,
            } => {
                if *end == usize::MAX {
                    format!("{} {}[{}..end]", path, mode.tag(), start)
                } else {
                    format!("{} {}[{}..{}]", path, mode.tag(), start, end)
                }
            }
            ReadScope::Search { pattern, base, .. } => format!("{} in {}", pattern, base),
            ReadScope::Exact { key } => key.clone(),
        }
    }
}

/// True when two file-page scopes overlap. Open-ended pages (`end == MAX`)
/// overlap everything that starts at or after their window start.
pub fn pages_overlap(a: &ReadScope, b: &ReadScope) -> bool {
    match (a, b) {
        (
            ReadScope::FilePage {
                path: pa,
                mode: ma,
                start: sa,
                end: ea,
            },
            ReadScope::FilePage {
                path: pb,
                mode: mb,
                start: sb,
                end: eb,
            },
        ) => pa == pb && ma == mb && sa < eb && sb < ea,
        _ => false,
    }
}

/// Git subcommands that only read repository state. Anything outside this list
/// (commit, push, checkout, reset, ...) is treated as a state change so that a
/// `git commit` never clears-free reads that are still valid, and never counts
/// as a read for the read-only streak.
const GIT_READONLY: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "remote",
    "branch",
    "rev-parse",
    "ls-files",
    "ls-tree",
    "grep",
    "describe",
    "cat-file",
    "blame",
    "shortlog",
    "config",
    "tag",
    "stash",
    "worktree",
    "for-each-ref",
    "symbolic-ref",
];

fn first_git_word(argv: &[String]) -> Option<&str> {
    argv.iter()
        .map(|s| s.as_str())
        .find(|s| !s.starts_with('-'))
}

/// True when a `git__run_command` argv is read-only.
pub fn is_readonly_git_argv(argv: &[String]) -> bool {
    match first_git_word(argv) {
        Some(w) => GIT_READONLY.contains(&w),
        None => false,
    }
}

/// Parse the effective read scope of a call. `None` means "not a read we can
/// reason about" - the caller then makes no duplicate claim at all (safe).
pub fn parse_read_scope(tool: &str, args: &str) -> Option<ReadScope> {
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    let obj = v.as_object()?;
    let s = |k: &str| {
        obj.get(k)
            .and_then(|x| x.as_str())
            .map(|x| x.to_string())
    };
    let n = |k: &str| obj.get(k).and_then(|x| x.as_u64()).map(|x| x as usize);
    let b = |k: &str| obj.get(k).and_then(|x| x.as_bool());
    match tool {
        "filesystem__read" => {
            let path = s("path")?;
            if b("lines").unwrap_or(false) {
                let start = n("offset").unwrap_or(1);
                let limit = n("limit").unwrap_or(500);
                Some(ReadScope::FilePage {
                    path,
                    mode: PagingMode::Lines,
                    start,
                    end: start.saturating_add(limit),
                })
            } else {
                let start = n("offset").unwrap_or(0);
                let end = match n("limit") {
                    Some(l) => start.saturating_add(l),
                    None => usize::MAX,
                };
                Some(ReadScope::FilePage {
                    path,
                    mode: PagingMode::Chars,
                    start,
                    end,
                })
            }
        }
        "filesystem__list" | "filesystem__info" => {
            let path = s("path")?;
            Some(ReadScope::Exact {
                key: format!("{}:{}", tool, path),
            })
        }
        "filesystem__grep" | "filesystem__search" => {
            let pattern = s("pattern")?;
            let base = s("path").unwrap_or_else(|| ".".to_string());
            let glob = s("glob").unwrap_or_default();
            Some(ReadScope::Search {
                kind: format!("{}:{}", tool, glob),
                pattern,
                base,
            })
        }
        "git__status" | "git__sync" => {
            let repo = s("repo_dir")?;
            Some(ReadScope::Exact {
                key: format!("{}:{}", tool, repo),
            })
        }
        "git__run_command" => {
            let repo = s("repo_dir")?;
            let argv: Vec<String> = obj
                .get("args")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            if !is_readonly_git_argv(&argv) {
                return None;
            }
            Some(ReadScope::Exact {
                key: format!("git:{}:{}", repo, argv.join(" ")),
            })
        }
        t if t.starts_with("search__") => {
            let q = s("query").unwrap_or_default();
            let scope = s("channel_id").unwrap_or_default();
            Some(ReadScope::Exact {
                key: format!("{}:{}:{}", t, scope, q),
            })
        }
        _ => None,
    }
}

/// Tools whose successful execution counts as observable progress in the
/// thread (they change repository / workspace / delivery state).
pub fn is_state_changing(tool: &str, args: &str) -> bool {
    match tool {
        "filesystem__write"
        | "filesystem__str_replace"
        | "filesystem__insert"
        | "filesystem__apply_patch"
        | "git__commit_and_push"
        | "git__create_github_repo"
        | "git__clone_repo"
        | "git__sync"
        | "ssh__run"
        | "ssh__copy"
        | "skills__create_skill"
        | "memory__promote_to_memory"
        | "memory__save_summary" => true,
        "docker__compose" => {
            let v: Option<serde_json::Value> = serde_json::from_str(args).ok();
            match v.as_ref().and_then(|v| v.get("command")).and_then(|c| c.as_str()) {
                Some(c) => !(c.starts_with("ps") || c.starts_with("logs")),
                None => true,
            }
        }
        "git__run_command" => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(arr) = v.get("args").and_then(|x| x.as_array()) {
                    let argv: Vec<String> = arr
                        .iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect();
                    return !is_readonly_git_argv(&argv);
                }
            }
            false
        }
        _ => false,
    }
}

/// The path a write tool targets, if we can tell (used to invalidate the read
/// records of a file that actually changed on disk).
pub fn written_path(tool: &str, args: &str) -> Option<String> {
    if !matches!(
        tool,
        "filesystem__write"
            | "filesystem__str_replace"
            | "filesystem__insert"
            | "filesystem__apply_patch"
    ) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    v.get("path").and_then(|p| p.as_str()).map(|s| s.to_string())
}

/// Verdict for one observed call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadVerdict {
    /// Not a read we can reason about, or a state-changing call.
    NotGuarded,
    /// First time this scope is read: it executed.
    New,
    /// The effective result is already in the thread context.
    Duplicate { first_iter: u32, overlap: bool },
}

struct SeenRead {
    identity: String,
    scope: ReadScope,
    iteration: u32,
}

/// Per-thread efficiency ledger.
#[derive(Default)]
pub struct EfficiencyLedger {
    seen: Vec<SeenRead>,
    duplicate_reads: u32,
    readonly_streak: u32,
    state_changes: u32,
    steering_level: u8,
    total_calls: u32,
    first_state_change_at: u32,
}

const STEER_L1: u32 = 6;
const STEER_L2: u32 = 12;
const STEER_L3: u32 = 24;

impl EfficiencyLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn duplicate_reads(&self) -> u32 {
        self.duplicate_reads
    }
    pub fn readonly_streak(&self) -> u32 {
        self.readonly_streak
    }
    pub fn state_changes(&self) -> u32 {
        self.state_changes
    }
    pub fn total_calls(&self) -> u32 {
        self.total_calls
    }

    /// Observe a call *before* it is executed.
    pub fn observe(&mut self, tool: &str, args: &str, iteration: u32) -> ReadVerdict {
        self.total_calls = self.total_calls.saturating_add(1);
        let scope = match parse_read_scope(tool, args) {
            Some(s) => s,
            None => {
                if is_state_changing(tool, args) {
                    self.note_state_change();
                    if let Some(path) = written_path(tool, args) {
                        self.note_write_path(&path);
                    }
                }
                return ReadVerdict::NotGuarded;
            }
        };
        let identity = scope.identity();
        if let Some(prev) = self
            .seen
            .iter()
            .find(|s| s.identity == identity && scopes_equivalent(&s.scope, &scope))
        {
            return ReadVerdict::Duplicate {
                first_iter: prev.iteration,
                overlap: pages_overlap(&prev.scope, &scope),
            };
        }
        self.readonly_streak = self.readonly_streak.saturating_add(1);
        self.seen.push(SeenRead {
            identity,
            scope,
            iteration,
        });
        ReadVerdict::New
    }

    /// Count a call that was blocked as a duplicate (also increments the
    /// read-only streak: a blocked read is still a non-progressing call).
    pub fn record_block(&mut self) {
        self.duplicate_reads = self.duplicate_reads.saturating_add(1);
        self.readonly_streak = self.readonly_streak.saturating_add(1);
    }

    /// A state change happened (write / commit / verified mutation).
    pub fn note_state_change(&mut self) {
        self.state_changes = self.state_changes.saturating_add(1);
        self.readonly_streak = 0;
        self.steering_level = 0;
    }

    /// A file actually changed on disk: drop the stale read records for it.
    pub fn note_write_path(&mut self, path: &str) {
        self.seen.retain(|s| match &s.scope {
            ReadScope::FilePage { path: p, .. } => p != path,
            _ => true,
        });
    }

    fn next_threshold(&self) -> Option<(u8, u32)> {
        let cur = self.steering_level;
        if cur < 1 && self.readonly_streak >= STEER_L1 {
            return Some((1, STEER_L1));
        }
        if cur < 2 && self.readonly_streak >= STEER_L2 {
            return Some((2, STEER_L2));
        }
        if cur < 3 && self.readonly_streak >= STEER_L3 {
            return Some((3, STEER_L3));
        }
        None
    }

    /// Escalating, behavioural steering signal. `None` when the thread is
    /// making progress. No truncation, no cap: the agent is told the facts and
    /// can comply within the same iteration.
    pub fn steering(&mut self, iteration: u32) -> Option<String> {
        if let Some((level, threshold)) = self.next_threshold() {
            self.steering_level = level;
            return Some(steering_text(
                level,
                threshold,
                self.readonly_streak,
                self.duplicate_reads,
                self.state_changes,
                iteration,
            ));
        }
        // Keep reminding every 10 read-only calls at the current level.
        if self.steering_level > 0 && self.readonly_streak % 10 == 0 {
            return Some(steering_text(
                self.steering_level,
                self.readonly_streak,
                self.readonly_streak,
                self.duplicate_reads,
                self.state_changes,
                iteration,
            ));
        }
        None
    }

    /// Compact metric line for logs / reports.
    pub fn metrics_summary(&self, thread_id: i64) -> String {
        let ratio = if self.state_changes == 0 {
            self.readonly_streak as f64
        } else {
            self.total_calls as f64 / self.state_changes as f64
        };
        format!(
            "thread={} calls={} duplicate_reads={} readonly_streak={} state_changes={} read_write_ratio={:.2} first_state_change_iter={}",
            thread_id,
            self.total_calls,
            self.duplicate_reads,
            self.readonly_streak,
            self.state_changes,
            ratio,
            self.first_state_change_at
        )
    }

    /// Iteration of the first state change (time-to-first-commit proxy).
    pub fn mark_state_change_at(&mut self, iteration: u32) {
        if self.first_state_change_at == 0 {
            self.first_state_change_at = iteration;
        }
    }
}

fn scopes_equivalent(a: &ReadScope, b: &ReadScope) -> bool {
    match (a, b) {
        (ReadScope::FilePage { .. }, ReadScope::FilePage { .. }) => pages_overlap(a, b),
        (ReadScope::Search { .. }, ReadScope::Search { .. }) => true,
        (ReadScope::Exact { .. }, ReadScope::Exact { .. }) => true,
        _ => false,
    }
}

/// The compact stub returned instead of re-executing and re-injecting content.
pub fn duplicate_stub(_tool: &str, scope: &ReadScope, first_iter: u32, overlap: bool) -> String {
    if overlap {
        format!(
            "[duplicate read - {} already in your context (first read at iteration {}); overlapping range, no payload re-injected. Use your notes; do not re-read. If you need a different region of this file, say so explicitly and continue from where you stopped.]",
            scope.describe(),
            first_iter
        )
    } else {
        format!(
            "[duplicate read - `{}` with the same effective arguments was already executed at iteration {}; the result is already in your context, no payload re-injected. Use your notes; do not re-run it.]",
            scope.describe(),
            first_iter
        )
    }
}

/// Escalating steering text (behavioural, never a cap).
pub fn steering_text(
    level: u8,
    threshold: u32,
    streak: u32,
    duplicates: u32,
    state_changes: u32,
    iteration: u32,
) -> String {
    let facts = format!(
        "({} read-only calls since the last state change, {} duplicate reads blocked, {} state changes, iteration {})",
        streak, duplicates, state_changes, iteration
    );
    match level {
        0 | 1 => format!(
            "EFFICIENCY CHECK {}: you have made {} read-only calls since the last state change - you are looping. Act on what you already have or report the blocker. Batch the remaining discovery into one call and then make the edit/commit.",
            facts, threshold
        ),
        2 => format!(
            "STOP EXPLORING {}: the information you need is already in this context. Make the edit and commit it, or answer with what you know plus the remaining uncertainty. Do not read the same file again.",
            facts
        ),
        _ => format!(
            "HARD STOP ON DISCOVERY {}: further read-only calls will not improve the answer. Produce the deliverable now (edit + commit + push) or fail the task with an explicit blocker; if the task is simple, answer in prose now.",
            facts
        ),
    }
}

/// Fatal provider errors: retrying these cannot succeed and only burns balance.
/// Returns a short human-readable reason.
pub fn classify_fatal_provider_error(body: &str) -> Option<String> {
    let b = body.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| b.contains(n));
    if has(&[
        "insufficient balance",
        "insufficient_balance",
        "insufficient quota",
        "insufficient_quota",
        "402",
        "payment required",
        "no credit",
        "out of credits",
        "exceeded your current quota",
    ]) {
        return Some(
            "provider billing error (insufficient balance / 402): no retry can succeed; \
             top up the provider account and re-run"
                .to_string(),
        );
    }
    if has(&[
        "401",
        "unauthorized",
        "invalid api key",
        "invalid_api_key",
        "authentication",
        "no auth credentials",
    ]) {
        return Some(
            "provider authentication error (401/unauthorized): check the API key, \
             no retry can succeed"
                .to_string(),
        );
    }
    if has(&["403", "permission denied", "forbidden"]) && has(&["api", "model", "provider", "key"]) {
        return Some("provider permission error (403): the key may not use this model".to_string());
    }
    None
}

/// Classification of a whole agent-loop outcome for reporting.
pub fn efficiency_grade(duplicate_reads: u32, readonly_streak: u32, state_changes: u32) -> &'static str {
    if duplicate_reads == 0 && state_changes > 0 && readonly_streak < 6 {
        "efficient"
    } else if duplicate_reads <= 2 && readonly_streak < 12 {
        "acceptable"
    } else {
        "looping"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope(tool: &str, args: serde_json::Value) -> Option<ReadScope> {
        parse_read_scope(tool, &args.to_string())
    }

    #[test]
    fn overlapping_file_pages_are_duplicates() {
        let a = scope(
            "filesystem__read",
            json!({"path": "/x/docker-compose.yml", "offset": 262, "limit": 4000}),
        )
        .unwrap();
        let b = scope(
            "filesystem__read",
            json!({"path": "/x/docker-compose.yml", "offset": 300, "limit": 2000}),
        )
        .unwrap();
        assert!(pages_overlap(&a, &b));
        assert!(scopes_equivalent(&a, &b));
    }

    #[test]
    fn non_overlapping_windows_of_same_file_are_allowed() {
        let a = scope(
            "filesystem__read",
            json!({"path": "/f", "offset": 0, "limit": 100}),
        )
        .unwrap();
        let b = scope(
            "filesystem__read",
            json!({"path": "/f", "offset": 5000, "limit": 100}),
        )
        .unwrap();
        assert!(!pages_overlap(&a, &b));
        assert!(!scopes_equivalent(&a, &b));
    }

    #[test]
    fn open_ended_read_overlaps_later_page_of_same_file() {
        let a = scope("filesystem__read", json!({"path": "/f"})).unwrap();
        let b = scope(
            "filesystem__read",
            json!({"path": "/f", "offset": 900000}),
        )
        .unwrap();
        assert!(pages_overlap(&a, &b));
    }

    #[test]
    fn ledger_flags_the_thread_2874_pattern() {
        let mut led = EfficiencyLedger::new();
        // same file, six different offset/limit combinations (what happened live)
        let args = [
            json!({"path": "/x/docker-compose.yml", "offset": 262, "limit": 500}),
            json!({"path": "/x/docker-compose.yml", "offset": 262, "limit": 4000}),
            json!({"path": "/x/docker-compose.yml", "offset": 300, "limit": 2000}),
            json!({"path": "/x/docker-compose.yml", "offset": 0, "limit": 50000}),
        ];
        assert_eq!(
            led.observe("filesystem__read", &args[0].to_string(), 1),
            ReadVerdict::New
        );
        for (i, a) in args.iter().enumerate().skip(1) {
            match led.observe("filesystem__read", &a.to_string(), i as u32 + 1) {
                ReadVerdict::Duplicate { .. } => led.record_block(),
                other => panic!("expected duplicate at {} got {:?}", i, other),
            }
        }
        assert_eq!(led.duplicate_reads(), 3);
    }

    #[test]
    fn state_change_resets_the_readonly_streak() {
        let mut led = EfficiencyLedger::new();
        for i in 0..5 {
            led.observe(
                "filesystem__read",
                &json!({"path": format!("/f{}", i)}).to_string(),
                i,
            );
        }
        assert_eq!(led.readonly_streak(), 5);
        assert!(led
            .observe(
                "filesystem__write",
                &json!({"path": "/f0", "content": "x"}).to_string(),
                6
            )
            .eq(&ReadVerdict::NotGuarded));
        assert_eq!(led.readonly_streak(), 0);
        assert_eq!(led.state_changes(), 1);
        // the written file's read record is invalidated -> a re-read is allowed
        let v = led.observe(
            "filesystem__read",
            &json!({"path": "/f0", "offset": 0, "limit": 10}).to_string(),
            7,
        );
        assert_eq!(v, ReadVerdict::New);
    }

    #[test]
    fn git_commit_is_a_state_change_and_git_status_is_a_read() {
        let commit = json!({"repo_dir": "/r", "args": ["commit", "-m", "x"]});
        assert!(is_state_changing("git__run_command", &commit.to_string()));
        assert!(parse_read_scope("git__run_command", &commit.to_string()).is_none());
        let status = json!({"repo_dir": "/r", "args": ["status"]});
        assert!(!is_state_changing("git__run_command", &status.to_string()));
        assert!(parse_read_scope("git__run_command", &status.to_string()).is_some());
    }

    #[test]
    fn repeated_git_status_is_a_duplicate() {
        let mut led = EfficiencyLedger::new();
        let args = json!({"repo_dir": "/r", "args": ["status"]}).to_string();
        assert_eq!(led.observe("git__run_command", &args, 1), ReadVerdict::New);
        assert!(matches!(
            led.observe("git__run_command", &args, 2),
            ReadVerdict::Duplicate { .. }
        ));
    }

    #[test]
    fn grep_scope_is_content_aware_and_duplicate_guardable() {
        let mut led = EfficiencyLedger::new();
        let a = json!({"pattern": "repeat_guard", "path": "/r"}).to_string();
        let b = json!({"path": "/r", "pattern": "repeat_guard"}).to_string(); // reordered
        assert_eq!(led.observe("filesystem__grep", &a, 1), ReadVerdict::New);
        assert!(matches!(
            led.observe("filesystem__grep", &b, 2),
            ReadVerdict::Duplicate { .. }
        ));
        let c = json!({"pattern": "other", "path": "/r"}).to_string();
        assert_eq!(led.observe("filesystem__grep", &c, 3), ReadVerdict::New);
    }

    #[test]
    fn steering_escalates_and_never_caps() {
        let mut led = EfficiencyLedger::new();
        let mut texts = Vec::new();
        for i in 0..30 {
            led.observe("git__status", &json!({"repo_dir": format!("/r{}", i)}).to_string(), i);
            if let Some(t) = led.steering(i) {
                texts.push(t);
            }
        }
        assert!(texts[0].contains("EFFICIENCY CHECK"));
        assert!(texts.iter().any(|t| t.contains("STOP EXPLORING")));
        assert!(texts.iter().any(|t| t.contains("HARD STOP")));
        // never a cap: the ledger keeps accepting observations
        assert!(led.observe("git__status", &json!({"repo_dir": "/r99"}).to_string(), 99).eq(&ReadVerdict::New));
    }

    #[test]
    fn fatal_provider_errors_are_classified() {
        assert!(classify_fatal_provider_error(
            "HTTP 402 Payment Required: {\"error\":{\"message\":\"Insufficient Balance\"}}"
        )
        .is_some());
        assert!(classify_fatal_provider_error("Insufficient Balance").is_some());
        assert!(classify_fatal_provider_error("401 Unauthorized: invalid api key").is_some());
        assert!(classify_fatal_provider_error("429 rate limit exceeded, retry later").is_none());
        assert!(classify_fatal_provider_error("context_length_exceeded").is_none());
    }

    #[test]
    fn duplicate_stub_has_no_payload() {
        let s = scope(
            "filesystem__read",
            json!({"path": "/f", "offset": 0, "limit": 500}),
        )
        .unwrap();
        let stub = duplicate_stub("filesystem__read", &s, 7, true);
        assert!(stub.contains("no payload re-injected"));
        assert!(stub.contains("f"));
    }

    #[test]
    fn metrics_summary_is_a_single_line() {
        let mut led = EfficiencyLedger::new();
        led.observe("git__status", &json!({"repo_dir": "/r"}).to_string(), 1);
        led.record_block();
        led.note_state_change();
        led.mark_state_change_at(4);
        let m = led.metrics_summary(2874);
        assert!(m.contains("duplicate_reads=1"));
        assert!(m.contains("thread=2874"));
        assert!(!m.contains('\n'));
    }

    #[test]
    fn grade_flags_a_looping_thread() {
        assert_eq!(efficiency_grade(0, 2, 3), "efficient");
        assert_eq!(efficiency_grade(9, 30, 0), "looping");
    }
}
