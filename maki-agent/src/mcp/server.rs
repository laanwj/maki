use std::collections::HashMap;
use std::sync::Arc;

use async_lock::Mutex;
use async_net::unix::{UnixListener, UnixStream};
use futures_lite::io::BufReader;
use futures_lite::{AsyncBufReadExt, AsyncWriteExt};
use serde_json::{Value, json};
use tracing::{debug, warn};

use super::error::McpError;
use super::protocol::{
    CallToolResult, IncomingMessage, JsonRpcNotification, LATEST_PROTOCOL_VERSION, ResourceContent,
    ResourceInfo, ToolInfo,
};
use super::transport::BoxFuture;
use crate::cancel::{CancelToken, CancelTrigger};

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

pub trait ServerHandler: Send + Sync + 'static {
    /// Called on `initialize`; for the executor the params carry the
    /// brain-pushed config blob. Rejecting the handshake rejects the connection.
    fn initialize(&self, params: Value) -> Result<(), String>;
    fn tools(&self) -> Vec<ToolInfo>;
    fn resources(&self) -> Vec<ResourceInfo>;
    fn read_resource(&self, uri: &str) -> Result<Vec<ResourceContent>, String>;
    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        args: Value,
        progress: ProgressSink,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Result<CallToolResult, String>>;
}

#[derive(Clone)]
pub struct ConnWriter {
    inner: Arc<Mutex<UnixStream>>,
}

impl ConnWriter {
    async fn write_json(&self, value: &impl serde::Serialize) -> Result<(), McpError> {
        let mut buf = serde_json::to_vec(value).map_err(|e| McpError::InvalidResponse {
            server: "client".into(),
            reason: e.to_string(),
        })?;
        buf.push(b'\n');
        let mut stream = self.inner.lock().await;
        stream
            .write_all(&buf)
            .await
            .map_err(|e| McpError::WriteFailed {
                server: "client".into(),
                reason: e.to_string(),
            })?;
        stream.flush().await.map_err(|e| McpError::WriteFailed {
            server: "client".into(),
            reason: e.to_string(),
        })
    }
}

/// Sends `notifications/progress` for one in-flight tool call. Silent when the
/// client did not supply a `progressToken`.
#[derive(Clone)]
pub struct ProgressSink {
    token: Option<Value>,
    writer: Option<ConnWriter>,
}

impl ProgressSink {
    pub fn detached() -> Self {
        Self {
            token: None,
            writer: None,
        }
    }

    pub async fn send(&self, progress: Value) {
        let (Some(token), Some(writer)) = (&self.token, &self.writer) else {
            return;
        };
        let params = json!({ "progressToken": token, "progress": progress });
        if let Err(e) = writer
            .write_json(&JsonRpcNotification::new(
                "notifications/progress",
                Some(params),
            ))
            .await
        {
            debug!(error = %e, "failed to send progress notification");
        }
    }
}

pub async fn serve_unix<H: ServerHandler>(
    listener: UnixListener,
    handler: Arc<H>,
) -> Result<(), McpError> {
    loop {
        let (stream, _) = listener.accept().await.map_err(|e| McpError::StartFailed {
            server: "listener".into(),
            reason: e.to_string(),
        })?;
        let handler = Arc::clone(&handler);
        smol::spawn(async move {
            if let Err(e) = run_conn(stream, handler).await {
                debug!(error = %e, "MCP connection closed");
            }
        })
        .detach();
    }
}

async fn run_conn<H: ServerHandler>(stream: UnixStream, handler: Arc<H>) -> Result<(), McpError> {
    let writer = ConnWriter {
        inner: Arc::new(Mutex::new(stream.clone())),
    };
    let in_flight: Arc<Mutex<HashMap<u64, CancelTrigger>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| McpError::ServerDied {
                server: format!("client: read failed: {e}"),
            })?;
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: IncomingMessage = match serde_json::from_str(trimmed) {
            Ok(msg) => msg,
            Err(e) => {
                debug!(error = %e, line = trimmed, "non-JSON-RPC line from client");
                continue;
            }
        };
        match (msg.id, msg.method) {
            (Some(id), Some(method)) => {
                let (trigger, token) = CancelToken::new();
                in_flight.lock().await.insert(id, trigger);
                let writer = writer.clone();
                let handler = Arc::clone(&handler);
                let in_flight = Arc::clone(&in_flight);
                smol::spawn(async move {
                    let response =
                        handle_request(&handler, id, &method, msg.params, writer.clone(), token)
                            .await;
                    if let Err(e) = writer.write_json(&response).await {
                        warn!(error = %e, id, "failed to write MCP response");
                    }
                    in_flight.lock().await.remove(&id);
                })
                .detach();
            }
            (None, Some(method)) => {
                if method == "notifications/cancelled"
                    && let Some(request_id) =
                        msg.params.as_ref().and_then(|p| p["requestId"].as_u64())
                    && let Some(trigger) = in_flight.lock().await.remove(&request_id)
                {
                    trigger.cancel();
                }
            }
            _ => {}
        }
    }
}

