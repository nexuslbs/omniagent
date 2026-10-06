//! Orchestration-task budgets: a HARD cumulative token cap plus an iteration
//! throttle for orchestrator threads (operator request, telegram thread 4183,
//! 2026-10-06).
//!
//! ## Why
//! An orchestrator thread (one that dispatches a dsh worker and then only
//! verifies it) burned 4.6M input + 72k output tokens over 45 iterations while
//! still processing: the loop had NO cumulative spend cap (the existing
//! `token_usage_budget` setting is TELEMETRY ONLY) and no minimum interval
//! between verification iterations, so the orchestrator polled in a tight
//! loop while the worker ran.
//!
//! ## What this module owns
//! The PURE resolution and arithmetic of the two orchestration knobs:
//!
//! * [`TOKEN_BUDGET_KNOB`] - CUMULATIVE provider-reported tokens (input +
//!   output) an orchestration thread may spend before the main loop stops
//!   cleanly with a cap notice. `0` disables the cap.
//! * [`ITERATION_MIN_INTERVAL_KNOB`] - minimum number of seconds between two
//!   orchestrator iterations while a dispatched worker is still running.
//!   `0` disables the throttle. The orchestrator may never poll faster.
//!
//! ## Tiers (highest wins)
//! `workflow_role > workflow > kanban_task > board > global setting`.
//! The channel/profile tiers do NOT carry these knobs (documented; the
//! workflow/board/task tiers are where an operator caps a task).
//!
//! ## Backwards compatibility (R5)
//! Neither knob touches an ordinary (non-orchestration) thread: the caller
//! applies the resolved budget only when [`is_orchestration_workflow`] (or an
//! explicit workflow/role/task declaration) opts the thread in, and the
//! per-iteration prompt budgets (`prompt_token_budget_hard` / `_soft`), their
//! compaction semantics and `max_iterations_*` are untouched.
//!
//! The knobs are deliberately SEPARATE from `token_usage_budget`: that one is a
//! telemetry reference for the in-prompt `=== Token Usage ===` block and never
//! stops anything. This module is the ENFORCEMENT side.

use std::time::Duration;

/// Global setting key: cumulative token budget for orchestration threads.
pub const TOKEN_BUDGET_KNOB: &str = "orchestration_token_budget";
/// Global setting key: minimum seconds between orchestrator iterations while a
/// dispatched worker runs.
pub const ITERATION_MIN_INTERVAL_KNOB: &str = "orchestration_iteration_min_interval_secs";
/// Global setting key: comma-separated workflow ids treated as orchestration
/// workflows.
pub const WORKFLOWS_KNOB: &str = "orchestration_workflows";

/// Default orchestration workflow ids (operator list, thread 4183):
/// `orchestrator`, `workstation`, `mvp`, `papers-orchestrator`.
pub const DEFAULT_ORCHESTRATION_WORKFLOWS: &str =
    "orchestrator,workstation,mvp,papers-orchestrator";

/// Which tier supplied a resolved value (provenance for the cap notice and
/// for the startup/log lines: defect class A5 - a cap must name its knob and
/// where the value came from).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnobTier {
    WorkflowRole,
    Workflow,
    Task,
    Board,
    GlobalSetting,
}

impl KnobTier {
    pub fn label(self) -> &'static str {
        match self {
            KnobTier::WorkflowRole => "workflow_role",
            KnobTier::Workflow => "workflow",
            KnobTier::Task => "task",
            KnobTier::Board => "board",
            KnobTier::GlobalSetting => "global_setting",
        }
    }
}

/// One resolved knob: the effective value plus a human-readable provenance
/// string naming the tier AND the file/field it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnobResolution {
    pub value: u64,
    pub tier: KnobTier,
    pub source: String,
}

impl KnobResolution {
    fn new(value: u64, tier: KnobTier, source: String) -> Self {
        Self {
            value,
            tier,
            source,
        }
    }
}

