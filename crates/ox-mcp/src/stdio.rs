//! stdio transport: one JSON-RPC message per line over a child process's
//! stdin/stdout.
//!
//! Architecture matches `src/cbm/mod.rs`: writers go through a channel so a
//! slow pipe can never block the reader, stdout is drained on its own task,
//! stderr is drained and logged (never parsed), and cancellation sends the
//! MCP `notifications/cancelled` before returning.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::jsonrpc::{self, Incoming, Notification, Request};
use super::transport::{CallError, ProtocolMode, RequestMetadata, Rpc, modern_params};

type Pending = HashMap<u64, oneshot::Sender<Result<Value, CallError>>>;

const WRITER_QUEUE_CAPACITY: usize = 64;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

fn writer_channel() -> (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) {
    mpsc::channel(WRITER_QUEUE_CAPACITY)
}

async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Ok(Some(frame))
            };
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let part_len = newline.map_or(buffer.len(), |index| index + 1);
        if frame.len() + part_len > MAX_FRAME_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "MCP stdio frame exceeds 16 MiB",
            ));
        }
        frame.extend_from_slice(&buffer[..part_len]);
        reader.consume(part_len);
        if newline.is_some() {
            return Ok(Some(frame));
        }
    }
}

pub(crate) struct StdioRpc {
    writer: mpsc::Sender<Vec<u8>>,
    pending: Arc<Mutex<Pending>>,
    protocol: Mutex<ProtocolMode>,
    next_id: AtomicU64,
    child: Arc<Mutex<Option<Child>>>,
    reader: JoinHandle<()>,
    writer_task: JoinHandle<()>,
    stderr: JoinHandle<()>,
    closed: Arc<AtomicBool>,
}

impl StdioRpc {
    /// Spawn `command` (argv, no shell) with the inherited environment plus
    /// `environment`, in `cwd` when given.
    pub(crate) fn spawn(
        command: &[String],
        environment: &BTreeMap<String, String>,
        cwd: Option<&Path>,
    ) -> std::io::Result<Self> {
        let Some(program) = command.first() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty MCP command",
            ));
        };
        let mut child = Command::new(program);
        child
            .args(&command[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            child.current_dir(cwd);
        }
        for (key, value) in environment {
            child.env(key, value);
        }
        let mut child = child.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("MCP child has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("MCP child has no stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("MCP child has no stderr"))?;

        let (writer, writer_rx) = writer_channel();
        let (server_writer, server_writer_rx) = writer_channel();
        let pending: Arc<Mutex<Pending>> = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));

        let writer_pending = Arc::clone(&pending);
        let writer_closed = Arc::clone(&closed);
        let writer_task = tokio::spawn(async move {
            let mut stdin = stdin;
            let mut writer_rx = writer_rx;
            let mut server_writer_rx = server_writer_rx;
            loop {
                let bytes = tokio::select! {
                    biased;
                    bytes = server_writer_rx.recv() => bytes,
                    bytes = writer_rx.recv() => bytes,
                    else => None,
                };
                let Some(bytes) = bytes else { break };
                if stdin.write_all(&bytes).await.is_err() {
                    writer_closed.store(true, Ordering::SeqCst);
                    for (_, sender) in writer_pending.lock().unwrap().drain() {
                        let _ = sender.send(Err(CallError::Closed));
                    }
                    break;
                }
            }
        });

        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(&mut stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(line = %line, "MCP server stderr");
            }
        });

        let reader_bus = server_writer.clone();
        let reader_pending = Arc::clone(&pending);
        let reader_closed = Arc::clone(&closed);
        let reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(frame)) = read_frame(&mut reader).await {
                let line = frame.strip_suffix(b"\n").unwrap_or(&frame);
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                let line = line.trim_ascii();
                if line.is_empty() {
                    continue;
                }
                let message: Incoming = match serde_json::from_slice(line) {
                    Ok(message) => message,
                    Err(error) => {
                        tracing::warn!(%error, "MCP server sent malformed JSON-RPC");
                        continue;
                    }
                };
                let Incoming {
                    id,
                    method,
                    result,
                    error,
                } = message;
                match (method, id) {
                    // Server → client request: ox implements none of the
                    // client-side capabilities; answer so the server does
                    // not wait forever.
                    (Some(method), Some(id)) => {
                        let reply = jsonrpc::error_response(
                            &id,
                            jsonrpc::METHOD_NOT_FOUND,
                            &format!("ox does not implement `{method}`"),
                        );
                        // A full queue must not stop the reader from
                        // correlating ordinary responses. The server request
                        // is unsupported, so dropping this best-effort error
                        // is safer than deadlocking the transport.
                        if reader_bus.try_send(jsonrpc::encode(&reply)).is_err() {
                            tracing::warn!(
                                "MCP writer queue full; dropped unsupported server-request reply"
                            );
                        }
                    }
                    // Server notification (list_changed, logging, ...):
                    // ignored in v1; the catalog is read at startup.
                    (Some(method), None) => {
                        tracing::trace!(method = %method, "MCP server notification");
                    }
                    // Response: correlate by numeric id.
                    (None, Some(id)) => {
                        let Some(id) = id.as_u64() else {
                            continue;
                        };
                        let Some(sender) = reader_pending.lock().unwrap().remove(&id) else {
                            continue;
                        };
                        let outcome = match error {
                            Some(error) => Err(CallError::Rpc(error)),
                            None => Ok(result.unwrap_or(Value::Null)),
                        };
                        let _ = sender.send(outcome);
                    }
                    (None, None) => {}
                }
            }
            // EOF means the server is gone: mark the transport closed under
            // the pending lock so a request cannot slip in between the flag
            // and the drain and then wait forever for an answer.
            let mut pending = reader_pending.lock().unwrap();
            reader_closed.store(true, Ordering::SeqCst);
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err(CallError::Closed));
            }
        });

        Ok(Self {
            writer,
            pending,
            protocol: Mutex::new(ProtocolMode::Legacy),
            next_id: AtomicU64::new(1),
            child: Arc::new(Mutex::new(Some(child))),
            reader,
            writer_task,
            stderr: stderr_task,
            closed,
        })
    }

    fn send_cancel(&self, id: u64) {
        let frame = jsonrpc::encode(&Notification {
            jsonrpc: "2.0",
            method: "notifications/cancelled".to_owned(),
            params: Some(json!({ "requestId": id, "reason": "cancelled" })),
        });
        let _ = self.writer.try_send(frame);
    }
}

