//! Sandboxed execution of a scanned Code Mode program.

use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result as AnyResult;
use rquickjs::{Context as QContext, Ctx, Error as JsError, Function, Runtime, Value};

use crate::{CodeResult, Limits, ToolInvoker, scan};

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
    /// Strictly decoded program result.
    pub result: Option<CodeResult>,
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

    let outcome: AnyResult<Option<CodeResult>> = context.with(|ctx| {
        install_bindings(&ctx, bindings)?;
        install_globals(&ctx, &scanned, invoker, logs.clone(), limits.output_bytes)?;
        ctx.eval::<(), _>(HARDEN)?;

        let program: Function<'_> = ctx.eval(scan::wrap(code))?;
        let value: Value<'_> = program.call(())?;
        if interrupted.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Ok(None);
        }
        decode_result(&ctx, value)
    });
    if interrupted.load(Ordering::Relaxed) || Instant::now() >= deadline {
        return Err(ExecutionError::Timeout(limits.timeout));
    }
    let result = outcome.map_err(|err| ExecutionError::Runtime(err.to_string()))?;
    let (kept_logs, truncated) =
        truncate_logs(&logs.lock().expect("log lock"), limits.output_bytes);
    Ok(CodeOutput {
        logs: kept_logs,
        truncated,
        result,
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
fn install_globals<'js>(
    ctx: &Ctx<'js>,
    scanned: &scan::Scan,
    invoker: Arc<dyn ToolInvoker>,
    buffer: Arc<Mutex<String>>,
    output_bytes: usize,
) -> AnyResult<()> {
    let tool_bridge = Function::new(
        ctx.clone(),
        move |path: String, args: String| -> rquickjs::Result<String> {
            invoker
                .invoke(&path, &args)
                .map_err(|err| JsError::InvalidClass {
                    class: "ToolInvoker",
                    message: err.to_string(),
                })
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
    let _: () = installer
        .call((tool_bridge, paths, log_bridge))
        .map_err(|err| anyhow::anyhow!("install globals: {err}"))?;
    Ok(())
}

const GLOBALS_INSTALLER: &str = r#"
(function (invoke, pathsJson, log) {
  const paths = JSON.parse(pathsJson);
  const tools = {};
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
        return JSON.parse(invoke(path, encoded));
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

fn decode_result<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> AnyResult<Option<CodeResult>> {
    if value.is_null() || value.is_undefined() {
        return Ok(None);
    }
    let json = ctx
        .json_stringify(value)
        .map_err(|err| anyhow::anyhow!("stringify result: {err}"))?
        .ok_or_else(|| anyhow::anyhow!("result has no JSON representation"))?
        .to_string()
        .map_err(|err| anyhow::anyhow!("stringify result: {err}"))?;
    let value: serde_json::Value =
        serde_json::from_str(&json).map_err(|err| anyhow::anyhow!("decode result: {err}"))?;
    CodeResult::decode(&value).map_err(|err| anyhow::anyhow!("decode result: {err}"))
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

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use super::{ExecutionError, execute};
    use crate::{Limits, ToolInvoker, execute_scoped};

    #[derive(Default)]
    struct MockInvoker {
        calls: Mutex<Vec<String>>,
    }

    impl ToolInvoker for MockInvoker {
        fn invoke(&self, path: &str, _args: &str) -> anyhow::Result<String> {
            self.calls.lock().expect("mock lock").push(path.to_owned());
            Ok(r#"{"ok":true}"#.to_owned())
        }
    }

    #[test]
    fn runtime_timeout_is_hard() {
        let invoker = Arc::new(MockInvoker::default());
        let limits = Limits {
            timeout: Duration::from_millis(100),
            ..Limits::default()
        };
        let started = Instant::now();
        let result = execute(
            "try { while (true) {} } catch (e) { return { done: true }; }",
            "{}",
            invoker,
            &limits,
        );
        let elapsed = started.elapsed();
        assert!(
            matches!(result, Err(ExecutionError::Timeout(_))),
            "a caught infinite loop must still be a timeout, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "the deadline must actually interrupt, took {elapsed:?}"
        );
    }

    #[test]
    fn runtime_only_scanned_tools() {
        let invoker = Arc::new(MockInvoker::default());
        let allowed = BTreeSet::from(["allowed.go".to_owned()]);
        let output = execute_scoped(
            r#"
                tools.allowed.go({});
                try { tools.hidden.go({}); } catch (e) {}
                return { done: true };
            "#,
            &allowed,
            "{}",
            invoker.clone(),
            &Limits::default(),
        )
        .expect("scanned program must run");
        assert_eq!(
            output.result.and_then(|r| r.done),
            Some(true),
            "the script result must survive execution"
        );
        assert_eq!(
            invoker.calls.lock().expect("mock lock").as_slice(),
            ["allowed.go"],
            "only tools present at scan time may be callable"
        );
    }

    #[test]
    fn output_budget() {
        let invoker = Arc::new(MockInvoker::default());
        let output = execute(
            r#"
                console.log("delivery-first", "x".repeat(100000));
                return { done: true, state: { ok: 1 } };
            "#,
            "{}",
            invoker,
            &Limits::default(),
        )
        .expect("program must run");
        assert!(
            output.logs.len() <= 32 * 1024,
            "logs must fit the 32 KiB budget, got {} bytes",
            output.logs.len()
        );
        assert!(output.truncated, "a 100 KiB log must be truncated");
        assert!(
            output.logs.starts_with("delivery-first"),
            "the head of the log (where delivery info goes) must be kept"
        );
        let result = output.result.expect("result must survive log truncation");
        assert_eq!(result.done, Some(true));
        assert_eq!(result.state, Some(serde_json::json!({"ok": 1})));

        let output = execute(
            r#"
                for (let i = 0; i < 1000; i++) console.log("x".repeat(1024));
                return { done: true };
            "#,
            "{}",
            Arc::new(MockInvoker::default()),
            &Limits::default(),
        )
        .expect("looping logs must not exhaust the buffer");
        assert!(
            output.logs.len() <= 32 * 1024,
            "repeated logs must stay bounded, got {} bytes",
            output.logs.len()
        );
        assert_eq!(output.result.and_then(|r| r.done), Some(true));
    }
}