/// Raw tier candidates for both knobs (all `None` = nothing declared at that
/// tier). Gathered by the caller from `workflows.yml` (role/workflow),
/// `kanban_tasks` (task) and `boards.yml` (board).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KnobCandidates {
    pub role_token_budget: Option<u64>,
    pub role_iteration_min_interval_secs: Option<u64>,
    pub workflow_token_budget: Option<u64>,
    pub workflow_iteration_min_interval_secs: Option<u64>,
    pub task_token_budget: Option<u64>,
    pub task_iteration_min_interval_secs: Option<u64>,
    pub board_token_budget: Option<u64>,
    pub board_iteration_min_interval_secs: Option<u64>,
}

impl KnobCandidates {
    /// True when any WORKFLOW or WORKFLOW_ROLE tier explicitly declares one of
    /// the knobs. That is an explicit orchestration opt-in even when the
    /// workflow id is not in `orchestration_workflows`.
    pub fn workflow_tier_declares_any(&self) -> bool {
        self.role_token_budget.is_some()
            || self.role_iteration_min_interval_secs.is_some()
            || self.workflow_token_budget.is_some()
            || self.workflow_iteration_min_interval_secs.is_some()
    }

    /// True when the TASK tier explicitly declares one of the knobs (a
    /// per-task opt-in for a workflow that is not in the configured list).
    pub fn task_tier_declares_any(&self) -> bool {
        self.task_token_budget.is_some() || self.task_iteration_min_interval_secs.is_some()
    }
}

/// The fully resolved orchestration budget of ONE thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedOrchestrationBudget {
    /// True when this thread is an orchestration thread (the cap/throttle
    /// apply). False = every knob is inert (R5: nothing changes).
    pub orchestration: bool,
    pub token_budget: KnobResolution,
    pub iteration_min_interval: KnobResolution,
}

impl ResolvedOrchestrationBudget {
    /// A thread that is NOT an orchestration thread: both knobs are inert.
    pub fn not_orchestration(global_token_budget: u64, global_interval_secs: u64) -> Self {
        Self {
            orchestration: false,
            token_budget: KnobResolution::new(
                global_token_budget,
                KnobTier::GlobalSetting,
                global_setting_source(TOKEN_BUDGET_KNOB),
            ),
            iteration_min_interval: KnobResolution::new(
                global_interval_secs,
                KnobTier::GlobalSetting,
                global_setting_source(ITERATION_MIN_INTERVAL_KNOB),
            ),
        }
    }

    /// The HARD cumulative cap is active: orchestration thread AND a positive
    /// budget. `0` disables the cap.
    pub fn cap_enabled(&self) -> bool {
        self.orchestration && self.token_budget.value > 0
    }

    /// The iteration throttle is active: orchestration thread AND a positive
    /// minimum interval. `0` disables the throttle.
    pub fn throttle_enabled(&self) -> bool {
        self.orchestration && self.iteration_min_interval.value > 0
    }
}

/// `"<knob> (source: <operator config | code default>)"` - the provenance
/// label the core already uses for the iteration caps, so an operator can tell
/// a configured value from a code default.
fn global_setting_source(knob: &str) -> String {
    format!(
        "{knob} (source: {})",
        crate::agent::config::setting_source_label(knob)
    )
}

