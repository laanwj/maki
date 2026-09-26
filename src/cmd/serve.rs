use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_net::unix::UnixListener;
use base64::Engine;
use color_eyre::eyre::{Context, Result};
use maki_agent::cancel::CancelToken;
use maki_agent::mcp::protocol::{
    CallToolContent, CallToolResult, ResourceContent, ResourceInfo, ToolInfo,
};
use maki_agent::mcp::push::ExecutorPush;
use maki_agent::mcp::server::{ProgressSink, ServerHandler, serve_unix};
use maki_agent::mcp::transport::BoxFuture;
use maki_agent::permissions::PermissionManager;
use maki_agent::tools::{
    CallOrigin, DescriptionContext, FileAccess, ToolAudience, ToolContext, ToolFilter, ToolRegistry,
};
use maki_agent::{AgentEvent, AgentMode, Envelope, EventSender, ToolOutput};
use maki_config::{PluginsConfig, ProjectConfig};
use maki_lua::{PluginHost, UiAction};
use serde_json::{Value, json};
use tracing::info;

const SOCKET_MODE: u32 = 0o660;

struct ExecutorHandler {
    registry: Arc<ToolRegistry>,
    workspace: PathBuf,
    workspace_canonical: PathBuf,
    call_ids: AtomicU64,
    /// Shared across calls so the dispatcher's per-file lock serializes
    /// siblings (one batch, two `edit`s on one file) instead of each call
    /// locking a private map.
    file_access: Arc<FileAccess>,
    host: std::sync::Mutex<PluginHost>,
    state: std::sync::Mutex<HandlerState>,
    notify_tx: flume::Sender<String>,
    skills_root: PathBuf,
    /// One config application at a time; a brain /reload reconnects and
    /// re-sends its push.
    applying: AtomicU64,
}

#[derive(Default)]
struct HandlerState {
    ready: bool,
    trusted: bool,
    deny_rules: Vec<maki_config::PermissionRule>,
    downstream: Option<maki_agent::McpHandle>,
}

/// Executor-role maki.notify ends up as MCP logging notifications. The drain
/// runs per plugin-host generation: a host's channel dies with it.
fn spawn_notify_drain(notify_tx: &flume::Sender<String>, host: &PluginHost) {
    let ui_action_rx = host.ui_action_rx();
    let notify_tx = notify_tx.clone();
    smol::spawn(async move {
        while let Ok(action) = ui_action_rx.recv_async().await {
            if let UiAction::Flash(message) = action {
                let _ = notify_tx.send(message);
            }
        }
    })
    .detach();
}

impl ExecutorHandler {
    async fn initialize_inner(&self, params: Value) -> Result<Value, String> {
        // One application at a time; a brain /reload reconnects and re-sends
        // its push, and every push reconfigures from scratch.
        if self.applying.swap(1, Ordering::AcqRel) != 0 {
            return Err("executor is reconfiguring".into());
        }
        let result = self.apply(params["executorConfig"].clone()).await;
        self.applying.store(0, Ordering::Release);
        result
    }

