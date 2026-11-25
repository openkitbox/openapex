//! Sandboxed execution of a scanned Code Mode program.

use std::{
    collections::{BTreeMap, BTreeSet},
    mem,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError, Sender},
    },
    time::{Duration, Instant},
};

use anyhow::{Result as AnyResult, anyhow};
use rquickjs::{
    Coerced, Context as QContext, Ctx, FromJs, Function, Runtime, Value,
    promise::{MaybePromise, PromiseState},
};

use crate::{CallEvent, CallStatus, Completion, Limits, ToolCallRecord, ToolInvoker, scan};

/// One completion delivered from a host task to the VM thread.
type CompletionMessage = (u64, Result<String, String>);

/// Bookkeeping for a tool call whose promise has not been settled yet.
struct PendingCall {
    path: String,
    args: String,
}

type PendingCalls = Mutex<BTreeMap<u64, PendingCall>>;

/// A bounded wait slice: the host may stay in flight indefinitely, so the VM
/// must wake often enough to re-check its wall-clock deadline.
const WAIT_SLICE: Duration = Duration::from_millis(25);

/// Strip every route from source text to executable code and lock down the
/// prototypes shared by built-ins. Adapted from the sandbox hardening used by
/// nsh's embedded QuickJS; `tools` is frozen here as well so a script cannot
/// replace an authorized leaf with a wrapper of its own.
const HARDEN: &str = r#"
(() => {
  const deny = (label) => function () {
    throw new TypeError(label + " is disabled in code actions");
  };
  const replace = (fn, value) => {
    Object.defineProperty(Object.getPrototypeOf(fn), "constructor", {
      value, writable: false, enumerable: false, configurable: false,
    });
  };
  const blocked = deny("the Function constructor");
  blocked.prototype = Function.prototype;
  replace(function () {}, blocked);
  replace(async function () {}, deny("the AsyncFunction constructor"));
  replace(function* () {}, deny("the GeneratorFunction constructor"));
  replace(async function* () {}, deny("the AsyncGeneratorFunction constructor"));
  Object.defineProperty(globalThis, "Function", {
    value: blocked, writable: false, configurable: false,
  });
  for (const name of ["eval", "fetch", "Bun", "require", "process",
                      "setTimeout", "setInterval", "importScripts", "WebSocket"]) {
    delete globalThis[name];
  }
  const tamed = [
    "constructor", "toString", "toLocaleString", "valueOf", "hasOwnProperty",
    "isPrototypeOf", "propertyIsEnumerable", "toJSON",
  ];
  const tame = (proto) => {
    for (const name of tamed) {
      const d = Object.getOwnPropertyDescriptor(proto, name);
      if (!d || !("value" in d) || !d.writable) continue;
      const value = d.value;
      Object.defineProperty(proto, name, {
        get() { return value; },
        set(v) {
          if (this === proto) throw new TypeError("Cannot assign to read only property '" + name + "'");
          Object.defineProperty(this, name, {
            value: v, writable: true, enumerable: true, configurable: true,
          });
        },
        enumerable: d.enumerable,
        configurable: false,
      });
    }
  };
  for (const proto of [
    Object.prototype, Array.prototype, Function.prototype, String.prototype, Number.prototype,
    Boolean.prototype, Symbol.prototype, RegExp.prototype, Promise.prototype, Map.prototype,
    Set.prototype, WeakMap.prototype, WeakSet.prototype, Date.prototype,
    Object.getPrototypeOf(Uint8Array.prototype),
  ]) {
    tame(proto);
    Object.freeze(proto);
  }
  const freeze = (value) => {
    if (value && typeof value === "object") {
      Object.values(value).forEach(freeze);
      Object.freeze(value);
    }
  };
  if (globalThis.tools) freeze(globalThis.tools);
})();
"#;

/// Why a program did not produce a result.
#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    /// The static scanner rejected the source; nothing was executed.
    #[error("code rejected by scan: {0}")]
    Scan(String),
    /// The wall-clock deadline (or the post-run deadline check) fired.
    #[error("script exceeded its {0:?} deadline")]
    Timeout(Duration),
    /// A runtime, bridge or decoding failure.
    #[error("code execution failed: {0}")]
    Runtime(String),
}

