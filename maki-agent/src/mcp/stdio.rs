use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::sync::Arc;
use std::time::Duration;

use futures_lite::{AsyncBufReadExt, io::BufReader};
use maki_providers::strip_provider_keys;
use serde_json::Value;
use tracing::warn;

use super::error::McpError;
use super::line::LineTransport;
use super::transport::{BoxFuture, McpTransport};

use crate::ChildGuard;

pub struct StdioTransport {
    io: LineTransport<async_process::ChildStdin>,
    _stderr_task: smol::Task<()>,
    _child: ChildGuard,
}

impl StdioTransport {
    pub fn spawn(
        name: &str,
        program: &str,
        args: &[String],
        environment: &HashMap<String, String>,
        timeout: Option<Duration>,
    ) -> Result<Self, McpError> {
        let mut std_cmd = std::process::Command::new(program);
        strip_provider_keys(&mut std_cmd)
            .args(args)
            .envs(environment);

        #[cfg(unix)]
        unsafe {
            std_cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        let mut cmd: async_process::Command = std_cmd.into();
        cmd.stdin(async_process::Stdio::piped())
            .stdout(async_process::Stdio::piped())
            .stderr(async_process::Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| McpError::StartFailed {
            server: name.into(),
            reason: e.to_string(),
        })?;

        let stdin = child.stdin.take().ok_or_else(|| McpError::StartFailed {
            server: name.into(),
            reason: "no stdin".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::StartFailed {
            server: name.into(),
            reason: "no stdout".into(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| McpError::StartFailed {
            server: name.into(),
            reason: "no stderr".into(),
        })?;

        let io = LineTransport::new(name, "stdio", stdin, BufReader::new(stdout), timeout);

        let name: Arc<str> = Arc::from(name);
        let stderr_task = smol::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            warn!(server = &*name, "{trimmed}");
                        }
                    }
                }
            }
        });

        Ok(Self {
            io,
            _stderr_task: stderr_task,
            _child: ChildGuard::new(child),
        })
    }
}

impl McpTransport for StdioTransport {
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
        // Flip `alive` so any in-flight reader or writer gives up with a clean error.
        // We deliberately do not signal the child here: the transport lives behind an
        // Arc, and once the last clone goes away `ChildGuard::drop` takes care of
        // killing the whole process group. Doing it twice just raced with itself.
        Box::pin(async move { self.io.kill() })
    }

    fn server_name(&self) -> &Arc<str> {
        self.io.server_name()
    }

    fn transport_kind(&self) -> &'static str {
        "stdio"
    }

    fn child_pids(&self) -> Vec<u32> {
        vec![self._child.id()]
    }

    fn notification_hub(&self) -> Option<super::line::NotificationHub> {
        Some(self.io.hub())
    }
}

#[cfg(test)]
mod tests {
    use super::super::line::{PendingMap, reader_loop};
    use super::*;
    use async_lock::Mutex;
    use futures_lite::io::Cursor;
    use smol::channel;
    use test_case::test_case;

    async fn read_single_response(input: &str) -> Result<Value, McpError> {
        let pending: Mutex<PendingMap> = Mutex::new(HashMap::new());
        let name: Arc<str> = Arc::from("test");

        let (tx, rx) = channel::bounded(1);
        pending.lock().await.insert(1, tx);

        let mut reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let _ = reader_loop(&name, &mut reader, &pending, &Default::default()).await;

        rx.try_recv().unwrap_or(Err(McpError::ServerDied {
            server: "no response received".into(),
        }))
    }

    #[test_case("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n" ; "lf_terminated")]
    #[test_case("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\r\n" ; "crlf_terminated")]
    #[test_case("  {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}  \n" ; "whitespace_padded")]
    #[test_case("\n\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n" ; "blank_lines_before")]
    #[test_case("not json\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n" ; "invalid_json_before")]
    fn reader_parses_valid_response(input: &str) {
        smol::block_on(async {
            assert!(read_single_response(input).await.is_ok());
        });
    }

    #[test]
    fn reader_returns_rpc_error() {
        let input =
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32600,\"message\":\"bad\"}}\n";
        smol::block_on(async {
            assert!(matches!(
                read_single_response(input).await,
                Err(McpError::RpcError { code: -32600, .. })
            ));
        });
    }
}
