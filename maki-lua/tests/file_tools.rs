//! Runs every file tool's handler for real against a tempdir. Covers the
//! whole call path, which restore-only and spec tests never touch — a broken
//! require in display code used to fail exactly there.

use std::sync::Arc;

use maki_agent::AgentMode;
use maki_agent::agent::tool_dispatch;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{CallOrigin, ToolRegistry};
use maki_lua::PluginHost;
use serde_json::{Value, json};
use test_case::test_case;

fn host() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_builtins(&maki_config::PluginsConfig::from_plugins(Default::default()))
        .unwrap();
    (reg, host)
}

fn run(reg: &Arc<ToolRegistry>, tool: &str, input: Value) -> String {
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.registry = Arc::clone(reg);
    let done = smol::block_on(tool_dispatch::run(
        "t1".into(),
        tool,
        &input,
        &ctx,
        CallOrigin::Model,
    ));
    assert!(!done.is_error, "{tool}: {}", done.output.as_text());
    done.output.as_text()
}

#[test_case("write" ; "write")]
#[test_case("edit" ; "edit")]
#[test_case("multiedit" ; "multiedit")]
#[test_case("read" ; "read")]
#[test_case("glob" ; "glob")]
#[test_case("grep" ; "grep")]
#[test_case("list" ; "list")]
#[test_case("index" ; "index")]
fn file_tool_handler_runs(tool: &str) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("demo.rs");
    std::fs::write(&file, "fn main() { let a = 1; }\n").unwrap();
    let file = file.to_string_lossy().into_owned();
    let dir_str = dir.path().to_string_lossy().into_owned();

    let (reg, _host) = host();
    let input = match tool {
        "write" => json!({ "path": file, "content": "fn main() { let a = 1; }\n" }),
        "edit" => json!({ "path": file, "old_string": "a = 1", "new_string": "a = 2" }),
        "multiedit" => {
            json!({ "path": file, "edits": [{ "old_string": "a = 1", "new_string": "a = 2" }] })
        }
        "read" => json!({ "path": file, "offset": 1, "limit": 10 }),
        "glob" => json!({ "pattern": "*.rs", "path": dir_str }),
        "grep" => json!({ "pattern": "main", "path": dir_str }),
        "list" => json!({ "path": dir_str }),
        "index" => json!({ "path": file }),
        _ => unreachable!(),
    };
    let out = run(&reg, tool, input);
    assert!(!out.is_empty(), "{tool}: empty output");
}
