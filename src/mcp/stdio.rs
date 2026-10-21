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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::jsonrpc::{self, Incoming, Notification, Request};
use super::transport::{CallError, Rpc};

type Pending = HashMap<u64, oneshot::Sender<Result<Value, CallError>>>;

pub(crate) struct StdioRpc {
    writer: mpsc::UnboundedSender<Vec<u8>>,
    pending: Arc<Mutex<Pending>>,
    next_id: AtomicU64,
    child: Arc<Mutex<Option<Child>>>,
    reader: JoinHandle<()>,
    writer_task: JoinHandle<()>,
    stderr: JoinHandle<()>,
    closed: AtomicBool,
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

        let (writer, writer_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let pending: Arc<Mutex<Pending>> = Arc::new(Mutex::new(HashMap::new()));

        let writer_task = tokio::spawn(async move {
            let mut stdin = stdin;
            let mut writer_rx = writer_rx;
            while let Some(bytes) = writer_rx.recv().await {
                if stdin.write_all(&bytes).await.is_err() {
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

        let reader_bus = writer.clone();
        let reader_pending = Arc::clone(&pending);
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let message: Incoming = match serde_json::from_str(line) {
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
                        let _ = reader_bus.send(jsonrpc::encode(&reply));
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
            for (_, sender) in reader_pending.lock().unwrap().drain() {
                let _ = sender.send(Err(CallError::Closed));
            }
        });

        Ok(Self {
            writer,
            pending,
            next_id: AtomicU64::new(1),
            child: Arc::new(Mutex::new(Some(child))),
            reader,
            writer_task,
            stderr: stderr_task,
            closed: AtomicBool::new(false),
        })
    }

    fn send_cancel(&self, id: u64) {
        let frame = jsonrpc::encode(&Notification {
            jsonrpc: "2.0",
            method: "notifications/cancelled".to_owned(),
            params: Some(json!({ "requestId": id, "reason": "cancelled" })),
        });
        let _ = self.writer.send(frame);
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
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        timeout: Option<std::time::Duration>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Value, CallError>> {
        Box::pin(async move {
            if self.closed.load(Ordering::SeqCst) {
                return Err(CallError::Closed);
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let (sender, receiver) = oneshot::channel();
            self.pending.lock().unwrap().insert(id, sender);
            let frame = jsonrpc::encode(&Request {
                jsonrpc: "2.0",
                id,
                method: method.to_owned(),
                params,
            });
            if self.writer.send(frame).is_err() {
                self.pending.lock().unwrap().remove(&id);
                return Err(CallError::Closed);
            }
            let deadline = timeout.map(|limit| tokio::time::Instant::now() + limit);
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
            let frame = jsonrpc::encode(&Notification {
                jsonrpc: "2.0",
                method: method.to_owned(),
                params,
            });
            self.writer.send(frame).map_err(|_| CallError::Closed)
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
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    /// Answers initialize / tools/list and ignores everything else.
    const STUB: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"stub","version":"0"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
    *'"method":"ping"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
    *'"method":"slow"'*)
      sleep 2
      printf '{"jsonrpc":"2.0","id":%s,"result":{"late":true}}\n' "$id"
      ;;
    *'"method":"server/ask"'*)
      # Server → client request; the client must answer with -32601.
      printf '{"jsonrpc":"2.0","id":"srv-1","method":"sampling/createMessage","params":{}}\n'
      ;;
  esac
done
"#;

    fn script(directory: &std::path::Path) -> Vec<String> {
        let path = directory.join("server.sh");
        std::fs::write(&path, STUB).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        vec![path.display().to_string()]
    }

    /// Child startup and shell scheduling are not instant on a loaded CI
    /// machine; a fixed sleep is a flaky race. Poll the file the server
    /// writes instead.
    async fn wait_for_file(path: &std::path::Path) -> String {
        for _ in 0..100 {
            if let Ok(contents) = std::fs::read_to_string(path)
                && !contents.is_empty()
            {
                return contents;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no content appeared at {}", path.display());
    }

    #[tokio::test]
    async fn request_round_trips_over_stdio() {
        let directory = tempfile::tempdir().unwrap();
        let rpc = StdioRpc::spawn(&script(directory.path()), &BTreeMap::new(), None).unwrap();
        let cancel = CancellationToken::new();
        let value = rpc
            .request(
                "initialize",
                Some(json!({})),
                Some(Duration::from_secs(5)),
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(value["serverInfo"]["name"], "stub");
        let tools = rpc
            .request("tools/list", None, Some(Duration::from_secs(5)), &cancel)
            .await
            .unwrap();
        assert_eq!(tools["tools"][0]["name"], "echo");
        rpc.shutdown().await;
    }

    #[tokio::test]
    async fn timeout_and_cancellation_release_the_call() {
        let directory = tempfile::tempdir().unwrap();
        let rpc = StdioRpc::spawn(&script(directory.path()), &BTreeMap::new(), None).unwrap();

        let cancel = CancellationToken::new();
        let timed = rpc
            .request("slow", None, Some(Duration::from_millis(50)), &cancel)
            .await;
        assert!(matches!(timed, Err(CallError::Timeout)));

        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let call = async { rpc.request("slow", None, None, &cancel).await };
        let interrupt = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            token.cancel();
        };
        let (result, ()) = tokio::join!(call, interrupt);
        assert!(matches!(result, Err(CallError::Cancelled)));
        rpc.shutdown().await;
    }

    #[tokio::test]
    async fn dead_server_fails_in_flight_calls() {
        let directory = tempfile::tempdir().unwrap();
        // Exits immediately: the request must observe Closed, not hang.
        let path = directory.path().join("dead.sh");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let rpc = StdioRpc::spawn(&[path.display().to_string()], &BTreeMap::new(), None).unwrap();
        let cancel = CancellationToken::new();
        let result = rpc
            .request(
                "initialize",
                Some(json!({})),
                Some(Duration::from_secs(2)),
                &cancel,
            )
            .await;
        assert!(matches!(result, Err(CallError::Closed)));
        rpc.shutdown().await;
    }

    #[tokio::test]
    async fn server_requests_are_rejected_with_method_not_found() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reply.sh");
        // Writes the client's reply to a file, then stays alive.
        std::fs::write(
            &path,
            r#"#!/bin/sh
printf '{"jsonrpc":"2.0","id":"srv-1","method":"sampling/createMessage","params":{}}\n'
while IFS= read -r line; do
  printf '%s\n' "$line" > reply.ndjson
done
"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let rpc = StdioRpc::spawn(
            &[path.display().to_string()],
            &BTreeMap::new(),
            Some(directory.path()),
        )
        .unwrap();
        let reply = wait_for_file(&directory.path().join("reply.ndjson")).await;
        assert!(reply.contains("-32601"), "reply was {reply}");
        assert!(reply.contains("srv-1"), "reply was {reply}");
        rpc.shutdown().await;
    }
}
