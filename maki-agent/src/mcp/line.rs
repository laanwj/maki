use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_lock::Mutex;
use futures_lite::{AsyncBufReadExt, AsyncWriteExt};
use serde_json::Value;
use smol::channel;
use tracing::{debug, info, warn};

use super::error::McpError;
use super::protocol::JsonRpcError;
use super::protocol::{JsonRpcNotification, JsonRpcRequest};
use crate::AgentEvent;
use crate::types::EventSender;

pub(crate) type PendingMap = HashMap<u64, channel::Sender<Result<Value, McpError>>>;

/// Notification routing for a line transport: in-flight tool calls watch
/// progress by progressToken, and UI message subscribers receive
/// notifications/message payloads.
type ProgressWatchers = HashMap<String, ProgressWatcher>;

struct ProgressWatcher {
    tool_id: Arc<str>,
    events: EventSender,
    /// Raw progress payloads for a registered view, when the call has one.
    payloads: Option<flume::Sender<Value>>,
}

#[derive(Clone, Default)]
pub struct NotificationHub {
    progress: Arc<std::sync::Mutex<ProgressWatchers>>,
    messages: Arc<std::sync::Mutex<Vec<flume::Sender<String>>>>,
}

pub struct ProgressGuard {
    hub: NotificationHub,
    token: String,
}

impl Drop for ProgressGuard {
    fn drop(&mut self) {
        self.hub
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.token);
    }
}

impl NotificationHub {
    /// Progress notifications carrying `token` become `ToolOutput` events for
    /// `tool_id` until the guard drops; with `payloads`, the raw progress
    /// value also flows there (a view's feed).
    pub fn watch_progress(
        &self,
        token: &str,
        tool_id: Arc<str>,
        events: EventSender,
        payloads: Option<flume::Sender<Value>>,
    ) -> ProgressGuard {
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                token.to_owned(),
                ProgressWatcher {
                    tool_id,
                    events,
                    payloads,
                },
            );
        ProgressGuard {
            hub: self.clone(),
            token: token.to_owned(),
        }
    }

    pub fn watch_messages(&self) -> flume::Receiver<String> {
        let (tx, rx) = flume::unbounded();
        self.messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tx);
        rx
    }

    fn route(&self, method: &str, params: Option<&Value>) {
        match method {
            "notifications/progress" => {
                let Some(params) = params else { return };
                let token = match &params["progressToken"] {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let watchers = self.progress.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(watcher) = watchers.get(&token) {
                    let progress = &params["progress"];
                    if let Some(content) = progress["content"].as_str() {
                        watcher.events.try_send(AgentEvent::ToolOutput {
                            id: watcher.tool_id.to_string(),
                            content: content.to_owned(),
                        });
                    }
                    // Structured payloads go to the view only: their shape is
                    // the tool's contract with it, not generic display text.
                    if let Some(payloads) = &watcher.payloads
                        && let Some(payload) = progress.get("payload")
                    {
                        let _ = payloads.try_send(payload.clone());
                    }
                }
            }
            "notifications/message" => {
                let Some(data) = params.and_then(|p| p["data"].as_str()) else {
                    return;
                };
                let messages = self.messages.lock().unwrap_or_else(|e| e.into_inner());
                for tx in messages.iter() {
                    let _ = tx.try_send(data.to_owned());
                }
            }
            _ => {}
        }
    }
}

#[derive(serde::Deserialize)]
struct IncomingLine {
    id: Option<u64>,
    method: Option<String>,
    params: Option<Value>,
    result: Option<Value>,
    error: Option<JsonRpcError>,
}

const LINE_DELIMITER: u8 = b'\n';

/// ndjson JSON-RPC connection shared by the byte-stream transports (stdio, socket).
pub struct LineTransport<W> {
    name: Arc<str>,
    kind: &'static str,
    writer: Mutex<W>,
    pending: Arc<Mutex<PendingMap>>,
    next_id: AtomicU64,
    /// `None` waits for the server indefinitely (the executor: its tools
    /// bound their own runs).
    timeout: Option<Duration>,
    alive: Arc<AtomicBool>,
    hub: NotificationHub,
    _reader_task: smol::Task<()>,
}