/// Parse the comma-separated `orchestration_workflows` setting into ids
/// (trimmed, empty entries dropped, lower-cased for a case-insensitive match).
pub fn parse_workflow_list(csv: &str) -> Vec<String> {
    csv.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// True when `workflow_id` is one of the configured orchestration workflows.
/// A missing/empty workflow id is never orchestration.
pub fn is_orchestration_workflow(workflow_id: Option<&str>, configured_csv: &str) -> bool {
    let Some(id) = workflow_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let id = id.to_ascii_lowercase();
    parse_workflow_list(configured_csv).contains(&id)
}

/// One tier candidate: `(tier, provenance, value)`.
type TierCandidate = (KnobTier, String, Option<u64>);

/// First non-`None` candidate wins (the candidate list is already ordered by
/// precedence); each candidate carries its OWN tier so the provenance names the
/// tier the value really came from.
fn first_some(candidates: &[TierCandidate]) -> Option<KnobResolution> {
    candidates
        .iter()
        .find_map(|(tier, source, value)| value.map(|v| KnobResolution::new(v, *tier, source.clone())))
}

/// Resolve both orchestration knobs for one thread.
///
/// `role_key` / `workflow_id` / `board` are OWNER LABELS used in the
/// provenance strings (never values). `global_*` are the global-setting
/// values (`0` = disabled).
///
/// Opt-in rule (R5): the thread is an orchestration thread when
/// 1. its workflow id is in `orchestration_workflows`, OR
/// 2. its WORKFLOW/ROLE tier explicitly declares one of the knobs, OR
/// 3. its TASK tier explicitly declares one of the knobs.
///
/// A board-level value alone never opts a task in (a board setting must not
/// silently start capping every task on that board).
#[allow(clippy::too_many_arguments)]
pub fn resolve_for_thread(
    workflow_id: Option<&str>,
    role_key: Option<&str>,
    board: Option<&str>,
    candidates: &KnobCandidates,
    configured_workflows_csv: &str,
    global_token_budget: u64,
    global_iteration_min_interval_secs: u64,
) -> ResolvedOrchestrationBudget {
    let orchestration = is_orchestration_workflow(workflow_id, configured_workflows_csv)
        || candidates.workflow_tier_declares_any()
        || candidates.task_tier_declares_any();
    if !orchestration {
        return ResolvedOrchestrationBudget::not_orchestration(
            global_token_budget,
            global_iteration_min_interval_secs,
        );
    }

    let wf = workflow_id.map(str::trim).filter(|s| !s.is_empty());
    let role = role_key.map(str::trim).filter(|s| !s.is_empty());
    let board = board.map(str::trim).filter(|s| !s.is_empty());

    let role_source = |knob: &str| match (wf, role) {
        (Some(w), Some(r)) => {
            format!("workflows.yml workflow '{w}' role '{r}' field {knob}")
        }
        (Some(w), None) => format!("workflows.yml workflow '{w}' role field {knob}"),
        (None, Some(r)) => format!("workflow role '{r}' field {knob}"),
        (None, None) => format!("workflow role field {knob}"),
    };
    let workflow_source =
        |knob: &str| format!("workflows.yml workflow '{}' field {knob}", wf.unwrap_or(""));
    let task_source = |knob: &str| format!("kanban_tasks.{knob} (task override)");
    let board_source =
        |knob: &str| format!("boards.yml board '{}' field {knob}", board.unwrap_or(""));

    let token_candidates: Vec<TierCandidate> = vec![
        (
            KnobTier::WorkflowRole,
            role_source("token_budget"),
            candidates.role_token_budget,
        ),
        (
            KnobTier::Workflow,
            workflow_source("token_budget"),
            candidates.workflow_token_budget,
        ),
        (
            KnobTier::Task,
            task_source("token_budget"),
            candidates.task_token_budget,
        ),
        (
            KnobTier::Board,
            board_source("token_budget"),
            candidates.board_token_budget,
        ),
    ];
    let interval_candidates: Vec<TierCandidate> = vec![
        (
            KnobTier::WorkflowRole,
            role_source("iteration_min_interval_secs"),
            candidates.role_iteration_min_interval_secs,
        ),
        (
            KnobTier::Workflow,
            workflow_source("iteration_min_interval_secs"),
            candidates.workflow_iteration_min_interval_secs,
        ),
        (
            KnobTier::Task,
            task_source("iteration_min_interval_secs"),
            candidates.task_iteration_min_interval_secs,
        ),
        (
            KnobTier::Board,
            board_source("iteration_min_interval_secs"),
            candidates.board_iteration_min_interval_secs,
        ),
    ];

    let token_budget = first_some(&token_candidates).unwrap_or(KnobResolution::new(
        global_token_budget,
        KnobTier::GlobalSetting,
        global_setting_source(TOKEN_BUDGET_KNOB),
    ));
    let iteration_min_interval = first_some(&interval_candidates).unwrap_or(KnobResolution::new(
        global_iteration_min_interval_secs,
        KnobTier::GlobalSetting,
        global_setting_source(ITERATION_MIN_INTERVAL_KNOB),
    ));

    ResolvedOrchestrationBudget {
        orchestration: true,
        token_budget,
        iteration_min_interval,
    }
}

/// Seconds to wait BEFORE the next orchestrator iteration.
///
/// `0` when the throttle is disabled, when no dispatched worker is in flight
/// (the orchestrator is doing real work and must not be slowed down), or when
/// the minimum interval has already elapsed. Otherwise the remaining part of
/// the interval.
pub fn throttle_wait_secs(
    min_interval_secs: u64,
    elapsed_since_last_iteration_secs: u64,
    worker_in_flight: bool,
) -> u64 {
    if min_interval_secs == 0 || !worker_in_flight {
        return 0;
    }
    min_interval_secs.saturating_sub(elapsed_since_last_iteration_secs)
}

/// Convenience wrapper over [`throttle_wait_secs`] taking a real elapsed
/// [`Duration`].
pub fn throttle_wait(
    min_interval_secs: u64,
    elapsed: Duration,
    worker_in_flight: bool,
) -> Duration {
    Duration::from_secs(throttle_wait_secs(
        min_interval_secs,
        elapsed.as_secs(),
        worker_in_flight,
    ))
}

/// True when a tool result is a BACKGROUND-TASK HANDLE (`status=processing` +
/// a `task_id`): the dispatched worker is still running, so the orchestrator is
/// in its verification/wait phase and the throttle applies.
///
/// Tolerant by design: the canonical envelope is JSON
/// (`{"status":"processing","task_id":"..."}`), but a result may carry the
/// envelope embedded in surrounding text, so after the JSON parse fails a
/// containment check is used.
pub fn output_is_background_handle(output: &str) -> bool {
    let trimmed = output.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if value.get("status").and_then(|s| s.as_str()) == Some("processing")
            && value.get("task_id").is_some()
        {
            return true;
        }
    }
    trimmed.contains("\"processing\"") && trimmed.contains("task_id")
}