    async fn apply(&self, config: Value) -> Result<Value, String> {
        let workspace = json!({ "workspace": self.workspace_canonical.to_string_lossy() });
        let push: ExecutorPush = if config.is_null() {
            ExecutorPush::default()
        } else {
            serde_json::from_value(config).map_err(|e| format!("invalid executorConfig: {e}"))?
        };

        // Tear down the previous generation first: downstream servers, the Lua
        // tools of the old plugin host, and the host itself (its drop stops
        // the Lua thread). In-flight calls from a dead connection error out.
        let old_downstream = self
            .state
            .lock()
            .map_err(|e| e.to_string())?
            .downstream
            .take();
        if let Some(handle) = old_downstream {
            handle.shutdown().await;
        }
        self.registry.clear_lua();

        let mut host =
            PluginHost::executor(Arc::clone(&self.registry)).map_err(|e| e.to_string())?;
        spawn_notify_drain(&self.notify_tx, &host);

        // Every push is a fresh provider-load generation: begin opens the
        // staging window and commit publishes it in one swap, so a provider
        // plugin the new push no longer loads stops serving. Without this the
        // second initialize re-registers a slug the first left staged.
        maki_providers::plugin::begin_load();
        let names = push.builtin_plugins.clone().unwrap_or_else(|| {
            maki_config::DEFAULT_BUILTINS
                .iter()
                .filter(|name| !maki_config::is_brain_role_builtin(name))
                .map(|s| (*s).to_owned())
                .collect()
        });
        host.load_builtins(&PluginsConfig {
            enabled: true,
            names,
            packages: Vec::new(),
            opts: push.plugin_opts.clone(),
        })
        .map_err(|e| e.to_string())?;
        for plugin in &push.plugin_sources {
            host.load_pushed_plugin(&plugin.name, &plugin.source)
                .map_err(|e| e.to_string())?;
        }
        maki_providers::plugin::commit_load();

        write_pushed_skills(&self.skills_root, &push.skills)?;

        let deny_rules = push
            .deny_rules
            .iter()
            .map(|rule| {
                Ok(maki_config::PermissionRule {
                    tool: maki_config::ToolKey::parse(&rule.tool).map_err(|e| e.to_string())?,
                    scope: rule.scope.clone(),
                    effect: maki_config::Effect::Deny,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        let downstream = if !push.mcp_servers.is_empty() {
            let handle = maki_agent::mcp::start_with_config(maki_agent::mcp::config::McpConfig {
                mcp: push.mcp_servers.clone(),
                ..Default::default()
            });
            // tools/list must see the downstream tools, so the handshake waits
            // for the initial connects to finish.
            if let Some(h) = &handle {
                h.ready().await;
            }
            handle
        } else {
            None
        };

        {
            let mut state = self.state.lock().map_err(|e| e.to_string())?;
            state.ready = true;
            state.trusted = push.trusted;
            state.deny_rules = deny_rules;
            state.downstream = downstream;
            *self.host.lock().map_err(|e| e.to_string())? = host;
        }

        info!(
            skills = push.skills.len(),
            plugins = push.plugin_sources.len(),
            mcp_servers = push.mcp_servers.len(),
            "executor configured"
        );
        Ok(workspace)
    }
}

impl ServerHandler for ExecutorHandler {
    /// The brain's config push arrives here; builtins load only now, because
    /// their plugin options ride the push. Every pushed initialize
    /// reconfigures from scratch, which is how a brain /reload lands.
    fn initialize<'a>(&'a self, params: Value) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move { self.initialize_inner(params).await })
    }

    fn workspace(&self) -> String {
        self.workspace_canonical.to_string_lossy().into_owned()
    }

    fn tools(&self) -> Vec<ToolInfo> {
        let has_downstream = self
            .state
            .lock()
            .map(|s| s.downstream.is_some())
            .unwrap_or(false);
        let dctx = DescriptionContext {
            filter: &ToolFilter::All,
            audience: ToolAudience::MAIN,
            workflow: false,
            mcp: has_downstream,
        };
        let mut tools: Vec<ToolInfo> = self
            .registry
            .iter()
            .iter()
            .map(|rt| ToolInfo {
                name: rt.name().to_string(),
                description: rt.tool.description(&dctx).into_owned(),
                input_schema: rt.tool.schema(),
            })
            .collect();
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(downstream) = &state.downstream {
            tools.extend(downstream.tool_infos());
        }
        tools
    }

    fn resources(&self) -> Vec<ResourceInfo> {
        // The workspace file list the brain's @-completion reads through us.
        // Corpus entries are relative to the workspace root.
        let index = maki_agent::file_index(&self.workspace);
        index.refresh();
        let mut resources: Vec<ResourceInfo> = index
            .corpus()
            .iter()
            .map(|rel| ResourceInfo {
                uri: format!("file://{}", self.workspace_canonical.join(rel).display()),
                name: rel.to_owned(),
                description: None,
                mime_type: None,
            })
            .collect();
        // Plugin-registered resources (maki://git/branch and friends) sit
        // beside the corpus; they own their non-file schemes outright.
        if let Ok(host) = self.host.lock() {
            resources.extend(
                host.registered_resources()
                    .into_iter()
                    .map(|uri| ResourceInfo {
                        name: uri.clone(),
                        uri,
                        description: None,
                        mime_type: None,
                    }),
            );
        }
        resources
    }

    fn read_resource(&self, uri: &str) -> Result<Vec<ResourceContent>, String> {
        let Some(path) = uri.strip_prefix("file://") else {
            let host = self.host.lock().map_err(|e| e.to_string())?;
            return match host.read_registered_resource(uri)? {
                Some(text) => Ok(vec![ResourceContent {
                    uri: uri.into(),
                    text: Some(text),
                    blob: None,
                }]),
                None => Err(format!("no resource registered for {uri}")),
            };
        };
        let canonical = fs::canonicalize(Path::new(path)).map_err(|e| e.to_string())?;
        if !canonical.starts_with(&self.workspace_canonical) {
            return Err("resource is outside the workspace".into());
        }
        match fs::read_to_string(&canonical) {
            Ok(text) => Ok(vec![ResourceContent {
                uri: uri.into(),
                text: Some(text),
                blob: None,
            }]),
            Err(_) => {
                let bytes = fs::read(&canonical).map_err(|e| e.to_string())?;
                Ok(vec![ResourceContent {
                    uri: uri.into(),
                    text: None,
                    blob: Some(Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &bytes,
                    )),
                }])
            }
        }
    }

    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        args: Value,
        session_id: Option<String>,
        task_id: Option<String>,
        progress: ProgressSink,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Result<CallToolResult, String>> {
        Box::pin(async move {
            let (ready, deny_rules, downstream) = {
                let state = self.state.lock().map_err(|e| e.to_string())?;
                (
                    state.ready,
                    state.deny_rules.clone(),
                    state.downstream.clone(),
                )
            };
            if !ready {
                return Err("executor is not initialized".into());
            }

            // Per-call event channel: ToolOutput events from this call become
            // progress notifications on the MCP connection.
            let (tx, rx) = flume::unbounded::<Envelope>();
            let event_tx = EventSender::new(tx, 0);
            let drain = {
                let progress = progress.clone();
                smol::spawn(async move {
                    while let Ok(envelope) = rx.recv_async().await {
                        match envelope.event {
                            AgentEvent::ToolOutput { id, content, .. } => {
                                progress.send(json!({ "id": id, "content": content })).await;
                            }
                            AgentEvent::ToolProgress { id, payload } => {
                                progress.send(json!({ "id": id, "payload": payload })).await;
                            }
                            _ => {}
                        }
                    }
                })
            };

            let id = format!("exe-{}", self.call_ids.fetch_add(1, Ordering::Relaxed));
            let mut ctx = executor_ctx(
                &event_tx,
                cancel,
                &self.workspace,
                &self.registry,
                &self.file_access,
                deny_rules,
                downstream,
            );
            // The per-call dispatch ctx carries the id, or a handler's live
            // channel (progress, live bufs) has nothing to key on.
            ctx.tool_use_id = Some(id.clone());
            // The chat (and subagent task) the call serves; per-session tool
            // state (the python tool's store) keys on these. An id the brain
            // sent but we cannot parse is a protocol bug: fail loudly.
            if let Some(raw) = &session_id {
                ctx.session_id = Some(raw.parse().map_err(|_| {
                    format!("tools/call {name}: unparsable maki_session_id {raw:?}")
                })?);
            }
            ctx.task_id = task_id.map(Arc::from);
            let done =
                maki_agent::agent::tool_dispatch::run(id, name, &args, &ctx, CallOrigin::Model)
                    .await;

            drop(ctx);
            drop(event_tx);
            drain.cancel().await;

            let content = match &*done.output {
                ToolOutput::Image { source, text } => vec![
                    CallToolContent::text(text.clone()),
                    CallToolContent::Image {
                        data: source.data.to_string(),
                        mime_type: source.media_type.mime().into(),
                    },
                ],
                other => vec![CallToolContent::text(other.as_text())],
            };
            Ok(CallToolResult {
                content,
                is_error: done.is_error,
            })
        })
    }
}