/// What a completed run delivered to the host.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeOutput {
    /// Captured `console.log` output, at most `Limits::output_bytes`.
    pub logs: String,
    /// Whether log output was cut to fit the budget.
    pub truncated: bool,
    /// JSON program result, decoded without imposing an automation contract.
    pub result: Option<serde_json::Value>,
    /// Final record for each tool call, in invocation order.
    pub calls: Vec<ToolCallRecord>,
}

/// Scan then execute `code` in a fresh QuickJS runtime.
pub fn execute(
    code: &str,
    bindings: &str,
    invoker: Arc<dyn ToolInvoker>,
    limits: &Limits,
) -> Result<CodeOutput, ExecutionError> {
    execute_impl(code, None, bindings, invoker, limits)
}

/// Execute with an additional authorization filter. The scanner still runs
/// first; a path it did not see cannot be made callable by the filter.
pub fn execute_scoped(
    code: &str,
    allowed: &BTreeSet<String>,
    bindings: &str,
    invoker: Arc<dyn ToolInvoker>,
    limits: &Limits,
) -> Result<CodeOutput, ExecutionError> {
    execute_impl(code, Some(allowed), bindings, invoker, limits)
}

fn execute_impl(
    code: &str,
    allowed: Option<&BTreeSet<String>>,
    bindings: &str,
    invoker: Arc<dyn ToolInvoker>,
    limits: &Limits,
) -> Result<CodeOutput, ExecutionError> {
    let mut scanned = scan::scan(code).map_err(|err| ExecutionError::Scan(err.to_string()))?;
    if let Some(allowed) = allowed {
        scanned.tools.retain(|path| allowed.contains(path));
    }
    let parsed_bindings: serde_json::Value =
        serde_json::from_str(bindings).map_err(|err| ExecutionError::Runtime(err.to_string()))?;
    if !parsed_bindings.is_object() {
        return Err(ExecutionError::Runtime(
            "bindings must be a JSON object".to_owned(),
        ));
    }

    let deadline = Instant::now() + limits.timeout;
    let interrupted = Arc::new(AtomicBool::new(false));
    let runtime =
        Runtime::new().map_err(|err| ExecutionError::Runtime(format!("create runtime: {err}")))?;
    runtime.set_memory_limit(limits.memory_bytes);
    runtime.set_max_stack_size(limits.stack_bytes);
    let trip = interrupted.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        let expired = Instant::now() >= deadline;
        if expired {
            trip.store(true, Ordering::Relaxed);
        }
        expired
    })));
    let context = QContext::full(&runtime)
        .map_err(|err| ExecutionError::Runtime(format!("create context: {err}")))?;
    let logs = Arc::new(Mutex::new(String::new()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let pending: Arc<PendingCalls> = Arc::new(Mutex::new(BTreeMap::new()));
    let (tx, rx) = mpsc::channel::<CompletionMessage>();

    let outcome: Result<Option<serde_json::Value>, ExecutionError> = context.with(|ctx| {
        install_bindings(&ctx, bindings).map_err(|err| ExecutionError::Runtime(err.to_string()))?;
        let settle = install_globals(
            &ctx,
            &scanned,
            invoker.clone(),
            Arc::clone(&logs),
            limits.output_bytes,
            tx.clone(),
            Arc::clone(&pending),
        )
        .map_err(|err| ExecutionError::Runtime(err.to_string()))?;
        ctx.eval::<(), _>(HARDEN)
            .map_err(|err| ExecutionError::Runtime(format!("harden globals: {err}")))?;

        let program: Function<'_> = ctx
            .eval(scan::wrap(code))
            .map_err(|err| ExecutionError::Runtime(format!("compile program: {err}")))?;
        let returned: Value<'_> = program
            .call(())
            .map_err(|err| ExecutionError::Runtime(format!("start program: {err}")))?;
        let promise = MaybePromise::from_value(returned);
        let mut tool_error: Option<String> = None;

        loop {
            if expired(&interrupted, deadline) {
                abandon_pending(&pending, invoker.as_ref(), &calls);
                return Err(ExecutionError::Timeout(limits.timeout));
            }
            while ctx.execute_pending_job() {
                if expired(&interrupted, deadline) {
                    abandon_pending(&pending, invoker.as_ref(), &calls);
                    return Err(ExecutionError::Timeout(limits.timeout));
                }
            }
            while let Ok(message) = rx.try_recv() {
                if let Some(error) =
                    settle_message(&settle, &pending, invoker.as_ref(), &calls, message)
                        .map_err(|err| ExecutionError::Runtime(err.to_string()))?
                    && tool_error.is_none()
                {
                    tool_error = Some(error);
                }
                if expired(&interrupted, deadline) {
                    abandon_pending(&pending, invoker.as_ref(), &calls);
                    return Err(ExecutionError::Timeout(limits.timeout));
                }
            }
            match promise.state() {
                PromiseState::Pending => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        abandon_pending(&pending, invoker.as_ref(), &calls);
                        return Err(ExecutionError::Timeout(limits.timeout));
                    }
                    match rx.recv_timeout(remaining.min(WAIT_SLICE)) {
                        Ok(message) => {
                            if let Some(error) =
                                settle_message(&settle, &pending, invoker.as_ref(), &calls, message)
                                    .map_err(|err| ExecutionError::Runtime(err.to_string()))?
                                && tool_error.is_none()
                            {
                                tool_error = Some(error);
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => {
                            abandon_pending(&pending, invoker.as_ref(), &calls);
                            return Err(ExecutionError::Runtime(
                                "tool completion channel closed while the program was pending"
                                    .to_owned(),
                            ));
                        }
                    }
                }
                PromiseState::Rejected => {
                    let reason = rejection_message(&ctx, &promise);
                    abandon_pending(&pending, invoker.as_ref(), &calls);
                    return Err(ExecutionError::Runtime(reason));
                }
                PromiseState::Resolved => {
                    let value: Value<'_> = match promise.finish() {
                        Ok(value) => value,
                        Err(err) => {
                            abandon_pending(&pending, invoker.as_ref(), &calls);
                            return Err(ExecutionError::Runtime(format!("resolve program: {err}")));
                        }
                    };
                    if expired(&interrupted, deadline) {
                        abandon_pending(&pending, invoker.as_ref(), &calls);
                        return Err(ExecutionError::Timeout(limits.timeout));
                    }
                    if let Some(error) = tool_error {
                        abandon_pending(&pending, invoker.as_ref(), &calls);
                        return Err(ExecutionError::Runtime(error));
                    }
                    let decoded = decode_result(&ctx, value)
                        .map_err(|err| ExecutionError::Runtime(err.to_string()));
                    // A script need not await every call it starts. Close any
                    // still-in-flight ticket before returning, on both the
                    // success and the decode-failure path.
                    abandon_pending(&pending, invoker.as_ref(), &calls);
                    return decoded;
                }
            }
        }
    });
    if expired(&interrupted, deadline) {
        return Err(ExecutionError::Timeout(limits.timeout));
    }
    let result = outcome?;
    let (kept_logs, truncated) =
        truncate_logs(&logs.lock().expect("log lock"), limits.output_bytes);
    let recorded_calls = mem::take(&mut *calls.lock().expect("call record lock"));
    Ok(CodeOutput {
        logs: kept_logs,
        truncated,
        result,
        calls: recorded_calls,
    })
}