async fn handle_request<H: ServerHandler>(
    handler: &Arc<H>,
    id: u64,
    method: &str,
    params: Option<Value>,
    writer: ConnWriter,
    cancel: CancelToken,
) -> Value {
    match method {
        "initialize" => match handler.initialize(params.unwrap_or(Value::Null)) {
            Ok(()) => result_value(
                id,
                json!({
                    "protocolVersion": LATEST_PROTOCOL_VERSION,
                    "capabilities": { "tools": {}, "resources": {} },
                    "serverInfo": { "name": "maki-executor", "version": env!("CARGO_PKG_VERSION") },
                }),
            ),
            Err(e) => error_value(id, INVALID_PARAMS, e),
        },
        "ping" => result_value(id, json!({})),
        "tools/list" => result_value(id, json!({ "tools": handler.tools() })),
        "resources/list" => result_value(id, json!({ "resources": handler.resources() })),
        "resources/read" => {
            let Some(uri) = params.as_ref().and_then(|p| p["uri"].as_str()) else {
                return error_value(id, INVALID_PARAMS, "resources/read needs params.uri".into());
            };
            match handler.read_resource(uri) {
                Ok(contents) => result_value(id, json!({ "contents": contents })),
                Err(e) => error_value(id, INVALID_PARAMS, e),
            }
        }
        "tools/call" => {
            let Some(name) = params.as_ref().and_then(|p| p["name"].as_str()) else {
                return error_value(id, INVALID_PARAMS, "tools/call needs params.name".into());
            };
            let args = params
                .as_ref()
                .and_then(|p| p["arguments"].as_object().cloned())
                .map(Value::Object)
                .unwrap_or_else(|| json!({}));
            let progress = ProgressSink {
                token: params
                    .as_ref()
                    .map(|p| p["_meta"]["progressToken"].clone())
                    .filter(|t| !t.is_null()),
                writer: Some(writer),
            };
            match handler.call_tool(name, args, progress, cancel).await {
                Ok(result) => result_value(id, serde_json::to_value(result).unwrap_or(Value::Null)),
                Err(message) => result_value(
                    id,
                    json!({ "content": [{ "text": message }], "isError": true }),
                ),
            }
        }
        _ => error_value(id, METHOD_NOT_FOUND, format!("no such method: {method}")),
    }
}