impl<W> LineTransport<W>
where
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    pub fn new(
        name: &str,
        kind: &'static str,
        writer: W,
        reader: impl AsyncBufReadExt + Send + Unpin + 'static,
        timeout: Option<Duration>,
    ) -> Self {
        let name: Arc<str> = Arc::from(name);
        let alive = Arc::new(AtomicBool::new(true));
        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));
        let hub = NotificationHub::default();
        let reader_task = {
            let name = Arc::clone(&name);
            let alive = Arc::clone(&alive);
            let pending = Arc::clone(&pending);
            let hub = hub.clone();
            smol::spawn(async move {
                let result = reader_loop(&name, reader, &pending, &hub).await;
                if let Err(e) = &result {
                    warn!(server = &*name, error = %e, "MCP reader loop ended");
                }
                alive.store(false, Ordering::Release);
                for (_, sender) in pending.lock().await.drain() {
                    let _ = sender
                        .send(Err(McpError::ServerDied {
                            server: (*name).into(),
                        }))
                        .await;
                }
            })
        };
        Self {
            name,
            kind,
            writer: Mutex::new(writer),
            pending,
            next_id: AtomicU64::new(1),
            timeout,
            alive,
            hub,
            _reader_task: reader_task,
        }
    }

    pub fn hub(&self) -> NotificationHub {
        self.hub.clone()
    }

    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(self.server_died());
        }

        let start = Instant::now();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest::new(id, method, params);

        let (tx, rx) = channel::bounded(1);
        self.pending.lock().await.insert(id, tx);

        if let Err(e) = self.write_line(&self.serialize(&req)?).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        let recv = async { rx.recv().await.unwrap_or(Err(self.server_died())) };
        let result = match self.timeout {
            Some(timeout) => {
                futures_lite::future::race(recv, async {
                    async_io::Timer::after(timeout).await;
                    Err(McpError::Timeout {
                        server: self.server(),
                        timeout_ms: timeout.as_millis() as u64,
                    })
                })
                .await
            }
            None => recv.await,
        };

        if result.is_err() {
            self.pending.lock().await.remove(&id);
        } else {
            info!(server = %self.server(), transport = self.kind, method, id, duration_ms = start.elapsed().as_millis() as u64, "MCP response");
        }

        result
    }

    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        let notif = JsonRpcNotification::new(method, params);
        self.write_line(&self.serialize(&notif)?).await
    }

    pub fn kill(&self) {
        self.alive.store(false, Ordering::Release);
    }

    pub fn server_name(&self) -> &Arc<str> {
        &self.name
    }

    async fn write_line(&self, line: &[u8]) -> Result<(), McpError> {
        let mut writer = self.writer.lock().await;
        writer
            .write_all(line)
            .await
            .map_err(|e| McpError::WriteFailed {
                server: self.server(),
                reason: e.to_string(),
            })?;
        writer.flush().await.map_err(|e| McpError::WriteFailed {
            server: self.server(),
            reason: e.to_string(),
        })
    }

    fn server(&self) -> String {
        (*self.name).into()
    }

    fn server_died(&self) -> McpError {
        McpError::ServerDied {
            server: self.server(),
        }
    }

    fn serialize(&self, value: &impl serde::Serialize) -> Result<Vec<u8>, McpError> {
        let mut buf = serde_json::to_vec(value).map_err(|e| McpError::InvalidResponse {
            server: self.server(),
            reason: e.to_string(),
        })?;
        buf.push(LINE_DELIMITER);
        Ok(buf)
    }
}

pub(crate) async fn reader_loop(
    name: &Arc<str>,
    mut reader: impl AsyncBufReadExt + Unpin,
    pending: &Mutex<PendingMap>,
    hub: &NotificationHub,
) -> Result<(), McpError> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| McpError::ServerDied {
                server: format!("{}: read failed: {e}", &**name),
            })?;

        if n == 0 {
            return Err(McpError::ServerDied {
                server: (**name).into(),
            });
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match serde_json::from_str::<IncomingLine>(trimmed) {
            Ok(incoming) => match (incoming.id, incoming.method) {
                (Some(id), None) => {
                    if let Some(sender) = pending.lock().await.remove(&id) {
                        let result = if let Some(err) = incoming.error {
                            Err(McpError::RpcError {
                                server: (**name).into(),
                                code: err.code,
                                message: err.message,
                            })
                        } else {
                            Ok(incoming.result.unwrap_or(Value::Null))
                        };
                        let _ = sender.send(result).await;
                    } else {
                        debug!(server = &**name, id, "response for unknown request id");
                    }
                }
                (None, Some(method)) => hub.route(&method, incoming.params.as_ref()),
                (Some(id), Some(method)) => {
                    // Server-initiated request; sampling and elicitation are
                    // never advertised, so this should not happen.
                    debug!(
                        server = &**name,
                        id, method, "ignoring unsupported server request"
                    );
                }
                (None, None) => {
                    debug!(
                        server = &**name,
                        line = trimmed,
                        "unparseable JSON-RPC line"
                    );
                }
            },
            Err(e) => {
                debug!(server = &**name, error = %e, line = trimmed, "non-JSON-RPC line from server");
            }
        }
    }
}