fn install_bindings<'js>(ctx: &Ctx<'js>, bindings: &str) -> AnyResult<()> {
    let value = ctx
        .json_parse(bindings.to_owned())
        .map_err(|err| anyhow::anyhow!("inject bindings: {err}"))?;
    ctx.globals()
        .set("bindings", value)
        .map_err(|err| anyhow::anyhow!("set bindings: {err}"))?;
    Ok(())
}

/// Install only scanned tool paths. The Rust bridge functions take owned
/// strings, so no JS value escapes the closure; the JS wrapper is the only
/// place that can name a path, and it is built from the scan result.
///
/// Returns the `settle` function kept private by the installer. It is called
/// only from the VM thread, so no JS object crosses a thread boundary.
fn install_globals<'js>(
    ctx: &Ctx<'js>,
    scanned: &scan::Scan,
    invoker: Arc<dyn ToolInvoker>,
    buffer: Arc<Mutex<String>>,
    output_bytes: usize,
    tx: Sender<CompletionMessage>,
    pending: Arc<PendingCalls>,
) -> AnyResult<Function<'js>> {
    let next_ticket = Arc::new(AtomicU64::new(1));
    let tool_bridge = Function::new(
        ctx.clone(),
        move |path: String, args: String| -> rquickjs::Result<u64> {
            invoker.on_call(CallEvent {
                path: &path,
                status: CallStatus::Running,
                input: Some(&args),
            });
            let ticket = next_ticket.fetch_add(1, Ordering::Relaxed);
            let claimed = Arc::new(AtomicBool::new(false));
            {
                let mut waiting = pending.lock().expect("pending lock");
                waiting.insert(ticket, PendingCall { path, args });
                // Borrow both owned strings from the single table entry; a
                // host that completes synchronously only claims its flag and
                // sends to the std channel — it never re-enters `pending` or
                // awaits, so holding this lock across `start` cannot deadlock.
                if let Some(call) = waiting.get(&ticket) {
                    let done = completion(ticket, tx.clone(), Arc::clone(&claimed));
                    if let Err(error) = invoker.start(&call.path, &call.args, done) {
                        send_error(&tx, ticket, &claimed, error.to_string());
                    }
                }
            }
            Ok(ticket)
        },
    )
    .map_err(|err| anyhow::anyhow!("tool bridge: {err}"))?;

    let log_bridge = Function::new(ctx.clone(), move |line: String| {
        append_log(&buffer, &line, output_bytes);
    })
    .map_err(|err| anyhow::anyhow!("log bridge: {err}"))?;

    let paths = serde_json::to_string(&scanned.tools.iter().collect::<Vec<_>>())
        .map_err(|err| anyhow::anyhow!("tool paths: {err}"))?;
    let installer: Function<'_> = ctx
        .eval(GLOBALS_INSTALLER)
        .map_err(|err| anyhow::anyhow!("globals installer: {err}"))?;
    let settle: Function<'_> = installer
        .call((tool_bridge, paths, log_bridge))
        .map_err(|err| anyhow::anyhow!("install globals: {err}"))?;
    Ok(settle)
}

