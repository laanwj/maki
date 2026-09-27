//! The plan tools against a real host: the plan path comes from the session,
//! not the mode, so a session implementing its plan still reads and amends
//! it in build mode. Writes carry `written_path` so the plan-ready flow fires.

use std::sync::Arc;

use maki_agent::agent::tool_dispatch;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{CallOrigin, ToolRegistry};
use maki_agent::{AgentMode, ToolDoneEvent};
use maki_lua::PluginHost;
use serde_json::{Value, json};

const PLAN_SRC: &str = include_str!("../../plugins/plan/init.lua");

fn host_with_plan() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("plan", PLAN_SRC).unwrap();
    (reg, host)
}

fn run(reg: &Arc<ToolRegistry>, mode: &AgentMode, tool: &str, input: Value) -> ToolDoneEvent {
    let mut ctx = stub_ctx(mode);
    ctx.registry = Arc::clone(reg);
    smol::block_on(tool_dispatch::run(
        "t1".into(),
        tool,
        &input,
        &ctx,
        CallOrigin::Model,
    ))
}

/// Build mode, but the session has a plan file: the state a session is in
/// while it implements an approved plan.
fn run_in_build_with_plan(
    reg: &Arc<ToolRegistry>,
    plan_path: &str,
    tool: &str,
    input: Value,
) -> ToolDoneEvent {
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.plan_path = Some(plan_path.into());
    ctx.registry = Arc::clone(reg);
    smol::block_on(tool_dispatch::run(
        "t1".into(),
        tool,
        &input,
        &ctx,
        CallOrigin::Model,
    ))
}

fn plan_mode(dir: &tempfile::TempDir) -> (AgentMode, String) {
    let path = dir.path().join("plans/related-tolerant-gar.md");
    let path = path.to_string_lossy().into_owned();
    (AgentMode::Plan((&path).into()), path)
}

#[test]
fn plan_write_writes_the_session_plan_file() {
    let dir = tempfile::tempdir().unwrap();
    let (mode, path) = plan_mode(&dir);
    let (reg, _host) = host_with_plan();

    let done = run(&reg, &mode, "plan_write", json!({ "content": "# Plan\n" }));
    assert!(!done.is_error, "got: {}", done.output.as_text());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Plan\n");
    assert_eq!(
        done.written_path.as_deref(),
        Some(path.as_str()),
        "the plan-ready flow keys on written_path"
    );
}

/// The mode gates the file tools, not the plan file.
#[test]
fn plan_tools_work_in_build_mode_when_a_plan_exists() {
    let dir = tempfile::tempdir().unwrap();
    let (_mode, path) = plan_mode(&dir);
    let (reg, _host) = host_with_plan();

    let done = run_in_build_with_plan(
        &reg,
        &path,
        "plan_write",
        json!({ "content": "alpha beta" }),
    );
    assert!(!done.is_error, "got: {}", done.output.as_text());
    assert_eq!(done.written_path.as_deref(), Some(path.as_str()));

    let done = run_in_build_with_plan(&reg, &path, "plan_read", json!({}));
    assert_eq!(done.output.as_text(), "alpha beta");
    assert!(!done.is_error);

    let done = run_in_build_with_plan(
        &reg,
        &path,
        "plan_edit",
        json!({ "old_string": "beta", "new_string": "gamma" }),
    );
    assert!(!done.is_error, "got: {}", done.output.as_text());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha gamma");
}

#[test]
fn plan_tools_error_without_a_plan_file() {
    let (reg, _host) = host_with_plan();
    for (tool, input) in [
        ("plan_read", json!({})),
        ("plan_write", json!({ "content": "x" })),
        ("plan_edit", json!({ "old_string": "a", "new_string": "b" })),
    ] {
        let done = run(&reg, &AgentMode::Build, tool, input);
        assert!(
            done.is_error,
            "{tool} must fail when the session has no plan"
        );
        assert!(
            done.output.as_text().contains("this session has none"),
            "{tool}: {}",
            done.output.as_text()
        );
    }
}

#[test]
fn plan_read_reports_missing_then_content() {
    let dir = tempfile::tempdir().unwrap();
    let (mode, _path) = plan_mode(&dir);
    let (reg, _host) = host_with_plan();

    let done = run(&reg, &mode, "plan_read", json!({}));
    assert_eq!(done.output.as_text(), "(no plan written yet)");
    assert!(!done.is_error);

    run(&reg, &mode, "plan_write", json!({ "content": "the plan" }));
    let done = run(&reg, &mode, "plan_read", json!({}));
    assert_eq!(done.output.as_text(), "the plan");
    assert!(!done.is_error);
}

#[test]
fn plan_edit_rewrites_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let (mode, path) = plan_mode(&dir);
    let (reg, _host) = host_with_plan();

    run(
        &reg,
        &mode,
        "plan_write",
        json!({ "content": "alpha beta" }),
    );
    let done = run(
        &reg,
        &mode,
        "plan_edit",
        json!({ "old_string": "beta", "new_string": "gamma" }),
    );
    assert!(!done.is_error, "got: {}", done.output.as_text());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha gamma");
    assert_eq!(done.written_path.as_deref(), Some(path.as_str()));
}

#[test]
fn plan_edit_without_a_plan_points_at_plan_write() {
    let dir = tempfile::tempdir().unwrap();
    let (mode, _path) = plan_mode(&dir);
    let (reg, _host) = host_with_plan();

    let done = run(
        &reg,
        &mode,
        "plan_edit",
        json!({ "old_string": "a", "new_string": "b" }),
    );
    assert!(done.is_error);
    assert!(
        done.output.as_text().contains("plan_write"),
        "got: {}",
        done.output.as_text()
    );
}
