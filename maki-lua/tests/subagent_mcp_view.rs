//! A subagent session's MCP tool calls must ride the same tool-view channel
//! the main chat's do: the view's `start` publishes the body buf on the
//! subagent's own event stream, the relay forwards it to the parent, and the
//! subagent chat renders the body from it. A buf that never arrives leaves the
//! row falling back to truncated llm_output.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use maki_agent::mcp::error::McpError;
use maki_agent::mcp::test_support::session_with_transport;
use maki_agent::mcp::transport::{BoxFuture as McpFuture, McpTransport};
use maki_agent::tools::ToolRegistry;
use maki_agent::tools::test_support::stub_ctx_with;
use maki_agent::{AgentEvent, AgentMode, Envelope, EventSender};
use maki_lua::{Interaction, PluginHost};
use maki_providers::provider::{BoxFuture, Provider};
use maki_providers::{
    AgentError, ContentBlock, Message, Model, ModelInfo, ProviderEvent, RequestOptions, Role,
    StopReason, StreamResponse, TokenUsage,
};
use maki_storage::id::SessionRef;
use serde_json::{Value, json};

const PLUGIN: &str = "subagent_mcp_view";
const TASK_TOOL: &str = "sv_task";
const MCP_TOOL: &str = "svpy";
const TASK_CALL_ID: &str = "sv_task_call_1";
const MCP_CALL_ID: &str = "sv_mcp_call_1";
const RUN_ID: u64 = 7;
const HEADER_SUMMARY: &str = "3 lines of svpy";
const EXECUTOR_OUTPUT: &str = "executor output line";

/// The executor half of the split: answers `tools/call` for the MCP tool.
struct FakeExecutor;

impl McpTransport for FakeExecutor {
    fn send_request<'a>(
        &'a self,
        method: &'a str,
        _params: Option<Value>,
    ) -> McpFuture<'a, Result<Value, McpError>> {
        let result = match method {
            "tools/call" => json!({
                "content": [{ "type": "text", "text": EXECUTOR_OUTPUT }]
            }),
            _ => json!({}),
        };
        Box::pin(async move { Ok(result) })
    }

    fn send_notification<'a>(
        &'a self,
        _method: &'a str,
        _params: Option<Value>,
    ) -> McpFuture<'a, Result<(), McpError>> {
        Box::pin(async { Ok(()) })
    }

    fn shutdown<'a>(&'a self) -> McpFuture<'a, ()> {
        Box::pin(async {})
    }

    fn server_name(&self) -> &Arc<str> {
        static NAME: std::sync::OnceLock<Arc<str>> = std::sync::OnceLock::new();
        NAME.get_or_init(|| Arc::from("executor"))
    }

    fn transport_kind(&self) -> &'static str {
        "fake"
    }
}

/// Answers the first request with the MCP tool call, every later one with
/// plain text, so the subagent runs exactly one tool.
struct CallToolOnce {
    asked: AtomicBool,
}

impl Provider for CallToolOnce {
    fn stream_message<'a>(
        &'a self,
        _model: &'a Model,
        _messages: &'a [Message],
        _system: &'a str,
        _tools: &'a Value,
        _events: &'a flume::Sender<ProviderEvent>,
        _opts: RequestOptions,
        _session: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        let (block, stop) = if self.asked.swap(true, Ordering::SeqCst) {
            (
                ContentBlock::Text {
                    text: "subagent done".into(),
                },
                StopReason::EndTurn,
            )
        } else {
            (
                ContentBlock::tool_use(
                    MCP_CALL_ID,
                    MCP_TOOL,
                    json!({ "code": "print('hi')\nprint('again')" }),
                ),
                StopReason::ToolUse,
            )
        };
        Box::pin(async move {
            Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![block],
                    ..Default::default()
                },
                usage: TokenUsage::default(),
                stop_reason: Some(stop),
                input_transformations: Vec::new(),
            })
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

const PLUGIN_SRC: &str = r#"
maki.api.register_tool_view({
  tool = "svpy",
  start = function(input, ctx)
    local buf = maki.ui.buf()
    buf:set_lines({ "CODE " .. (input.code or "?") })
    ctx:live_buf(buf)
    return { buf = buf }, "3 lines of svpy"
  end,
  done = function(state, input, output)
    state.buf:line({ { "done", "dim" } })
  end,
})

maki.api.register_tool({
  name = "sv_task",
  description = "spawn a subagent that calls the MCP tool once",
  schema = { type = "object", properties = {}, additionalProperties = false },
  handler = function(input, ctx)
    local sess, err = maki.agent.session(ctx, {
      tools = {
        {
          name = "svpy",
          description = "run a script",
          input_schema = {
            type = "object",
            properties = { code = { type = "string" } },
            required = { "code" },
          },
        },
      },
      mcp = true,
    })
    if err then
      return { llm_output = err, is_error = true }
    end
    local r, perr = sess:prompt("go")
    sess:close()
    if perr then
      return { llm_output = perr, is_error = true }
    end
    return r.text
  end,
})
"#;