/// The cap-termination notice for the cumulative token budget (mirrors
/// [`crate::agent::main_loop::cap_termination_notice`]: the knob, the effective
/// value and its provenance are always named - a cap must never end a thread
/// unobservably).
pub fn cap_notice(token_budget: u64, cumulative_tokens: u64, source: &str) -> String {
    format!(
        "Cumulative token budget ({token_budget}) exceeded: the thread used \
         {cumulative_tokens} tokens. knob: {TOKEN_BUDGET_KNOB} (source: {source}). \
         The task was interrupted before completion."
    )
}

/// Observability line for a throttle wait (logged by the main loop).
pub fn throttle_notice(min_interval_secs: u64, waited_secs: u64, source: &str) -> String {
    format!(
        "Orchestration iteration throttle: waiting {waited_secs}s before the next \
         iteration while the dispatched worker is still running (minimum interval \
         {min_interval_secs}s, knob: {ITERATION_MIN_INTERVAL_KNOB}, source: {source})."
    )
}

/// Resolved orchestration limits for ONE thread, with the provenance labels
/// used by the logs and the cap notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadOrchestrationLimits {
    pub budget: ResolvedOrchestrationBudget,
    pub workflow_id: Option<String>,
    pub role_key: Option<String>,
    pub board: Option<String>,
}

/// The TASK-tier raw candidates of a kanban task (NULL = declares nothing).
#[derive(Debug, Clone, Default)]
struct TaskKnobRow {
    workflow_id: Option<String>,
    board: Option<String>,
    token_budget: Option<i64>,
    iteration_min_interval_secs: Option<i64>,
}

