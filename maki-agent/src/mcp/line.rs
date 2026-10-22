use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_lite::{AsyncBufReadExt, AsyncWriteExt};
use serde_json::Value;
use smol::channel;
use smol::lock::Mutex;
use tracing::{debug, info, warn};

use super::error::McpError;
use super::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use super::transport::CancelNotify;

pub(crate) type PendingMap = HashMap<u64, channel::Sender<Result<Value, McpError>>>;

const LINE_DELIMITER: u8 = b'\n';

/// ndjson JSON-RPC connection shared by the byte-stream transports (stdio, socket).
pub struct LineTransport<W> {
    name: Arc<str>,
    kind: &'static str,
    writer: Arc<Mutex<W>>,
    pending: Arc<Mutex<PendingMap>>,
    next_id: AtomicU64,
    timeout: Duration,
    alive: Arc<AtomicBool>,
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
        timeout: Duration,
    ) -> Self {
        let name: Arc<str> = Arc::from(name);
        let alive = Arc::new(AtomicBool::new(true));
        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));
        let reader_task = {
            let name = Arc::clone(&name);
            let alive = Arc::clone(&alive);
            let pending = Arc::clone(&pending);
            smol::spawn(async move {
                let result = reader_loop(&name, reader, &pending).await;
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
            _reader_task: reader_task,
        }
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
        let result = futures_lite::future::race(recv, async {
            smol::Timer::after(self.timeout).await;
            Err(McpError::Timeout {
                server: self.server(),
                timeout_ms: self.timeout.as_millis() as u64,
            })
        })
        .await;

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

        match serde_json::from_str::<JsonRpcResponse>(trimmed) {
            Ok(resp) => {
                if let Some(id) = resp.id {
                    if let Some(sender) = pending.lock().await.remove(&id) {
                        let result = if let Some(err) = resp.error {
                            Err(McpError::RpcError {
                                server: (**name).into(),
                                code: err.code,
                                message: err.message,
                            })
                        } else {
                            Ok(resp.result.unwrap_or(Value::Null))
                        };
                        let _ = sender.send(result).await;
                    } else {
                        debug!(server = &**name, id, "response for unknown request id");
                    }
                } else {
                    debug!(server = &**name, "received notification (no id)");
                }
            }
            Err(e) => {
                debug!(server = &**name, error = %e, line = trimmed, "non-JSON-RPC line from server");
            }
        }
    }
}