/// Build the one-shot reply handed to a host invoker. A duplicate reply is
/// dropped here, before it can reach the JS resolver map.
fn completion(ticket: u64, tx: Sender<CompletionMessage>, claimed: Arc<AtomicBool>) -> Completion {
    Box::new(move |result| {
        if claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            let _ = tx.send((ticket, result));
        }
    })
}

fn send_error(tx: &Sender<CompletionMessage>, ticket: u64, claimed: &AtomicBool, error: String) {
    if claimed
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
    {
        let _ = tx.send((ticket, Err(error)));
    }
}

fn expired(interrupted: &AtomicBool, deadline: Instant) -> bool {
    interrupted.load(Ordering::Relaxed) || Instant::now() >= deadline
}

/// Resolve or reject one JS promise and publish its terminal record. The
/// returned text is a tool failure the program did not itself reject with.
fn settle_message<'js>(
    settle: &Function<'js>,
    pending: &PendingCalls,
    invoker: &dyn ToolInvoker,
    records: &Mutex<Vec<ToolCallRecord>>,
    message: CompletionMessage,
) -> AnyResult<Option<String>> {
    let (ticket, result) = message;
    let Some(call) = pending.lock().expect("pending lock").remove(&ticket) else {
        return Ok(None);
    };
    let status = if result.is_ok() {
        CallStatus::Completed
    } else {
        CallStatus::Error
    };
    let (ok, payload) = match result {
        Ok(text) => (true, text),
        Err(text) => (false, text),
    };
    invoker.on_call(CallEvent {
        path: &call.path,
        status,
        input: Some(&call.args),
    });
    records
        .lock()
        .expect("call record lock")
        .push(ToolCallRecord {
            path: call.path,
            status,
            input: Some(call.args),
        });
    let settled: bool = settle
        .call((ticket, ok, payload.as_str()))
        .map_err(|err| anyhow!("settle tool call {ticket}: {err}"))?;
    if !settled {
        anyhow::bail!("tool completion {ticket} had no waiting promise");
    }
    Ok((status == CallStatus::Error).then_some(payload))
}

/// A run that ends before its tools do still has to close their host-side
/// lifecycle rows. Removing the entries also makes late replies no-ops.
fn abandon_pending(
    pending: &PendingCalls,
    invoker: &dyn ToolInvoker,
    records: &Mutex<Vec<ToolCallRecord>>,
) {
    let left = mem::take(&mut *pending.lock().expect("pending lock"));
    for (_, call) in left {
        invoker.on_call(CallEvent {
            path: &call.path,
            status: CallStatus::Error,
            input: Some(&call.args),
        });
        records
            .lock()
            .expect("call record lock")
            .push(ToolCallRecord {
                path: call.path,
                status: CallStatus::Error,
                input: Some(call.args),
            });
    }
}