fn event_name(envelope: &Envelope) -> String {
    serde_json::to_value(&envelope.event).unwrap()["type"]
        .as_str()
        .unwrap_or("?")
        .to_owned()
}

#[test]
fn subagent_mcp_view_body_reaches_the_parent_stream() {
    let reg = Arc::clone(ToolRegistry::global_arc());
    // SingleProcess role: a Brain host only lets bundled plugins register
    // tools, and the event chain under test (subagent dispatch -> view start
    // -> live buf on the subagent stream -> relay to the parent) is
    // role-agnostic.
    let host = PluginHost::start(Arc::clone(&reg), Interaction::Tty, true).unwrap();
    host.load_source(PLUGIN, PLUGIN_SRC).unwrap();

    let (parent_tx, parent_rx) = flume::unbounded::<Envelope>();
    let event_tx = EventSender::new(parent_tx, RUN_ID);
    let mcp = session_with_transport(&[(MCP_TOOL, "run a script")], Arc::new(FakeExecutor));

    let mut ctx = stub_ctx_with(&AgentMode::Build, Some(&event_tx), Some(TASK_CALL_ID));
    ctx.registry = Arc::clone(&reg);
    ctx.mcp = Some(mcp);
    ctx.provider = Arc::new(CallToolOnce {
        asked: AtomicBool::new(false),
    });
    ctx.model = Arc::new(Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap());

    let entry = reg.get(TASK_TOOL).expect("task tool registered");
    let inv = entry.tool.parse(&json!({})).expect("parse failed");
    let done = smol::block_on(async { inv.execute(&ctx).await });
    host.unload(PLUGIN).unwrap();
    let output = done
        .output
        .as_ref()
        .map(|o| o.as_text())
        .unwrap_or_default();
    assert!(output.contains("subagent done"), "task failed: {output}");

    let events: Vec<Envelope> = parent_rx.drain().collect();
    let names: Vec<String> = events.iter().map(event_name).collect();
    let routing: Vec<String> = events
        .iter()
        .map(|e| {
            let id = match &e.event {
                AgentEvent::ToolStart(s) => s.id.clone(),
                AgentEvent::ToolDone(d) => d.id.clone(),
                AgentEvent::LiveToolBuf { id, .. } => id.clone(),
                AgentEvent::ToolSnapshot { id, .. } => id.clone(),
                AgentEvent::ToolPending { id, .. } => id.clone(),
                _ => String::new(),
            };
            format!(
                "{} id={:?} subagent={} run_id={}",
                event_name(e),
                id,
                e.subagent
                    .as_ref()
                    .map(|s| s.parent_tool_use_id.to_string())
                    .unwrap_or_else(|| "NONE".into()),
                e.run_id
            )
        })
        .collect();
    println!("parent stream:\n{}", routing.join("\n"));

    let start = events.iter().find_map(|e| match &e.event {
        AgentEvent::ToolStart(start) if start.tool.as_ref() == MCP_TOOL => Some(start),
        _ => None,
    });
    assert!(
        start.is_some(),
        "the subagent's MCP call must announce itself; got {names:?}"
    );
    let start = start.unwrap();
    assert_eq!(start.summary, HEADER_SUMMARY);

    let live_idx = events
        .iter()
        .position(|e| matches!(&e.event, AgentEvent::LiveToolBuf { id, .. } if id == MCP_CALL_ID));
    let Some(live_idx) = live_idx else {
        panic!(
            "the view's live body buf must ride the subagent stream to the parent; got {names:?}"
        )
    };
    let live = &events[live_idx];
    assert!(
        live.subagent
            .as_ref()
            .is_some_and(|s| s.parent_tool_use_id == TASK_CALL_ID),
        "the live buf must carry the subagent stamp so the TUI routes it to \
         the subagent's chat; got subagent={:?}",
        live.subagent
            .as_ref()
            .map(|s| s.parent_tool_use_id.to_string())
    );
    assert_eq!(live.run_id, RUN_ID);

    let finished = events
        .iter()
        .any(|e| matches!(&e.event, AgentEvent::ToolDone(d) if d.id == MCP_CALL_ID));
    assert!(finished, "the MCP call must finish; got {names:?}");

    // A frame that ticks between the two events must never see a body for a
    // call it has not been told about: the live buf would be evicted as an
    // orphan and the row falls back to raw llm_output. ToolStart first, live
    // buf second, on the same stream.
    let start_pos = events
        .iter()
        .position(|e| matches!(&e.event, AgentEvent::ToolStart(s) if s.id == MCP_CALL_ID))
        .expect("tool start captured");
    let live_pos = events
        .iter()
        .position(|e| matches!(&e.event, AgentEvent::LiveToolBuf { id, .. } if id == MCP_CALL_ID))
        .expect("live buf captured");
    assert!(
        start_pos < live_pos,
        "ToolStart must precede its LiveToolBuf (start at {start_pos}, buf at {live_pos})"
    );
}
