//! Pure keyed-instruction state core (upstream `instructions/index.ts` +
//! `session/instruction-state.ts`), ox shape: no persisted state table — the
//! baseline lives in memory per session and the wire window is folded from
//! system-row metadata each step (a compacted-away delta re-delivers).

use std::collections::BTreeMap;

use serde_json::Value;

use crate::prompt::{self, Observed, ObservedValue, SourceValue};
use ox_schema::message::Message;

/// sha256 hex of the value's canonical JSON (`"null"` when absent). The hash
/// is persisted in delta-row metadata, so the algorithm must stay stable.
pub(crate) fn hash(value: &Option<SourceValue>) -> String {
    use sha2::Digest;
    let canonical = serde_json::to_string(value).expect("SourceValue is serializable");
    format!("{:x}", sha2::Sha256::digest(canonical.as_bytes()))
}

/// What the model is believed to hold for one source.
pub(crate) struct SourceState {
    /// Frozen at baseline creation — these bytes are what system[1] shows.
    pub initial_hash: String,
    pub initial_value: Option<SourceValue>,
    /// What the latest emitted delta asserts (starts equal to the initial state).
    pub delivered_hash: String,
    pub delivered_value: Option<SourceValue>,
}

/// Per-session frozen initial render + per-source state. In memory only
/// (restart re-baselines from current sources; the wire fold corrects stale
/// delta rows with a re-asserting delta).
pub(crate) struct Baseline {
    /// Frozen system[1] bytes.
    pub initial_render: String,
    pub sources: BTreeMap<&'static str, SourceState>,
}

pub(crate) fn baseline(observed: &[Observed]) -> Baseline {
    let mut sources = BTreeMap::new();
    for source in observed {
        let value = observed_value_to_option(&source.value);
        let hash = hash(&value);
        sources.insert(
            source.key,
            SourceState {
                initial_hash: hash.clone(),
                initial_value: value.clone(),
                delivered_hash: hash,
                delivered_value: value,
            },
        );
    }
    Baseline {
        initial_render: prompt::render_initial(observed),
        sources,
    }
}

/// `Present` → the value; `Removed`/`Unavailable` → absent for state purposes.
fn observed_value_to_option(value: &ObservedValue) -> Option<SourceValue> {
    match value {
        ObservedValue::Present(v) => Some(v.clone()),
        ObservedValue::Removed | ObservedValue::Unavailable => None,
    }
}

/// Fold the visible window's system-row metadata into key → delivered hash
/// (`None` = removed). `boundary` is the compaction boundary index shared
/// with `build_chat_messages`; later rows win.
pub(crate) fn fold(messages: &[Message], boundary: usize) -> BTreeMap<String, Option<String>> {
    let mut folded = BTreeMap::new();
    for message in messages.iter().skip(boundary) {
        if let Message::System {
            metadata: Some(metadata),
            ..
        } = message
            && let Some(Value::Object(delta)) = metadata.get("instructions")
        {
            for (key, value) in delta {
                // `"removed"` is upstream's sentinel; a legacy `null` decodes
                // the same way. Anything that is not a string is a malformed
                // row, treated as removed.
                let decoded = match value.as_str() {
                    Some(REMOVED_SENTINEL) | None => None,
                    Some(hash) => Some(hash.to_owned()),
                };
                folded.insert(key.clone(), decoded);
            }
        }
    }
    folded
}

/// One pending instruction delta.
pub(crate) struct Delta {
    pub key: &'static str,
    /// New hash; `None` = the source is removed (upstream `Instruction.removed`).
    pub hash: Option<String>,
    pub value: Option<SourceValue>,
    pub text: String,
}