fn result_value(id: u64, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_value(id: u64, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::super::protocol::CallToolContent;
    use super::super::socket::SocketTransport;
    use super::super::transport::{self, McpTransport};
    use super::*;
    use std::time::Duration;

    struct FakeHandler;

    impl ServerHandler for FakeHandler {
        fn initialize(&self, _params: Value) -> Result<(), String> {
            Ok(())
        }
        fn tools(&self) -> Vec<ToolInfo> {
            vec![ToolInfo {
                name: "echo".into(),
                description: "echo the args".into(),
                input_schema: json!({ "type": "object" }),
            }]
        }
        fn resources(&self) -> Vec<ResourceInfo> {
            vec![ResourceInfo {
                uri: "file:///ws/main.rs".into(),
                name: "main.rs".into(),
                description: None,
                mime_type: None,
            }]
        }
        fn read_resource(&self, uri: &str) -> Result<Vec<ResourceContent>, String> {
            Ok(vec![ResourceContent {
                uri: uri.into(),
                text: Some("fn main() {}\n".into()),
                blob: None,
            }])
        }
        fn call_tool<'a>(
            &'a self,
            name: &'a str,
            args: Value,
            progress: ProgressSink,
            cancel: CancelToken,
        ) -> BoxFuture<'a, Result<CallToolResult, String>> {
            Box::pin(async move {
                match name {
                    "echo" => Ok(CallToolResult {
                        content: vec![CallToolContent {
                            text: args["text"].as_str().unwrap_or("").into(),
                        }],
                        is_error: false,
                    }),
                    "hang" => {
                        cancel
                            .race(std::future::pending::<()>())
                            .await
                            .map(|()| CallToolResult {
                                content: vec![],
                                is_error: false,
                            })
                    }
                    "progress" => {
                        progress.send(json!({ "chunk": "half" })).await;
                        Ok(CallToolResult {
                            content: vec![CallToolContent {
                                text: "done".into(),
                            }],
                            is_error: false,
                        })
                    }
                    _ => Err(format!("unknown tool: {name}")),
                }
            })
        }
    }

    fn spawn_server_with<H: ServerHandler>(
        dir: &tempfile::TempDir,
        handler: H,
    ) -> std::path::PathBuf {
        let path = dir.path().join("srv.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        smol::spawn(serve_unix(listener, Arc::new(handler))).detach();
        path
    }

    fn spawn_server(dir: &tempfile::TempDir) -> std::path::PathBuf {
        spawn_server_with(dir, FakeHandler)
    }

    #[test]
    fn initialize_list_and_call() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = spawn_server(&dir);
            let client = SocketTransport::connect("test", &path, Duration::from_secs(5))
                .await
                .unwrap();

            let caps = transport::initialize(&client).await.unwrap();
            assert!(caps.tools);
            assert!(caps.resources);

            let tools = transport::list_tools(&client).await.unwrap();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].name, "echo");

            let out = transport::call_tool(&client, "echo", &json!({ "text": "hi" }))
                .await
                .unwrap();
            assert_eq!(out, "hi");

            let err = transport::call_tool(&client, "nope", &json!({}))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("unknown tool"), "got: {err}");
        });
    }

    #[test]
    fn resources_list_and_read() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = spawn_server(&dir);
            let client = SocketTransport::connect("test", &path, Duration::from_secs(5))
                .await
                .unwrap();

            let resources = transport::list_resources(&client).await.unwrap();
            assert_eq!(resources.len(), 1);
            assert_eq!(resources[0].uri, "file:///ws/main.rs");

            let contents = transport::read_resource(&client, "file:///ws/main.rs")
                .await
                .unwrap();
            assert_eq!(contents[0].text.as_deref(), Some("fn main() {}\n"));
        });
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = spawn_server(&dir);
            let client = SocketTransport::connect("test", &path, Duration::from_secs(5))
                .await
                .unwrap();
            let err = client.send_request("bogus/method", None).await.unwrap_err();
            assert!(matches!(
                err,
                McpError::RpcError {
                    code: METHOD_NOT_FOUND,
                    ..
                }
            ));
        });
    }

    /// Signals when the hanging tool call has actually started, so the
    /// cancelled notification is guaranteed to arrive after registration.
    struct HangHandler {
        entered: smol::channel::Sender<()>,
    }

    impl ServerHandler for HangHandler {
        fn initialize(&self, _params: Value) -> Result<(), String> {
            Ok(())
        }
        fn tools(&self) -> Vec<ToolInfo> {
            vec![]
        }
        fn resources(&self) -> Vec<ResourceInfo> {
            vec![]
        }
        fn read_resource(&self, _uri: &str) -> Result<Vec<ResourceContent>, String> {
            Ok(vec![])
        }
        fn call_tool<'a>(
            &'a self,
            _name: &'a str,
            _args: Value,
            _progress: ProgressSink,
            cancel: CancelToken,
        ) -> BoxFuture<'a, Result<CallToolResult, String>> {
            Box::pin(async move {
                let _ = self.entered.send(()).await;
                cancel
                    .race(std::future::pending::<()>())
                    .await
                    .map(|()| CallToolResult {
                        content: vec![],
                        is_error: false,
                    })
            })
        }
    }

    #[test]
    fn cancelled_notification_cancels_in_flight_call() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let (entered_tx, entered_rx) = smol::channel::bounded(1);
            let path = spawn_server_with(
                &dir,
                HangHandler {
                    entered: entered_tx,
                },
            );
            let client = Arc::new(
                SocketTransport::connect("test", &path, Duration::from_secs(5))
                    .await
                    .unwrap(),
            );

            // Cancellation is per-connection, and the first request on a fresh
            // transport gets id 1; the cancelled notification refers back to it.
            let call = {
                let client = Arc::clone(&client);
                smol::spawn(async move {
                    transport::call_tool(client.as_ref(), "hang", &json!({})).await
                })
            };
            entered_rx.recv().await.unwrap();
            client
                .send_notification("notifications/cancelled", Some(json!({ "requestId": 1 })))
                .await
                .unwrap();

            let err = call.await.unwrap_err();
            assert!(err.to_string().contains("cancelled"), "got: {err}");
        });
    }

    #[test]
    fn progress_notification_precedes_response() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = spawn_server(&dir);
            let mut stream = UnixStream::connect(&path).await.unwrap();

            let request = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": "progress", "arguments": {}, "_meta": { "progressToken": 7 } },
            });
            let mut buf = serde_json::to_vec(&request).unwrap();
            buf.push(b'\n');
            stream.write_all(&buf).await.unwrap();
            stream.flush().await.unwrap();

            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let progress: Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(progress["method"], "notifications/progress");
            assert_eq!(progress["params"]["progressToken"], json!(7));
            assert_eq!(progress["params"]["progress"], json!({ "chunk": "half" }));

            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let response: Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(response["id"], 1);
            assert_eq!(response["result"]["content"][0]["text"], "done");
        });
    }
}