fn rejection_message<'js>(ctx: &Ctx<'js>, promise: &MaybePromise<'js>) -> String {
    // `result` throws the rejection reason into the context; `catch` takes it
    // back out. Never call `finish` on a rejected promise.
    let _: Option<rquickjs::Result<Value<'js>>> = promise.result();
    let reason = ctx.catch();
    Coerced::<String>::from_js(ctx, reason)
        .map(|text| text.0)
        .unwrap_or_else(|_| "promise rejected".to_owned())
}

const GLOBALS_INSTALLER: &str = r#"
(function (start, pathsJson, log) {
  const paths = JSON.parse(pathsJson);
  const tools = {};
  // Resolvers stay in this closure: the script can never reach a ticket or
  // settle somebody else's pending call.
  const pending = new Map();
  for (const path of paths) {
    const parts = path.split(".");
    let target = tools;
    for (const part of parts.slice(0, -1)) {
      if (!Object.prototype.hasOwnProperty.call(target, part)) target[part] = {};
      target = target[part];
    }
    Object.defineProperty(target, parts[parts.length - 1], {
      enumerable: true,
      configurable: false,
      writable: false,
      value: function () {
        const args = Array.prototype.slice.call(arguments);
        const encoded = JSON.stringify(args.length === 1 ? args[0] : args) || "{}";
        return new Promise(function (resolve, reject) {
          const ticket = start(path, encoded);
          pending.set(ticket, { resolve: resolve, reject: reject });
        });
      },
    });
  }
  const freeze = (value) => {
    if (value && typeof value === "object") {
      Object.values(value).forEach(freeze);
      Object.freeze(value);
    }
  };
  freeze(tools);
  globalThis.tools = tools;
  globalThis.console = {
    log: function () {
      const args = Array.prototype.slice.call(arguments);
      log(args.map((x) => typeof x === "string" ? x : JSON.stringify(x)).join(" "));
    },
  };
  return function settle(ticket, ok, payload) {
    const entry = pending.get(ticket);
    if (entry === undefined) return false;
    pending.delete(ticket);
    if (ok) entry.resolve(JSON.parse(payload));
    else entry.reject(new Error(payload));
    return true;
  };
})
"#;

/// Bound the captured buffer while the script is still running: truncating
/// only at the end would let a 30-second logging loop grow the host heap
/// without limit. Twice the delivery budget leaves room for a last line and
/// the truncation marker.
fn append_log(buffer: &Mutex<String>, line: &str, output_bytes: usize) {
    let mut output = buffer.lock().expect("log lock");
    let cap = output_bytes.saturating_mul(2);
    if output.len() >= cap {
        return;
    }
    if !output.is_empty() {
        output.push('\n');
    }
    let remaining = cap.saturating_sub(output.len());
    if line.len() > remaining {
        let mut end = remaining;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&line[..end]);
    } else {
        output.push_str(line);
    }
}

fn decode_result<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> AnyResult<Option<serde_json::Value>> {
    if value.is_null() || value.is_undefined() {
        return Ok(None);
    }
    let json = ctx
        .json_stringify(value)
        .map_err(|err| anyhow::anyhow!("stringify result: {err}"))?
        .ok_or_else(|| anyhow::anyhow!("result has no JSON representation"))?
        .to_string()
        .map_err(|err| anyhow::anyhow!("stringify result: {err}"))?;
    serde_json::from_str(&json).map_err(|err| anyhow::anyhow!("decode result: {err}"))
}

/// Keep the head of the log: delivery information printed first survives a
/// cut, and the strictly decoded result is never part of the log budget.
fn truncate_logs(logs: &str, limit: usize) -> (String, bool) {
    const MARKER: &str = "[truncated]";
    if logs.len() <= limit {
        return (logs.to_owned(), false);
    }
    if limit <= MARKER.len() {
        return (MARKER.chars().take(limit / MARKER.len()).collect(), true);
    }
    let mut cut = limit - MARKER.len();
    while cut > 0 && !logs.is_char_boundary(cut) {
        cut -= 1;
    }
    (format!("{}{MARKER}", &logs[..cut]), true)
}