/// `None` deadline means sleep forever, which is what a `select!` arm wants.
async fn sleep_until_optional(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

enum Wait {
    Done(Result<Result<Value, CallError>, oneshot::error::RecvError>),
    Cancelled,
    TimedOut,
}

impl Rpc for StdioRpc {
    fn set_protocol(&self, protocol: ProtocolMode) {
        *self.protocol.lock().unwrap() = protocol;
    }

    fn request<'a>(
        &'a self,
        metadata: RequestMetadata<'a>,
        params: Option<Value>,
        timeout: Option<std::time::Duration>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Value, CallError>> {
        Box::pin(async move {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let (sender, receiver) = oneshot::channel();
            {
                let mut pending = self.pending.lock().unwrap();
                // Checked while holding the lock the reader takes before it
                // marks the transport closed, so a dead server can never
                // leave a call waiting for a reply that cannot arrive.
                if self.closed.load(Ordering::SeqCst) {
                    return Err(CallError::Closed);
                }
                pending.insert(id, sender);
            }
            let params = match *self.protocol.lock().unwrap() {
                ProtocolMode::Legacy => params,
                ProtocolMode::Modern => modern_params(metadata, params),
            };
            let deadline = timeout.map(|limit| tokio::time::Instant::now() + limit);
            let frame = jsonrpc::encode(&Request {
                jsonrpc: "2.0",
                id,
                method: metadata.method.to_owned(),
                params,
            });
            let send = self.writer.send(frame);
            tokio::pin!(send);
            let send_result = tokio::select! {
                result = &mut send => result,
                () = cancel.cancelled() => {
                    self.pending.lock().unwrap().remove(&id);
                    self.send_cancel(id);
                    return Err(CallError::Cancelled);
                }
                () = sleep_until_optional(deadline) => {
                    self.pending.lock().unwrap().remove(&id);
                    self.send_cancel(id);
                    return Err(CallError::Timeout);
                }
            };
            if send_result.is_err() {
                self.pending.lock().unwrap().remove(&id);
                return Err(CallError::Closed);
            }
            let outcome = tokio::select! {
                result = receiver => Wait::Done(result),
                () = cancel.cancelled() => Wait::Cancelled,
                () = sleep_until_optional(deadline) => Wait::TimedOut,
            };
            match outcome {
                Wait::Done(Ok(result)) => result,
                Wait::Done(Err(_)) => Err(CallError::Closed),
                Wait::Cancelled => {
                    self.pending.lock().unwrap().remove(&id);
                    self.send_cancel(id);
                    Err(CallError::Cancelled)
                }
                Wait::TimedOut => {
                    self.pending.lock().unwrap().remove(&id);
                    self.send_cancel(id);
                    Err(CallError::Timeout)
                }
            }
        })
    }

    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move {
            if self.closed.load(Ordering::SeqCst) {
                return Err(CallError::Closed);
            }
            let frame = jsonrpc::encode(&Notification {
                jsonrpc: "2.0",
                method: method.to_owned(),
                params,
            });
            self.writer.send(frame).await.map_err(|_| CallError::Closed)
        })
    }

    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.closed.store(true, Ordering::SeqCst);
            let child = self.child.lock().unwrap().take();
            if let Some(mut child) = child {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            self.reader.abort();
            self.writer_task.abort();
            self.stderr.abort();
            for (_, sender) in self.pending.lock().unwrap().drain() {
                let _ = sender.send(Err(CallError::Closed));
            }
        })
    }
}

