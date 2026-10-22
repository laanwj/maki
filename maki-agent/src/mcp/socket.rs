use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_net::unix::UnixStream;
use futures_lite::io::BufReader;
use serde_json::Value;

use super::error::McpError;
use super::line::LineTransport;
use super::transport::{BoxFuture, McpTransport};

pub struct SocketTransport {
    io: LineTransport<UnixStream>,
}

impl SocketTransport {
    pub async fn connect(name: &str, path: &Path, timeout: Duration) -> Result<Self, McpError> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|e| McpError::StartFailed {
                server: name.into(),
                reason: format!("connect {}: {e}", path.display()),
            })?;
        let io = LineTransport::new(
            name,
            "socket",
            stream.clone(),
            BufReader::new(stream),
            timeout,
        );
        Ok(Self { io })
    }
}

impl McpTransport for SocketTransport {
    fn send_request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(self.io.request(method, params))
    }

    fn send_notification<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(self.io.notify(method, params))
    }

    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move { self.io.kill() })
    }

    fn server_name(&self) -> &Arc<str> {
        self.io.server_name()
    }

    fn transport_kind(&self) -> &'static str {
        "socket"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_net::unix::UnixListener;
    use futures_lite::{AsyncBufReadExt, AsyncWriteExt};
    use serde_json::json;
    use smol::channel;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn spawn_server(
        dir: &tempfile::TempDir,
        handler: impl FnOnce(UnixStream) -> smol::Task<()> + Send + 'static,
    ) -> (std::path::PathBuf, UnixListener) {
        let path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let handler_listener = listener.clone();
        smol::spawn(async move {
            let (stream, _) = handler_listener.accept().await.expect("accept");
            handler(stream).await
        })
        .detach();
        (path, listener)
    }

    fn request_response() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let (path, _listener) = spawn_server(&dir, |mut stream| {
                smol::spawn(async move {
                    let mut reader = BufReader::new(stream.clone());
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    let msg: Value = serde_json::from_str(line.trim()).unwrap();
                    let id = msg["id"].as_u64().unwrap();
                    let response = json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}});
                    let mut buf = serde_json::to_vec(&response).unwrap();
                    buf.push(b'\n');
                    stream.write_all(&buf).await.unwrap();
                    stream.flush().await.unwrap();
                })
            });
            let transport = SocketTransport::connect("test", &path, TIMEOUT)
                .await
                .unwrap();
            let result = transport
                .send_request("tools/list", None)
                .await
                .expect("request failed");
            assert_eq!(result, json!({"ok": true}));
        });
    }

    fn notification_forwarded() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let (tx, rx) = channel::bounded::<Value>(1);
            let (path, _listener) = spawn_server(&dir, move |stream| {
                smol::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    tx.send(serde_json::from_str(line.trim()).unwrap())
                        .await
                        .unwrap();
                })
            });
            let transport = SocketTransport::connect("test", &path, TIMEOUT)
                .await
                .unwrap();
            transport
                .send_notification("notifications/initialized", None)
                .await
                .unwrap();
            let received = rx.recv().await.unwrap();
            assert_eq!(received["method"], "notifications/initialized");
            assert!(received.get("id").is_none());
        });
    }

    fn timeout_when_server_silent() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let (path, _listener) = spawn_server(&dir, |stream| {
                smol::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    let _ = reader.read_line(&mut line).await;
                    std::future::pending::<()>().await;
                })
            });
            let transport = SocketTransport::connect("test", &path, Duration::from_millis(100))
                .await
                .unwrap();
            let result = transport.send_request("tools/list", None).await;
            assert!(matches!(result, Err(McpError::Timeout { .. })));
        });
    }

    /// An abandoned request must tell the server: the caller's cancel races
    /// the call away brain-side, and without a `notifications/cancelled` the
    /// server runs the call to completion.
    fn abandoned_request_sends_cancel_notification() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let (tx, rx) = channel::bounded::<Value>(2);
            let (path, _listener) = spawn_server(&dir, move |stream| {
                let tx = tx.clone();
                smol::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    for _ in 0..2 {
                        reader.read_line(&mut line).await.unwrap();
                        tx.send(serde_json::from_str(line.trim()).unwrap())
                            .await
                            .unwrap();
                        line.clear();
                    }
                    std::future::pending::<()>().await;
                })
            });
            let transport = SocketTransport::connect("test", &path, TIMEOUT)
                .await
                .unwrap();
            let (trigger, token) = crate::cancel::CancelToken::new();
            let call = token.race(transport.send_request("tools/call", None));
            smol::spawn(async move {
                smol::Timer::after(Duration::from_millis(100)).await;
                trigger.cancel();
            })
            .detach();
            let result = call.await;
            assert!(result.is_err(), "raced request should report cancelled");
            let request_line = rx.recv().await.unwrap();
            // Bounded, so a transport that loses its cancel guard fails here
            // instead of parking until the test harness gives up.
            let cancel_line = futures_lite::future::or(async { rx.recv().await.unwrap() }, async {
                smol::Timer::after(Duration::from_secs(5)).await;
                panic!("cancel notification never arrived");
            })
            .await;
            let id = request_line["id"].as_u64().unwrap();
            assert_eq!(request_line["method"], "tools/call");
            assert_eq!(cancel_line["method"], "notifications/cancelled");
            assert_eq!(cancel_line["params"]["requestId"], id);
        });
    }

    fn server_died_on_close() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let (path, _listener) = spawn_server(&dir, |stream| {
                smol::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    let _ = reader.read_line(&mut line).await;
                })
            });
            let transport = SocketTransport::connect("test", &path, TIMEOUT)
                .await
                .unwrap();
            let result = transport.send_request("tools/list", None).await;
            assert!(matches!(result, Err(McpError::ServerDied { .. })));
        });
    }

    #[test]
    fn socket_transport() {
        request_response();
        notification_forwarded();
        timeout_when_server_silent();
        server_died_on_close();
        abandoned_request_sends_cancel_notification();
    }
}
