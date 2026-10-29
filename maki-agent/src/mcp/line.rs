use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_lite::{AsyncBufReadExt, AsyncWriteExt};
use serde_json::Value;
use smol::channel;
use smol::lock::Mutex;
use tracing::{debug, info, warn};

use super::error::McpError;
use super::protocol::JsonRpcError;
use super::protocol::{JsonRpcNotification, JsonRpcRequest};
use super::transport::CancelNotify;
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
    logs: Arc<std::sync::Mutex<LogWatchers>>,
}

const PENDING_LOG_CAP: usize = 1024;

/// Log watchers plus the lines that reached the hub before any of them: the
/// executor replays its boot records the moment the brain connects, while
/// the brain is still handshaking and has not subscribed yet.
#[derive(Default)]
struct LogWatchers {
    senders: Vec<flume::Sender<String>>,
    pending: VecDeque<String>,
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

    /// `maki/log` notifications from an executor: complete log records, one
    /// per line, for the brain to file into its own log. Lines that arrived
    /// before the first watcher are drained into it, so the replayed boot
    /// records survive the handshake window.
    pub fn watch_logs(&self) -> flume::Receiver<String> {
        let (tx, rx) = flume::unbounded();
        let mut logs = self.logs.lock().unwrap_or_else(|e| e.into_inner());
        for line in logs.pending.drain(..) {
            let _ = tx.try_send(line);
        }
        logs.senders.push(tx);
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
            "maki/log" => {
                let Some(line) = params.and_then(|p| p["line"].as_str()) else {
                    return;
                };
                let mut logs = self.logs.lock().unwrap_or_else(|e| e.into_inner());
                if logs.senders.is_empty() {
                    // The executor already bounded the loss on its side; the
                    // cap here only guards a brain that never subscribes.
                    if logs.pending.len() < PENDING_LOG_CAP {
                        logs.pending.push_back(line.to_owned());
                    }
                    return;
                }
                for tx in logs.senders.iter() {
                    let _ = tx.try_send(line.to_owned());
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
    writer: Arc<Mutex<W>>,
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
            writer: Arc::new(Mutex::new(writer)),
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
        // Armed until a reply for our id lands: a dropped future (the
        // caller's cancel raced us away) or a failed attempt leaves the
        // request running server-side unless we tell the server it was
        // cancelled.
        let mut cancel_notify = self.cancel_notify(id);
        let result = match self.timeout {
            Some(timeout) => {
                futures_lite::future::race(recv, async {
                    smol::Timer::after(timeout).await;
                    Err(McpError::Timeout {
                        server: self.server(),
                        timeout_ms: timeout.as_millis() as u64,
                    })
                })
                .await
            }
            None => recv.await,
        };

        // A reply the server sent for our id ends the request, whatever it
        // says: a cancel after an answer is noise. Everything else keeps the
        // guard armed; a dead reader is skipped inside the fire.
        match &result {
            Ok(_) | Err(McpError::RpcError { .. }) => cancel_notify.disarm(),
            Err(_) => {}
        }
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

    /// This transport's [`CancelNotify`]: one more ndjson line on the
    /// connection's writer, unless the reader has already died.
    fn cancel_notify(&self, id: u64) -> CancelNotify {
        let name = Arc::clone(&self.name);
        let kind = self.kind;
        let writer = Arc::clone(&self.writer);
        let alive = Arc::clone(&self.alive);
        CancelNotify::armed(move || {
            if !alive.load(Ordering::Acquire) {
                return;
            }
            smol::spawn(async move {
                let buf = match serde_json::to_vec(&JsonRpcNotification::cancelled(id)) {
                    Ok(mut buf) => {
                        buf.push(LINE_DELIMITER);
                        buf
                    }
                    Err(e) => {
                        warn!(server = &*name, transport = kind, error = %e, "serialize cancel notification failed");
                        return;
                    }
                };
                let mut writer = writer.lock().await;
                if let Err(e) = writer.write_all(&buf).await {
                    warn!(server = &*name, transport = kind, error = %e, "failed to send cancel notification");
                    return;
                }
                if let Err(e) = writer.flush().await {
                    warn!(server = &*name, transport = kind, error = %e, "failed to flush cancel notification");
                }
            })
            .detach();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maki_log_and_message_route_to_their_own_watchers() {
        let hub = NotificationHub::default();
        let logs = hub.watch_logs();
        let messages = hub.watch_messages();

        hub.route(
            "maki/log",
            Some(&serde_json::json!({ "line": "{\"level\":\"INFO\"}" })),
        );
        hub.route(
            "notifications/message",
            Some(&serde_json::json!({ "level": "info", "data": "flash" })),
        );

        assert_eq!(logs.try_recv().unwrap(), "{\"level\":\"INFO\"}");
        assert_eq!(messages.try_recv().unwrap(), "flash");
        assert!(logs.try_recv().is_err());
        assert!(messages.try_recv().is_err());
    }

    /// The executor's replay arrives while the brain is still handshaking,
    /// before the event loop subscribes; those lines wait for the first
    /// watcher instead of vanishing.
    #[test]
    fn maki_log_lines_arriving_before_any_watcher_are_kept() {
        let hub = NotificationHub::default();
        hub.route("maki/log", Some(&serde_json::json!({ "line": "early" })));
        let logs = hub.watch_logs();
        hub.route("maki/log", Some(&serde_json::json!({ "line": "late" })));

        assert_eq!(logs.try_recv().unwrap(), "early");
        assert_eq!(logs.try_recv().unwrap(), "late");
        assert!(logs.try_recv().is_err());
    }
}