/// Load the raw TASK-tier row of a kanban task. A missing task id (plain
/// chat/channel thread) or a lookup error yields `None`: no task tier.
///
/// The columns are added by `db-migrations` (`ALTER TABLE kanban_tasks ADD
/// COLUMN IF NOT EXISTS ...`) and are run at every startup, so a failure here
/// can only mean the row vanished; the tier is then simply absent.
async fn load_task_knob_row(pool: &sqlx::PgPool, task_id: Option<&str>) -> Option<TaskKnobRow> {
    let id = task_id.map(str::trim).filter(|s| !s.is_empty())?;
    sqlx::query_as::<_, (Option<String>, Option<String>, Option<i64>, Option<i64>)>(
        "SELECT workflow_id, board, token_budget, iteration_min_interval_secs \
         FROM kanban_tasks WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .map(
        |(workflow_id, board, token_budget, iteration_min_interval_secs)| TaskKnobRow {
            workflow_id,
            board,
            token_budget,
            iteration_min_interval_secs,
        },
    )
}

/// A DB `BIGINT` candidate as a knob value: a negative value is invalid and
/// counts as UNSET (never as a zero budget, which would mean "disabled").
fn candidate_from_db(value: Option<i64>) -> Option<u64> {
    value.filter(|v| *v >= 0).map(|v| v as u64)
}

