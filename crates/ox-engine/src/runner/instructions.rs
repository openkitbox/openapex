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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::{KEY_DATE, KEY_INSTRUCTIONS, Observed, ObservedValue, SourceValue};
    use ox_schema::message::{Message, TimeCreated};
    use serde_json::json;

    fn agents(content: &str) -> Observed {
        Observed {
            key: KEY_INSTRUCTIONS,
            value: ObservedValue::Present(SourceValue::Files {
                files: vec![crate::prompt::InstructionFile {
                    path: "/repo/AGENTS.md".to_owned(),
                    content: content.to_owned(),
                }],
            }),
        }
    }

    fn absent(key: &'static str) -> Observed {
        Observed {
            key,
            value: ObservedValue::Removed,
        }
    }

    fn date(value: &str) -> Observed {
        Observed {
            key: KEY_DATE,
            value: ObservedValue::Present(SourceValue::Date {
                value: value.to_owned(),
            }),
        }
    }

    /// `Record` is a `BTreeMap`, not `serde_json::Map`: build it directly.
    fn system_row(delta: serde_json::Value) -> Message {
        let mut metadata = BTreeMap::new();
        metadata.insert("notice".to_owned(), json!("instructions"));
        metadata.insert("instructions".to_owned(), delta);
        Message::System {
            id: "msg_sys".to_owned(),
            metadata: Some(metadata),
            time: TimeCreated { created: 0 },
            text: "delta text".to_owned(),
            description: None,
        }
    }

    #[test]
    fn hash_is_stable_and_content_sensitive() {
        let a = hash(&Some(SourceValue::Date { value: "x".into() }));
        assert_eq!(a, hash(&Some(SourceValue::Date { value: "x".into() })));
        assert_ne!(a, hash(&Some(SourceValue::Date { value: "y".into() })));
        assert_ne!(a, hash(&None));
    }

    #[test]
    fn baseline_freezes_initial_render_and_state() {
        let observed = vec![agents("rule one"), date("Thu Oct 22 2026")];
        let baseline = baseline(&observed);
        assert!(
            baseline
                .initial_render
                .contains("Instructions from: /repo/AGENTS.md\nrule one")
        );
        assert!(
            baseline
                .initial_render
                .contains("Today's date: Thu Oct 22 2026")
        );
        let state = &baseline.sources[KEY_INSTRUCTIONS];
        assert_eq!(state.delivered_hash, state.initial_hash);
    }

    #[test]
    fn unchanged_sources_produce_no_deltas() {
        let observed = vec![agents("rule one")];
        let baseline = baseline(&observed);
        assert!(diff(&observed, &BTreeMap::new(), &baseline).is_empty());
    }

    #[test]
    fn changed_source_yields_one_delta_and_apply_updates_baseline() {
        let before = vec![agents("rule one")];
        let mut baseline = baseline(&before);
        let after = vec![agents(
            "completely different text that is much longer than before so the replacement form wins over the diff form entirely",
        )];
        let deltas = diff(&after, &BTreeMap::new(), &baseline);
        assert_eq!(deltas.len(), 1);
        assert!(
            deltas[0].text.starts_with("The instructions changed:"),
            "{}",
            deltas[0].text
        );
        apply(&deltas, &mut baseline);
        let wire: BTreeMap<String, Option<String>> =
            [(KEY_INSTRUCTIONS.to_owned(), deltas[0].hash.clone())]
                .into_iter()
                .collect();
        assert!(diff(&after, &wire, &baseline).is_empty());
    }

    #[test]
    fn swallowed_delta_is_redelivered() {
        let before = vec![agents("rule one")];
        let mut baseline = baseline(&before);
        let after = vec![agents("rule two v2")];
        let deltas = diff(&after, &BTreeMap::new(), &baseline);
        apply(&deltas, &mut baseline);
        // 窗口里 delta 行消失（被 compaction 折掉）→ 回落到 initial → 重投
        let deltas2 = diff(&after, &BTreeMap::new(), &baseline);
        assert_eq!(deltas2.len(), 1);
        assert!(
            deltas2[0].text.contains("rule two v2"),
            "{}",
            deltas2[0].text
        );
    }

    #[test]
    fn stale_wire_row_restates_the_current_content() {
        // 重启：baseline 已按当前内容重建（delivered == current），wire 仍留着旧 delta 行
        let current = vec![agents("rule three")];
        let baseline = baseline(&current);
        let wire: BTreeMap<String, Option<String>> =
            [(KEY_INSTRUCTIONS.to_owned(), Some("stale-hash".to_owned()))]
                .into_iter()
                .collect();
        let deltas = diff(&current, &wire, &baseline);
        assert_eq!(deltas.len(), 1);
        assert_eq!(
            deltas[0].text,
            "Instructions from: /repo/AGENTS.md\nrule three"
        );
    }

    #[test]
    fn stale_wire_row_for_a_vanished_source_emits_removal() {
        // 重启且源在停机期间被删除：baseline 的 initial/delivered 都是 None，
        // wire 还留着旧 delta 行 → 必须补一条 removed 文案。
        let baseline = baseline(&[]);
        let wire: BTreeMap<String, Option<String>> =
            [(KEY_INSTRUCTIONS.to_owned(), Some("stale-hash".to_owned()))]
                .into_iter()
                .collect();
        let deltas = diff(&[absent(KEY_INSTRUCTIONS)], &wire, &baseline);
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].hash, None);
        assert_eq!(
            deltas[0].text,
            "Previously loaded instructions no longer apply."
        );
    }

    #[test]
    fn removed_source_renders_removed_text_and_reappearance_restores_initial() {
        let before = vec![agents("rule one")];
        let mut baseline = baseline(&before);
        let gone = vec![absent(KEY_INSTRUCTIONS)];
        let deltas = diff(&gone, &BTreeMap::new(), &baseline);
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].hash, None);
        assert_eq!(
            deltas[0].text,
            "Previously loaded instructions no longer apply."
        );
        apply(&deltas, &mut baseline);
        let back = vec![agents("rule one")];
        let wire: BTreeMap<String, Option<String>> =
            [(KEY_INSTRUCTIONS.to_owned(), None)].into_iter().collect();
        let deltas2 = diff(&back, &wire, &baseline);
        assert_eq!(deltas2.len(), 1);
        assert_eq!(
            deltas2[0].text,
            "Instructions from: /repo/AGENTS.md\nrule one"
        );
    }

    #[test]
    fn unavailable_source_is_skipped() {
        let before = vec![agents("rule one")];
        let baseline = baseline(&before);
        let observed = vec![Observed {
            key: KEY_INSTRUCTIONS,
            value: ObservedValue::Unavailable,
        }];
        assert!(diff(&observed, &BTreeMap::new(), &baseline).is_empty());
    }

    #[test]
    fn source_appearing_mid_session_renders_its_initial_text() {
        // baseline 里没有该源（例如 MCP 中途连上 / skills 中途出现）
        let baseline = baseline(&[]);
        let deltas = diff(&[agents("rule one")], &BTreeMap::new(), &baseline);
        assert_eq!(deltas.len(), 1);
        assert_eq!(
            deltas[0].text,
            "Instructions from: /repo/AGENTS.md\nrule one"
        );
    }

    #[test]
    fn date_rollover_yields_is_now_text() {
        let baseline = baseline(&[date("Thu Oct 22 2026")]);
        let deltas = diff(&[date("Fri Oct 23 2026")], &BTreeMap::new(), &baseline);
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].text, "Today's date is now: Fri Oct 23 2026");
    }

    #[test]
    fn fold_reads_last_write_per_key_and_respects_boundary() {
        let rows = vec![
            system_row(json!({"core/date": "aaa"})),
            system_row(json!({"core/date": "bbb", "core/mcp-guidance": "removed"})),
        ];
        let folded = fold(&rows, 0);
        assert_eq!(folded["core/date"].as_deref(), Some("bbb"));
        assert_eq!(folded["core/mcp-guidance"], None);
        let folded = fold(&rows, 1);
        assert_eq!(folded["core/date"].as_deref(), Some("bbb"));
        // Legacy `null` rows and malformed non-string values decode as removed.
        let legacy = vec![system_row(json!({
            "core/mcp-guidance": null,
            "core/skill-guidance": 42,
        }))];
        let folded = fold(&legacy, 0);
        assert_eq!(folded["core/mcp-guidance"], None);
        assert_eq!(folded["core/skill-guidance"], None);
    }

    #[test]
    fn delta_record_encodes_removed_with_the_upstream_sentinel() {
        let baseline = baseline(&[agents("rule one")]);
        let changed = diff(&[agents("rule two v2")], &BTreeMap::new(), &baseline);
        let record = delta_record(&changed);
        assert_eq!(
            record[KEY_INSTRUCTIONS].as_str(),
            changed[0].hash.as_deref()
        );
        let removed = diff(&[absent(KEY_INSTRUCTIONS)], &BTreeMap::new(), &baseline);
        let record = delta_record(&removed);
        assert_eq!(record[KEY_INSTRUCTIONS].as_str(), Some("removed"));
    }

    /// Batch F2: the catalog policy is part of the observed value, so a flip
    /// mid-session hashes differently and the keyed framework re-asserts the
    /// catalog exactly once (`.ox/plans/ox-injection-hardening.md` §2-F).
    #[test]
    fn a_catalog_policy_flip_fires_exactly_one_delta() {
        use crate::prompt::{CatalogPolicy, KEY_SKILLS, SkillSummary};
        fn skills(policy: CatalogPolicy) -> Observed {
            Observed {
                key: KEY_SKILLS,
                value: ObservedValue::Present(SourceValue::Skills {
                    skills: vec![SkillSummary {
                        id: "i".into(),
                        name: "N".into(),
                        description: Some("d".into()),
                    }],
                    policy,
                }),
            }
        }
        let mut baseline = baseline(&[skills(CatalogPolicy::Full)]);
        let deltas = diff(
            &[skills(CatalogPolicy::Compact)],
            &BTreeMap::new(),
            &baseline,
        );
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].key, KEY_SKILLS);
        apply(&deltas, &mut baseline);
        // The wire now shows the delta row the publish projected, so the
        // believed state is what we just delivered: no second delta.
        let wire: BTreeMap<String, Option<String>> =
            [(KEY_SKILLS.to_owned(), deltas[0].hash.clone())]
                .into_iter()
                .collect();
        assert!(diff(&[skills(CatalogPolicy::Compact)], &wire, &baseline).is_empty());
    }
}