impl Drop for StdioRpc {
    fn drop(&mut self) {
        // `kill_on_drop` already covers the child; this makes the intent
        // explicit and reaps the process when the last tool handle goes away.
        let child = self.child.lock().unwrap().take();
        if let Some(mut child) = child {
            let _ = child.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::ProtocolMode;
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    #[tokio::test]
    async fn writer_queue_is_bounded() {
        let (writer, _receiver) = writer_channel();
        for _ in 0..WRITER_QUEUE_CAPACITY {
            writer.try_send(Vec::new()).unwrap();
        }
        assert!(writer.try_send(Vec::new()).is_err());
    }

    #[tokio::test]
    async fn writer_failure_closes_requests_without_deadline() {
        let command = vec!["sh".to_owned(), "-c".to_owned(), "exit 0".to_owned()];
        let rpc = StdioRpc::spawn(&command, &BTreeMap::new(), None).unwrap();
        let cancel = CancellationToken::new();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            rpc.request(RequestMetadata::new("tools/list"), None, None, &cancel),
        )
        .await
        .expect("writer failure must wake the request");
        assert!(matches!(result, Err(CallError::Closed)));
        rpc.shutdown().await;
    }

    #[tokio::test]
    async fn modern_requests_add_metadata_and_legacy_body_is_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("server.sh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"tools/list"'*)
      printf '%s' "$line" | grep -q '"_meta"' || exit 11
      printf '%s' "$line" | grep -q '"protocolVersion":"2026-07-28"' || exit 11
      printf '%s' "$line" | grep -q '"clientInfo":{"name":"ox"' || exit 11
      printf '%s' "$line" | grep -q '"clientCapabilities":{}' || exit 11
      ;;
    *'"method":"tools/call"'*)
      case "$line" in
        *'"_meta"'*)
          printf '%s' "$line" | grep -q '"protocolVersion":"2026-07-28"' || exit 12
          printf '%s' "$line" | grep -q '"clientInfo":{"name":"ox"' || exit 12
          printf '%s' "$line" | grep -q '"clientCapabilities":{}' || exit 12
          printf '%s' "$line" | grep -q '"toolName":"echo"' || exit 12
          ;;
        *'"params":{"name":"echo","arguments":{}}}'*) ;;
        *) exit 12 ;;
      esac
      ;;
    *)
      case "$line" in
        *'"_meta"'*) exit 13 ;;
      esac
      ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
done
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let command = vec![script.display().to_string()];
        let rpc = StdioRpc::spawn(&command, &BTreeMap::new(), None).unwrap();
        let cancel = CancellationToken::new();

        rpc.set_protocol(ProtocolMode::Modern);
        rpc.request(
            RequestMetadata::new("tools/list"),
            None,
            Some(Duration::from_secs(1)),
            &cancel,
        )
        .await
        .unwrap();
        rpc.request(
            RequestMetadata::new("tools/call").tool("echo"),
            Some(json!({ "name": "echo", "arguments": {} })),
            Some(Duration::from_secs(1)),
            &cancel,
        )
        .await
        .unwrap();

        rpc.set_protocol(ProtocolMode::Legacy);
        let result = rpc
            .request(
                RequestMetadata::new("tools/call").tool("echo"),
                Some(json!({ "name": "echo", "arguments": {} })),
                Some(Duration::from_secs(1)),
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(result, json!({ "ok": true }));
        rpc.shutdown().await;
    }

    #[tokio::test]
    async fn server_request_error_does_not_block_following_response() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("server.sh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  printf '%s\n' '{"jsonrpc":"2.0","id":99,"method":"sampling/createMessage","params":{}}'
  printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
done
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let command = vec![script.display().to_string()];
        let rpc = StdioRpc::spawn(&command, &BTreeMap::new(), None).unwrap();
        for protocol in [ProtocolMode::Legacy, ProtocolMode::Modern] {
            rpc.set_protocol(protocol);
            let cancel = CancellationToken::new();
            let result = rpc
                .request(
                    RequestMetadata::new("tools/list"),
                    None,
                    Some(Duration::from_secs(1)),
                    &cancel,
                )
                .await
                .unwrap();
            assert_eq!(result, json!({ "ok": true }));
        }
        rpc.shutdown().await;
    }
}