/// Load and resolve the orchestration budget of ONE thread, ONCE, at thread
/// start (`crate::agent::main_loop::run_main_loop`).
///
/// Sources, in resolution order:
/// - `workflow_role` / `workflow`: `config/workflows.yml` (the role comes from
///   `threads.workflow_step`, the workflow from the kanban task row);
/// - `kanban_task`: `kanban_tasks.token_budget` /
///   `.iteration_min_interval_secs`;
/// - `board`: `config/boards.yml` (the task's board);
/// - `global setting`: the resolved [`crate::agent::config::AgentConfig`]
///   snapshot.
///
/// Nothing is shallow-read later: the caller uses the returned resolved struct
/// for every decision, and `orchestration == false` (a non-orchestration
/// thread) leaves both knobs inert.
pub async fn load_for_thread(
    pool: &sqlx::PgPool,
    data_dir: &str,
    task_id: Option<&str>,
    workflow_step: Option<&str>,
    cfg: &crate::agent::config::AgentConfig,
) -> ThreadOrchestrationLimits {
    let role_key = workflow_step.and_then(crate::workflows::role_for_step);
    let task_row = load_task_knob_row(pool, task_id).await;
    let workflow_id = task_row.as_ref().and_then(|r| r.workflow_id.clone());
    let board = task_row.as_ref().and_then(|r| r.board.clone());

    // WORKFLOW_ROLE + WORKFLOW tiers, read separately so the provenance names
    // the tier the value really came from (role > workflow).
    let (role_token, role_interval, wf_token, wf_interval) = workflow_id
        .as_deref()
        .and_then(|id| crate::workflows::WorkflowsFile::load_workflow(data_dir, id).ok().flatten())
        .map(|wf| {
            let role_def = role_key.and_then(|r| wf.roles.get(r));
            (
                role_def.and_then(|r| r.overrides.token_budget),
                role_def.and_then(|r| r.overrides.iteration_min_interval_secs),
                wf.defaults.token_budget,
                wf.defaults.iteration_min_interval_secs,
            )
        })
        .unwrap_or((None, None, None, None));

    // BOARD tier: the task's board in config/boards.yml. A board failure
    // contributes nothing (the board gate itself is enforced by thread
    // creation; this resolver must never fail a running thread).
    let (board_token, board_interval) = board
        .as_deref()
        .and_then(|b| crate::boards::task_board(data_dir, Some(b)).ok().flatten())
        .map(|b| (b.token_budget, b.iteration_min_interval_secs))
        .unwrap_or((None, None));

    let candidates = KnobCandidates {
        role_token_budget: role_token,
        role_iteration_min_interval_secs: role_interval,
        workflow_token_budget: wf_token,
        workflow_iteration_min_interval_secs: wf_interval,
        task_token_budget: candidate_from_db(
            task_row.as_ref().and_then(|r| r.token_budget),
        ),
        task_iteration_min_interval_secs: candidate_from_db(
            task_row.as_ref().and_then(|r| r.iteration_min_interval_secs),
        ),
        board_token_budget: board_token,
        board_iteration_min_interval_secs: board_interval,
    };

    let budget = resolve_for_thread(
        workflow_id.as_deref(),
        role_key,
        board.as_deref(),
        &candidates,
        &cfg.orchestration_workflows,
        cfg.orchestration_token_budget,
        cfg.orchestration_iteration_min_interval_secs,
    );

    ThreadOrchestrationLimits {
        budget,
        workflow_id,
        role_key: role_key.map(str::to_string),
        board,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> KnobCandidates {
        KnobCandidates::default()
    }

    // ── Resolution precedence (R2) ──────────────────────────────────────────

    /// The chain is workflow_role > workflow > task > board > global setting,
    /// per knob (the two knobs resolve independently).
    #[test]
    fn tier_precedence_role_beats_workflow_beats_task_beats_board_beats_global() {
        let c = KnobCandidates {
            role_token_budget: Some(1_000_000),
            role_iteration_min_interval_secs: None,
            workflow_token_budget: Some(2_000_000),
            workflow_iteration_min_interval_secs: Some(1800),
            task_token_budget: Some(1_500_000),
            task_iteration_min_interval_secs: None,
            board_token_budget: Some(900_000),
            board_iteration_min_interval_secs: None,
        };
        let r = resolve_for_thread(
            Some("mvp"),
            Some("executor"),
            Some("omnidev"),
            &c,
            DEFAULT_ORCHESTRATION_WORKFLOWS,
            3_000_000,
            3600,
        );
        assert!(r.orchestration);
        // role wins for the token budget
        assert_eq!(r.token_budget.value, 1_000_000);
        assert_eq!(r.token_budget.tier, KnobTier::WorkflowRole);
        assert!(r.token_budget.source.contains("role 'executor'"));
        // workflow wins for the interval (the role left it unset)
        assert_eq!(r.iteration_min_interval.value, 1800);
        assert_eq!(r.iteration_min_interval.tier, KnobTier::Workflow);
        assert!(r.iteration_min_interval.source.contains("workflow 'mvp'"));

        // Drop the two upper tiers -> task, then board, then global.
        let c2 = KnobCandidates {
            task_token_budget: Some(1_500_000),
            board_token_budget: Some(900_000),
            ..candidates()
        };
        let r2 = resolve_for_thread(
            Some("mvp"),
            Some("executor"),
            Some("omnidev"),
            &c2,
            DEFAULT_ORCHESTRATION_WORKFLOWS,
            3_000_000,
            3600,
        );
        assert_eq!(r2.token_budget.value, 1_500_000);
        assert_eq!(r2.token_budget.tier, KnobTier::Task);
        assert!(r2.token_budget.source.contains("kanban_tasks.token_budget"));

        let c3 = KnobCandidates {
            board_token_budget: Some(900_000),
            ..candidates()
        };
        let r3 = resolve_for_thread(
            Some("mvp"),
            None,
            Some("omnidev"),
            &c3,
            DEFAULT_ORCHESTRATION_WORKFLOWS,
            3_000_000,
            3600,
        );
        assert_eq!(r3.token_budget.value, 900_000);
        assert_eq!(r3.token_budget.tier, KnobTier::Board);
    }

    /// Nothing declared -> the global settings apply and are named as such,
    /// including whether they are operator-configured or code defaults.
    #[test]
    fn nothing_declared_falls_back_to_the_global_settings() {
        let r = resolve_for_thread(
            Some("workstation"),
            Some("executor"),
            Some("main"),
            &candidates(),
            DEFAULT_ORCHESTRATION_WORKFLOWS,
            3_000_000,
            3600,
        );
        assert!(r.orchestration);
        assert_eq!(r.token_budget.value, 3_000_000);
        assert_eq!(r.token_budget.tier, KnobTier::GlobalSetting);
        assert!(r.token_budget.source.contains(TOKEN_BUDGET_KNOB));
        assert_eq!(r.iteration_min_interval.value, 3600);
        assert!(r.iteration_min_interval.source.contains(ITERATION_MIN_INTERVAL_KNOB));
    }

    /// A SIMPLE orchestration task: the TASK tier caps it at 1M (R2).
    #[test]
    fn simple_task_capped_at_one_million_via_the_task_tier() {
        let c = KnobCandidates {
            task_token_budget: Some(1_000_000),
            ..candidates()
        };
        let r = resolve_for_thread(
            Some("mvp"),
            Some("executor"),
            Some("main"),
            &c,
            DEFAULT_ORCHESTRATION_WORKFLOWS,
            3_000_000,
            3600,
        );
        assert_eq!(r.token_budget.value, 1_000_000);
        assert!(r.cap_enabled());
    }

    /// `0` DISABLES each knob (never "budget of zero").
    #[test]
    fn zero_disables_the_cap_and_the_throttle() {
        let c = KnobCandidates {
            role_token_budget: Some(0),
            role_iteration_min_interval_secs: Some(0),
            ..candidates()
        };
        let r = resolve_for_thread(
            Some("mvp"),
            Some("executor"),
            None,
            &c,
            DEFAULT_ORCHESTRATION_WORKFLOWS,
            3_000_000,
            3600,
        );
        assert!(r.orchestration);
        assert_eq!(r.token_budget.value, 0);
        assert!(!r.cap_enabled(), "0 disables the cap");
        assert_eq!(r.iteration_min_interval.value, 0);
        assert!(!r.throttle_enabled(), "0 disables the throttle");
    }

    // ── Opt-in / backwards compatibility (R5) ───────────────────────────────

    /// A workflow id in the configured list opts the thread in.
    #[test]
    fn configured_workflow_ids_are_orchestration() {
        for id in ["orchestrator", "workstation", "mvp", "papers-orchestrator", " WORKSTATION "] {
            assert!(
                is_orchestration_workflow(Some(id), DEFAULT_ORCHESTRATION_WORKFLOWS),
                "{id} must be an orchestration workflow"
            );
        }
        for id in ["dev-executor", "omniagent-dev", "research", ""] {
            assert!(!is_orchestration_workflow(Some(id), DEFAULT_ORCHESTRATION_WORKFLOWS));
        }
        assert!(!is_orchestration_workflow(None, DEFAULT_ORCHESTRATION_WORKFLOWS));
        // The list is configurable.
        assert!(is_orchestration_workflow(Some("my-wf"), "my-wf,other"));
    }

    /// A plain (non-orchestration) thread with nothing declared is completely
    /// unaffected: both knobs inert.
    #[test]
    fn non_orchestration_thread_is_unaffected() {
        let r = resolve_for_thread(
            Some("dev-executor"),
            Some("executor"),
            Some("omnidev"),
            &candidates(),
            DEFAULT_ORCHESTRATION_WORKFLOWS,
            3_000_000,
            3600,
        );
        assert!(!r.orchestration);
        assert!(!r.cap_enabled());
        assert!(!r.throttle_enabled());
        // Global values are still reported (informational), the caller applies
        // nothing because orchestration == false.
        assert_eq!(r.token_budget.value, 3_000_000);
    }

    /// A workflow that is NOT in the list but EXPLICITLY declares a knob opts
    /// in; a board-level value alone never opts in.
    #[test]
    fn explicit_workflow_role_or_task_declaration_opts_in_but_board_alone_does_not() {
        let wf_declared = KnobCandidates {
            workflow_token_budget: Some(2_000_000),
            ..candidates()
        };
        assert!(
            resolve_for_thread(
                Some("research"),
                None,
                None,
                &wf_declared,
                DEFAULT_ORCHESTRATION_WORKFLOWS,
                3_000_000,
                3600
            )
            .orchestration
        );
        let task_declared = KnobCandidates {
            task_iteration_min_interval_secs: Some(600),
            ..candidates()
        };
        assert!(
            resolve_for_thread(
                Some("research"),
                None,
                None,
                &task_declared,
                DEFAULT_ORCHESTRATION_WORKFLOWS,
                3_000_000,
                3600
            )
            .orchestration
        );
        let board_only = KnobCandidates {
            board_token_budget: Some(1_000_000),
            ..candidates()
        };
        assert!(
            !resolve_for_thread(
                Some("research"),
                None,
                Some("main"),
                &board_only,
                DEFAULT_ORCHESTRATION_WORKFLOWS,
                3_000_000,
                3600
            )
            .orchestration,
            "a board value alone must not silently cap every task on the board"
        );
    }

    // ── Throttle arithmetic (R3) ────────────────────────────────────────────

    #[test]
    fn throttle_waits_the_remaining_interval_only_while_a_worker_runs() {
        // Worker in flight, 10s since the last iteration -> 3590s to wait.
        assert_eq!(throttle_wait_secs(3600, 10, true), 3590);
        // Interval already elapsed -> no wait.
        assert_eq!(throttle_wait_secs(3600, 3600, true), 0);
        assert_eq!(throttle_wait_secs(3600, 9999, true), 0);
        // No worker in flight -> the orchestrator is doing real work, no wait.
        assert_eq!(throttle_wait_secs(3600, 0, false), 0);
        // 0 disables the throttle.
        assert_eq!(throttle_wait_secs(0, 0, true), 0);
        assert_eq!(
            throttle_wait(60, Duration::from_secs(15), true),
            Duration::from_secs(45)
        );
        assert_eq!(
            throttle_wait(60, Duration::from_secs(15), false),
            Duration::ZERO
        );
    }

    #[test]
    fn background_handle_detection_accepts_the_canonical_and_embedded_envelopes() {
        assert!(output_is_background_handle(
            r#"{"status":"processing","task_id":"task_1_2"}"#
        ));
        assert!(output_is_background_handle(
            "started\n{\"status\":\"processing\",\"task_id\":\"t\"}\n"
        ));
        assert!(!output_is_background_handle(r#"{"status":"completed","task_id":"t"}"#));
        assert!(!output_is_background_handle(r#"{"ok":true}"#));
        assert!(!output_is_background_handle("plain text"));
        assert!(!output_is_background_handle(""));
    }

    // ── Cap notice (R1) ─────────────────────────────────────────────────────

    #[test]
    fn cap_notice_names_the_knob_the_value_and_the_source() {
        let msg = cap_notice(1_000_000, 1_000_001, "kanban_tasks.token_budget (task override)");
        assert!(msg.contains("1000000"), "{msg}");
        assert!(msg.contains("1000001"), "{msg}");
        assert!(msg.contains(TOKEN_BUDGET_KNOB), "{msg}");
        assert!(msg.contains("kanban_tasks.token_budget"), "{msg}");
        assert!(msg.contains("interrupted"), "{msg}");
    }

    #[test]
    fn throttle_notice_names_the_interval_and_source() {
        let msg = throttle_notice(3600, 3590, "global_setting");
        assert!(msg.contains("3590"), "{msg}");
        assert!(msg.contains("3600"), "{msg}");
        assert!(msg.contains(ITERATION_MIN_INTERVAL_KNOB), "{msg}");
    }

    #[test]
    fn db_candidates_reject_negative_values() {
        assert_eq!(candidate_from_db(Some(1_000_000)), Some(1_000_000));
        assert_eq!(candidate_from_db(Some(0)), Some(0));
        assert_eq!(candidate_from_db(Some(-5)), None);
        assert_eq!(candidate_from_db(None), None);
    }

    #[test]
    fn workflow_list_parsing_ignores_blank_entries() {
        assert_eq!(
            parse_workflow_list(" a ,, B , "),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(parse_workflow_list("").is_empty());
    }
}
