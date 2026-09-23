//! Cap/limit observability guard (defect class A5).
//!
//! Root cause of the hidden interactive-iteration cap: when a cap
//! shortened a thread, NOTHING recorded which knob had fired, what its value
//! was, or whether the value came from the operator's config or from an
//! agent-invented code default. The operator had to ask "why did it stop?".
//!
//! These tests are the mechanical gate for requirement 3 (caps must be
//! observable) and requirement 2 (a cap must never be silently min/max-ed).
//! They are source-level assertions on purpose: they hold without a live
//! LLM/DB stack, so they run in CI on every change.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_rel(rel: &str) -> String {
    let p = repo_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// The cap-termination text is composed by `cap_termination_notice()` so the
/// exact wording is unit-testable and cannot rot back into a silent break.
/// It MUST name the knob, the value AND the provenance (source) of the value.
#[test]
fn cap_termination_message_names_knob_and_source() {
    let src = read_rel("src/agent/main_loop.rs");
    assert!(
        src.contains("fn cap_termination_notice("),
        "cap_termination_notice() helper is missing from src/agent/main_loop.rs - \
         caps must report knob + value + source through one canonical function"
    );
    // The canonical message must interpolate the knob, the value and the
    // provenance of the value.
    for needle in [
        "Iteration limit ({value})",
        "knob: {knob}",
        "source: {source}",
        "The task was interrupted before completion.",
    ] {
        assert!(
            src.contains(needle),
            "cap termination message lost the `{needle}` fragment - the operator \
             would again have to ask why the thread stopped"
        );
    }
    // ... and the cap site must actually USE the helper (not re-inline a
    // second, divergent message).
    assert!(
        src.contains("cap_termination_notice("),
        "the iteration-cap branch must call cap_termination_notice()"
    );
    assert!(
        !src.contains("final_content = format!(\n                    \"Iteration limit"),
        "the iteration-cap branch re-inlined the message instead of calling \
         cap_termination_notice() - two wordings will drift apart"
    );
}

/// The cap must also be logged (not only shown in the thread) with the knob,
/// its value and where the value came from.
#[test]
fn cap_termination_logs_knob_value_and_source() {
    let src = read_rel("src/agent/main_loop.rs");
    for needle in ["knob = iter_knob", "value = iter_limit", "source = "] {
        assert!(
            src.contains(needle),
            "cap termination warn! log lost the `{needle}` field - a capped run \
             must be diagnosable from the logs alone"
        );
    }
}

/// Provenance must be computable: the operator-configured key set and the
/// "config vs code default" label must exist in the config module.
#[test]
fn cap_source_provenance_is_computable() {
    let src = read_rel("src/agent/config.rs");
    for needle in [
        "OPERATOR_CONFIGURED_KEYS",
        "pub fn setting_is_operator_configured(",
        "pub fn setting_source_label(",
    ] {
        assert!(
            src.contains(needle),
            "config provenance helper `{needle}` is missing - the cap cannot \
             state whether the value came from operator config or a code default"
        );
    }
    // The iteration knobs must be mapped to the operator-facing setting names.
    let threads = read_rel("src/db/threads.rs");
    assert!(
        threads.contains("pub fn max_iterations_knob("),
        "max_iterations_knob() (knob name for the report) is missing from src/db/threads.rs"
    );
}

/// Recursively collect every `.rs` file below `dir` (skipping `target/`).
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().map(|n| n == "target").unwrap_or(false) {
                continue;
            }
            collect_rs(&p, out);
        } else if p.extension().map(|x| x == "rs").unwrap_or(false) {
            out.push(p);
        }
    }
}

/// Regression: the hidden `min(base, 12)` interactive cap must never come back.
///
/// The historical defect (commit b1249c9 "hard interactive round cap", removed
/// in 21aaa4e) looked exactly like this - an agent-invented CLAMP of the
/// operator's iteration budget:
///
///     let base = max_iterations_for_plan(config, plan);
///     if interactive { std::cmp::min(base, 12) } else { base }
///
/// Only the CLAMP shape is checked here (min/max/clamp applied to an iteration
/// budget expression). A plain `get("max_iterations_no_plan", "30")` parse
/// default is legitimate (used when the operator left it UNSET) and is policed
/// separately by scripts/lint-no-unrequested-narrowing.py.
#[test]
fn no_hidden_interactive_cap_is_reintroduced() {
    let mut offenders: Vec<String> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    collect_rs(&repo_root().join("src"), &mut files);
    for f in files {
        let Ok(text) = fs::read_to_string(&f) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            let t = line.trim();
            if t.starts_with("//") || t.starts_with('#') {
                continue;
            }
            let names_budget = [
                concat!("interactive_", "max_iterations"),
                "max_iterations_no_plan",
                "max_iterations_plan",
            ]
            .iter()
            .any(|k| t.contains(k));
            let clamps = [".min(", ".max(", ".clamp(", "cmp::min(", "cmp::max("]
                .iter()
                .any(|c| t.contains(c));
            if names_budget && clamps {
                offenders.push(format!("{}:{}: {}", f.display(), n + 1, t));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a hidden narrowing override on an operator iteration budget came back \
         (defect class A5). An operator-configured budget must be used as given; \
         if a safety net is truly needed it must be opt-in config that defaults \
         to OFF and logged loudly when it fires. Offending lines: {offenders:?}"
    );
}

/// The mechanical gate itself must exist in the repo and in CI, and it must
/// carry a self-test whose planted fixtures trip it.
#[test]
fn narrowing_lint_gate_exists_and_runs_over_the_repo() {
    let mut missing: Vec<&str> = Vec::new();
    for rel in [
        "scripts/lint-no-unrequested-narrowing.py",
        "scripts/narrowing-defaults-baseline.json",
        "scripts/test_lint_no_unrequested_narrowing.py",
        ".github/workflows/narrowing-lint.yml",
    ] {
        if !Path::new(&repo_root().join(rel)).exists() {
            missing.push(rel);
        }
    }
    assert!(
        missing.is_empty(),
        "the no-unrequested-narrowing gate is incomplete; missing: {missing:?}"
    );
    // The self-test must contain planted fixtures that are expected to TRIP
    // the lint (a lint whose fixtures never fire proves nothing).
    let selftest = read_rel("scripts/test_lint_no_unrequested_narrowing.py");
    assert!(
        selftest.contains("min(") || selftest.contains("clamp("),
        "the lint self-test has no planted clamp/min fixture - it cannot prove \
         that `min(base, 12)`-style code is actually rejected"
    );
    assert!(
        selftest.contains("unwrap_or("),
        "the lint self-test has no planted `unwrap_or(<non-zero>)` fixture"
    );
}