pub(crate) fn diff(
    observed: &[Observed],
    wire: &BTreeMap<String, Option<String>>,
    baseline: &Baseline,
) -> Vec<Delta> {
    let mut deltas = Vec::new();
    for source in observed {
        // A transient read failure says nothing about the source: keep what
        // the model already has (upstream `Unavailable`).
        if matches!(source.value, ObservedValue::Unavailable) {
            continue;
        }
        let current = observed_value_to_option(&source.value);
        let current_hash = hash(&current);
        let state = baseline.sources.get(source.key);
        // What the model is believed to hold: the visible wire delta when one
        // survives in the window, else the frozen initial render.
        let believed_hash = match wire.get(source.key) {
            Some(wire_hash) => wire_hash.clone().unwrap_or_else(|| hash(&None)),
            None => state
                .map(|s| s.initial_hash.clone())
                .unwrap_or_else(|| hash(&None)),
        };
        if current_hash == believed_hash {
            continue;
        }
        let (text, hash) = match &current {
            None => match prompt::removed_text(source.key) {
                // Removal text depends only on the source kind — deliverable even when
                // the baseline never held content (a restart after the file vanished).
                Some(text) => (text, None),
                None => continue, // date/env cannot be removed
            },
            Some(value) => {
                // Best-known previous content for the diff form: only a
                // surviving row that matches what we delivered tells us what
                // the model actually saw. Anything else (the delta was
                // compacted away, or a stale row survived a restart) falls
                // back to the frozen initial value / a full restatement, so
                // the model never gets a diff against content it may not hold.
                let previous = match wire.get(source.key) {
                    // The surviving row is exactly what we delivered: we know its content.
                    Some(w) if w.as_deref() == state.map(|s| s.delivered_hash.as_str()) => {
                        state.and_then(|s| s.delivered_value.as_ref())
                    }
                    // A stale row (e.g. after a restart): we cannot reconstruct what the
                    // model saw, so restate the current content in full.
                    Some(_) => None,
                    None => state.and_then(|s| s.initial_value.as_ref()),
                };
                (prompt::changed_text(value, previous), Some(current_hash))
            }
        };
        deltas.push(Delta {
            key: source.key,
            hash,
            value: current,
            text,
        });
    }
    deltas
}

/// Delta texts joined for the wire (upstream `render`: `"\n\n"`).
pub(crate) fn render_updates(deltas: &[Delta]) -> String {
    deltas
        .iter()
        .map(|d| d.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Upstream `Instruction.removed` wire sentinel (`schema/src/instruction.ts:20`).
pub(crate) const REMOVED_SENTINEL: &str = "removed";

/// Encode deltas as the event/metadata `delta` record: source key → hash, or
/// `"removed"` (upstream `Instruction.Delta`).
pub(crate) fn delta_record(
    deltas: &[Delta],
) -> std::collections::BTreeMap<String, serde_json::Value> {
    deltas
        .iter()
        .map(|d| {
            (
                d.key.to_owned(),
                serde_json::Value::String(
                    d.hash
                        .clone()
                        .unwrap_or_else(|| REMOVED_SENTINEL.to_owned()),
                ),
            )
        })
        .collect()
}

/// The machine-readable source entries behind one delta (batch I / L6): the
/// projected system row's `instructionSources` metadata carries one entry per
/// instruction file (`global` = home file, `project` = ancestor chain) and
/// one `config` entry per non-file source, so replay/audit tooling never
/// string-matches paths. Removed sources carry no files: one key-only entry.
pub(crate) fn source_records(deltas: &[Delta]) -> Vec<ox_schema::event::InstructionSource> {
    deltas
        .iter()
        .flat_map(|delta| match &delta.value {
            Some(SourceValue::Files { files }) => files
                .iter()
                .map(|file| ox_schema::event::InstructionSource {
                    key: delta.key.to_owned(),
                    path: Some(file.path.clone()),
                    kind: Some(file.kind),
                })
                .collect::<Vec<_>>(),
            _ => {
                vec![ox_schema::event::InstructionSource {
                    key: delta.key.to_owned(),
                    path: None,
                    kind: Some(ox_schema::event::InstructionSourceKind::Config),
                }]
            }
        })
        .collect()
}

/// Record the emitted deltas into the baseline (delivered only; the frozen
/// initial state never moves).
pub(crate) fn apply(deltas: &[Delta], baseline: &mut Baseline) {
    for delta in deltas {
        let state = baseline
            .sources
            .entry(delta.key)
            .or_insert_with(|| SourceState {
                initial_hash: hash(&None),
                initial_value: None,
                delivered_hash: hash(&None),
                delivered_value: None,
            });
        state.delivered_hash = delta.hash.clone().unwrap_or_else(|| hash(&None));
        state.delivered_value = delta.value.clone();
    }
}