fn executor_ctx(
    event_tx: &EventSender,
    cancel: CancelToken,
    workspace: &Path,
    registry: &Arc<ToolRegistry>,
    file_access: &Arc<FileAccess>,
    deny_rules: Vec<maki_config::PermissionRule>,
    downstream: Option<maki_agent::McpHandle>,
) -> ToolContext {
    let mut ctx = maki_agent::tools::interpreter_ctx(
        &AgentMode::Build,
        event_tx,
        cancel,
        Arc::new(PermissionManager::new(
            maki_config::PermissionsConfig {
                yolo: true,
                rules: deny_rules,
                ..Default::default()
            },
            workspace.to_path_buf(),
            ProjectConfig::discover(workspace),
            Arc::default(),
        )),
        Arc::clone(file_access),
        None,
        Arc::clone(registry),
    );
    ctx.mcp = downstream.map(|handle| maki_agent::McpSession::new(handle, &[]));
    ctx
}

fn write_pushed_skills(
    root: &Path,
    skills: &[maki_agent::mcp::push::SkillPush],
) -> Result<(), String> {
    // Replace wholesale: a skill removed from the config must not linger.
    if root.exists() {
        fs::remove_dir_all(root).map_err(|e| e.to_string())?;
    }
    for skill in skills {
        let dir = root.join(&skill.name);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        fs::write(dir.join("SKILL.md"), &skill.content).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn run(socket: PathBuf, workspace: Option<PathBuf>) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
    if let Some(workspace) = workspace {
        std::env::set_current_dir(&workspace)
            .with_context(|| format!("chdir {}", workspace.display()))?;
    }
    let cwd = std::env::current_dir().context("resolve current directory")?;
    let workspace_canonical = cwd.canonicalize().context("canonicalize workspace")?;

    let registry = Arc::clone(ToolRegistry::global_arc());
    // Builtins load at initialize, when the brain's config push has arrived.
    let host = PluginHost::executor(Arc::clone(&registry)).context("initialize lua plugin host")?;

    let (notify_tx, notify_rx) = flume::unbounded::<String>();

    let handler = Arc::new(ExecutorHandler {
        registry,
        workspace: cwd,
        workspace_canonical,
        call_ids: AtomicU64::new(1),
        file_access: FileAccess::fresh(),
        host: std::sync::Mutex::new(host),
        state: std::sync::Mutex::new(HandlerState::default()),
        notify_tx,
        skills_root: maki_storage::StateDir::resolve()
            .context("resolve state directory")?
            .path()
            .join("executor/skills"),
        applying: AtomicU64::new(0),
    });

    if let Some(parent) = socket.parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent)
            .and_then(|()| fs::set_permissions(parent, fs::Permissions::from_mode(0o700)))
            .with_context(|| format!("create {}", parent.display()))?;
    }
    if socket.exists() {
        fs::remove_file(&socket).with_context(|| format!("remove stale {}", socket.display()))?;
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(SOCKET_MODE))
        .with_context(|| format!("chmod {}", socket.display()))?;

    info!(socket = %socket.display(), "executor listening");
    smol::block_on(serve_unix(listener, handler, notify_rx)).context("serve")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::mcp::push::{ExecutorPush, PluginSourcePush, SkillPush};

    const PUSHED_TOOL: &str = "pushtool";
    const SKILL_BODY: &str = "---\nname: demo\ndescription: pushed\n---\nbody text\n";

    fn test_handler(workspace: &Path, skills_root: &Path) -> Arc<ExecutorHandler> {
        let registry = Arc::new(ToolRegistry::new());
        let host = PluginHost::executor(Arc::clone(&registry)).unwrap();
        let (notify_tx, _) = flume::unbounded();
        Arc::new(ExecutorHandler {
            registry,
            workspace: workspace.to_path_buf(),
            workspace_canonical: workspace.canonicalize().unwrap(),
            call_ids: AtomicU64::new(1),
            file_access: FileAccess::fresh(),
            host: std::sync::Mutex::new(host),
            state: std::sync::Mutex::new(HandlerState::default()),
            notify_tx,
            skills_root: skills_root.to_path_buf(),
            applying: AtomicU64::new(0),
        })
    }

    fn plugin_source(text: &str) -> PluginSourcePush {
        PluginSourcePush {
            name: PUSHED_TOOL.into(),
            source: format!(
                r#"maki.api.register_tool({{
    name = "{PUSHED_TOOL}",
    description = "pushed",
    schema = {{ type = "object", properties = {{}} }},
    handler = function() return "{text}" end,
}})"#
            ),
        }
    }

    fn initialize(handler: &ExecutorHandler, push: ExecutorPush) {
        smol::block_on(handler.initialize(json!({ "executorConfig": push }))).unwrap();
    }

    fn call(handler: &ExecutorHandler, name: &str) -> CallToolResult {
        call_with(handler, name, json!({}))
    }

    fn call_with(handler: &ExecutorHandler, name: &str, args: Value) -> CallToolResult {
        smol::block_on(handler.call_tool(
            name,
            args,
            None,
            None,
            ProgressSink::detached(),
            CancelToken::none(),
        ))
        .unwrap()
    }

    fn call_text(handler: &ExecutorHandler, name: &str) -> String {
        call(handler, name).joined_text()
    }

    #[test]
    fn every_initialize_reconfigures() {
        let dir = tempfile::tempdir().unwrap();
        let handler = test_handler(dir.path(), &dir.path().join("skills"));

        let mut push = ExecutorPush {
            trusted: true,
            ..Default::default()
        };
        push.plugin_sources.push(plugin_source("v1"));
        initialize(&handler, push);
        assert_eq!(call_text(&handler, PUSHED_TOOL), "v1");

        let mut push = ExecutorPush {
            trusted: true,
            ..Default::default()
        };
        push.plugin_sources.push(plugin_source("v2"));
        initialize(&handler, push);
        assert_eq!(call_text(&handler, PUSHED_TOOL), "v2");

        initialize(&handler, ExecutorPush::default());
        let result = call(&handler, PUSHED_TOOL);
        assert!(result.is_error);
        assert!(
            result.joined_text().contains("unknown tool"),
            "got: {}",
            result.joined_text()
        );
        let tools = handler.tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(!names.contains(&PUSHED_TOOL));
        assert!(names.contains(&"bash"));
    }

    #[test]
    fn session_and_task_id_reach_tool_ctx() {
        let dir = tempfile::tempdir().unwrap();
        let handler = test_handler(dir.path(), &dir.path().join("skills"));
        let push = ExecutorPush {
            plugin_sources: vec![PluginSourcePush {
                name: "whoami".into(),
                source: r#"maki.api.register_tool({
    name = "whoami",
    description = "d",
    schema = { type = "object", properties = {} },
    handler = function(_, ctx) return (ctx:session_id() or "nil") .. "/" .. ctx:task_id() end,
})"#
                .into(),
            }],
            ..Default::default()
        };
        initialize(&handler, push);

        let raw = "01965087-4c71-7f00-8000-000000000000";
        let normalized = raw
            .parse::<maki_storage::id::SessionRef>()
            .unwrap()
            .id()
            .to_string();
        let out = smol::block_on(handler.call_tool(
            "whoami",
            json!({}),
            Some(raw.into()),
            Some("toolu_abc".into()),
            ProgressSink::detached(),
            CancelToken::none(),
        ))
        .unwrap();
        assert_eq!(out.joined_text(), format!("{normalized}/toolu_abc"));

        // Absent ids stay absent; an unparsable session id fails the call.
        assert_eq!(call_text(&handler, "whoami"), "nil/main");
        let bad = smol::block_on(handler.call_tool(
            "whoami",
            json!({}),
            Some("not a session id".into()),
            None,
            ProgressSink::detached(),
            CancelToken::none(),
        ));
        assert!(bad.is_err(), "garbage session id must fail loudly");
    }

    /// Two calls on one `mutable_path` must serialize across the handler:
    /// the sleep forces each call's read-modify-write to overlap the other's,
    /// so a per-call FileAccess (the old behavior) loses one write.
    #[test]
    fn concurrent_mutable_path_calls_serialize() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("f.txt");
        fs::write(&target, "").unwrap();
        let handler = test_handler(dir.path(), &dir.path().join("skills"));
        let push = ExecutorPush {
            plugin_sources: vec![PluginSourcePush {
                name: "slowedit".into(),
                source: r#"---@permissions fs_read, fs_write, run

maki.api.register_tool({
    name = "slowedit",
    description = "d",
    mutable_path = "path",
    schema = { type = "object", properties = {
        path = { type = "string" },
        tag = { type = "string" },
    } },
    handler = function(input)
        local content = assert(maki.fs.read(input.path))
        maki.fn.jobwait(maki.fn.jobstart("sleep 0.3"))
        assert(maki.fs.write(input.path, content .. input.tag))
        return "done"
    end,
})"#
                .into(),
            }],
            ..Default::default()
        };
        initialize(&handler, push);

        let call = |tag: &str| {
            handler.call_tool(
                "slowedit",
                json!({ "path": target.to_str().unwrap(), "tag": tag }),
                None,
                None,
                ProgressSink::detached(),
                CancelToken::none(),
            )
        };
        let (a, b) = smol::block_on(smol::future::zip(call("A"), call("B")));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert!(
            !a.is_error && !b.is_error,
            "a: {}, b: {}",
            a.joined_text(),
            b.joined_text()
        );
        let content = fs::read_to_string(&target).unwrap();
        assert!(
            content.contains('A') && content.contains('B'),
            "both writes must land, got: {content:?}"
        );
    }

    #[test]
    fn pushed_builtin_names_never_load_brain_role_tools() {
        let dir = tempfile::tempdir().unwrap();
        let handler = test_handler(dir.path(), &dir.path().join("skills"));
        let push = ExecutorPush {
            builtin_plugins: Some(vec!["read".into(), "task".into()]),
            ..Default::default()
        };
        initialize(&handler, push);
        let tools = handler.tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"read"));
        assert!(!names.contains(&"task"));
    }

    /// A plugin-registered resource rides resources/list + resources/read
    /// next to the file corpus, without touching the file:// confinement.
    #[test]
    fn registered_resources_cross_the_socket() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let handler = test_handler(dir.path(), &dir.path().join("skills"));
            let push = ExecutorPush {
                plugin_sources: vec![PluginSourcePush {
                    name: "res".into(),
                    source: r#"maki.mcp.register_resource({
    uri = "maki://test/thing",
    read = function(uri) return "served:" .. uri end,
})"#
                    .into(),
                }],
                ..Default::default()
            };
            let sock = dir.path().join("e.sock");
            let listener = async_net::unix::UnixListener::bind(&sock).unwrap();
            smol::spawn(maki_agent::mcp::server::serve_unix(
                listener,
                handler,
                flume::unbounded().1,
            ))
            .detach();

            let transport = maki_agent::mcp::socket::SocketTransport::connect(
                "test",
                &sock,
                Some(std::time::Duration::from_secs(5)),
            )
            .await
            .unwrap();
            maki_agent::mcp::transport::initialize_with(
                &transport,
                Some(json!({ "executorConfig": push })),
            )
            .await
            .unwrap();

            let listed = maki_agent::mcp::transport::list_resources(&transport)
                .await
                .unwrap();
            let uris: Vec<&str> = listed.iter().map(|r| r.uri.as_str()).collect();
            assert!(
                uris.contains(&"maki://test/thing"),
                "registered resource missing from {uris:?}"
            );

            let contents =
                maki_agent::mcp::transport::read_resource(&transport, "maki://test/thing")
                    .await
                    .unwrap();
            assert_eq!(
                contents[0].text.as_deref(),
                Some("served:maki://test/thing")
            );

            let missing =
                maki_agent::mcp::transport::read_resource(&transport, "maki://test/absent").await;
            assert!(missing.is_err(), "a URI nobody registered is an error");

            let outside =
                maki_agent::mcp::transport::read_resource(&transport, "file:///etc/hostname").await;
            assert!(outside.is_err(), "outside the workspace stays refused");
        });
    }

    #[test]
    fn skills_are_replaced_wholesale() {
        let dir = tempfile::tempdir().unwrap();
        let skills_root = dir.path().join("skills");
        let handler = test_handler(dir.path(), &skills_root);

        let mut push = ExecutorPush::default();
        push.skills.push(SkillPush {
            name: "demo".into(),
            content: SKILL_BODY.into(),
        });
        initialize(&handler, push);
        assert!(skills_root.join("demo/SKILL.md").exists());

        initialize(&handler, ExecutorPush::default());
        assert!(!skills_root.join("demo/SKILL.md").exists());
    }

    #[test]
    fn progress_payloads_cross_the_socket() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let handler = test_handler(dir.path(), &dir.path().join("skills"));
            let push = ExecutorPush {
                plugin_sources: vec![PluginSourcePush {
                    name: "prog".into(),
                    source: r#"maki.api.register_tool({
    name = "prog",
    description = "d",
    schema = { type = "object", properties = {} },
    handler = function(input, ctx)
        ctx:progress({ step = 1 })
        ctx:progress("plain text line")
        return "done"
    end,
})"#
                    .into(),
                }],
                ..Default::default()
            };
            let sock = dir.path().join("e.sock");
            let listener = async_net::unix::UnixListener::bind(&sock).unwrap();
            smol::spawn(maki_agent::mcp::server::serve_unix(
                listener,
                handler,
                flume::unbounded().1,
            ))
            .detach();

            let transport = maki_agent::mcp::socket::SocketTransport::connect(
                "test",
                &sock,
                Some(std::time::Duration::from_secs(5)),
            )
            .await
            .unwrap();
            maki_agent::mcp::transport::initialize_with(
                &transport,
                Some(json!({ "executorConfig": push })),
            )
            .await
            .unwrap();
            let (events_tx, _events_rx) = flume::unbounded();
            let (payload_tx, payload_rx) = flume::unbounded();
            let out = maki_agent::mcp::transport::call_tool_streaming(
                &transport,
                "prog",
                &json!({}),
                "c1",
                maki_agent::mcp::transport::CallRoute::default(),
                &EventSender::new(events_tx, 0),
                Some(payload_tx),
            )
            .await
            .unwrap();
            assert_eq!(out.text, "done");
            let payloads: Vec<Value> = payload_rx.try_iter().collect();
            assert_eq!(
                payloads,
                vec![json!({ "step": 1 }), json!("plain text line")]
            );
        });
    }

    #[test]
    fn progress_streams_as_the_job_prints() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let handler = test_handler(dir.path(), &dir.path().join("skills"));
            let push = ExecutorPush {
                plugin_sources: vec![PluginSourcePush {
                    name: "streamer".into(),
                    source: r#"---@permissions run
maki.api.register_tool({
    name = "streamer",
    description = "d",
    schema = { type = "object", properties = {} },
    handler = function(input, ctx)
        local parts = {}
        maki.fn.jobstart({ "python3", "-c", "import time; print('one', flush=True); time.sleep(1.5); print('two')" }, {
            on_stdout = function(_, line) ctx:progress(line) end,
            on_exit = function(_, code)
                ctx:finish({ llm_output = "done" })
            end,
        })
        return nil
    end,
})"#
                    .into(),
                }],
                ..Default::default()
            };
            let sock = dir.path().join("e.sock");
            let listener = async_net::unix::UnixListener::bind(&sock).unwrap();
            smol::spawn(maki_agent::mcp::server::serve_unix(
                listener,
                handler,
                flume::unbounded().1,
            ))
            .detach();

            let transport = maki_agent::mcp::socket::SocketTransport::connect(
                "test",
                &sock,
                Some(std::time::Duration::from_secs(10)),
            )
            .await
            .unwrap();
            maki_agent::mcp::transport::initialize_with(
                &transport,
                Some(json!({ "executorConfig": push })),
            )
            .await
            .unwrap();
            let (events_tx, _events_rx) = flume::unbounded();
            let (payload_tx, payload_rx) = flume::unbounded::<Value>();
            let start = std::time::Instant::now();
            let args = json!({});
            let events = EventSender::new(events_tx, 0);
            let call = maki_agent::mcp::transport::call_tool_streaming(
                &transport,
                "streamer",
                &args,
                "c1",
                maki_agent::mcp::transport::CallRoute::default(),
                &events,
                Some(payload_tx),
            );
            // The call future is lazy: it must be polled alongside the payload
            // collection, or the request is never sent and nothing streams.
            let collect = async {
                let mut arrivals = Vec::new();
                while arrivals.len() < 2 {
                    let payload = payload_rx.recv_async().await.unwrap();
                    arrivals.push((start.elapsed(), payload));
                }
                arrivals
            };
            let (arrivals, out) = smol::future::zip(collect, call).await;
            assert_eq!(out.unwrap().text, "done");
            let first = arrivals[0].0.as_millis();
            let second = arrivals[1].0.as_millis();
            assert!(first < 1200, "first payload batched: {arrivals:?}");
            assert!(
                second >= 1200,
                "second payload should follow the sleep: {arrivals:?}"
            );
        });
    }

    /// An image tool's reply rides the standard content blocks: the caption
    /// as text, the pixels as a base64 image block, reassembled client-side.
    #[test]
    fn image_results_cross_the_socket() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let handler = test_handler(dir.path(), &dir.path().join("skills"));
            let push = ExecutorPush {
                plugin_sources: vec![PluginSourcePush {
                    name: "img".into(),
                    source: r#"maki.api.register_tool({
    name = "img",
    description = "d",
    schema = { type = "object", properties = {} },
    handler = function()
        return {
            llm_output = "[image: shot.png 2B 1x1]",
            image = { media_type = "image/png", data = "aGk=" },
        }
    end,
})"#
                    .into(),
                }],
                ..Default::default()
            };
            let sock = dir.path().join("e.sock");
            let listener = async_net::unix::UnixListener::bind(&sock).unwrap();
            smol::spawn(maki_agent::mcp::server::serve_unix(
                listener,
                handler,
                flume::unbounded().1,
            ))
            .detach();

            let transport = maki_agent::mcp::socket::SocketTransport::connect(
                "test",
                &sock,
                Some(std::time::Duration::from_secs(5)),
            )
            .await
            .unwrap();
            maki_agent::mcp::transport::initialize_with(
                &transport,
                Some(json!({ "executorConfig": push })),
            )
            .await
            .unwrap();
            let (events_tx, _events_rx) = flume::unbounded();
            let out = maki_agent::mcp::transport::call_tool_streaming(
                &transport,
                "img",
                &json!({}),
                "c1",
                maki_agent::mcp::transport::CallRoute::default(),
                &EventSender::new(events_tx, 0),
                None,
            )
            .await
            .unwrap();
            assert_eq!(out.text, "[image: shot.png 2B 1x1]");
            let image = out.image.expect("the image rides the result");
            assert_eq!(image.media_type, maki_agent::ImageMediaType::Png);
            assert_eq!(&*image.data, "aGk=");
        });
    }

    #[test]
    fn pushed_deny_rules_deny_calls() {
        let dir = tempfile::tempdir().unwrap();
        let handler = test_handler(dir.path(), &dir.path().join("skills"));
        let echo = json!({ "command": "echo hi" });

        initialize(&handler, ExecutorPush::default());
        assert!(!call_with(&handler, "bash", echo.clone()).is_error);

        let mut push = ExecutorPush::default();
        push.deny_rules.push(maki_agent::mcp::push::DenyRule {
            tool: "bash".into(),
            scope: None,
        });
        initialize(&handler, push);
        let denied = call_with(&handler, "bash", echo.clone());
        assert!(denied.is_error);
        assert!(
            denied
                .joined_text()
                .contains(maki_agent::permissions::PERMISSION_DENIED_PREFIX),
            "got: {}",
            denied.joined_text()
        );

        // The next push replaces the rules wholesale.
        initialize(&handler, ExecutorPush::default());
        assert!(!call_with(&handler, "bash", echo).is_error);
    }

    #[test]
    fn resources_stay_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("ok.txt"), "inside").unwrap();
        let handler = test_handler(&workspace, &dir.path().join("skills"));
        initialize(&handler, ExecutorPush::default());

        let canonical = workspace.canonicalize().unwrap();
        assert_eq!(handler.workspace(), canonical.to_string_lossy().as_ref());

        let uri = format!(
            "file://{}",
            workspace.canonicalize().unwrap().join("ok.txt").display()
        );
        let contents = handler.read_resource(&uri).unwrap();
        assert_eq!(contents[0].text.as_deref(), Some("inside"));

        let outside = handler.read_resource("file:///etc/hostname");
        assert!(outside.is_err());
    }
}
