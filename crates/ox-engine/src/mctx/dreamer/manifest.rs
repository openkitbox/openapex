use std::sync::OnceLock;

use regex::Regex;

use crate::mctx::memory::CATEGORIES;

// ---------------------------------------------------------------------------
// Applied-manifest report
// ---------------------------------------------------------------------------

/// What one applied manifest did, for the report and the log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ApplyReport {
    pub(super) verified: usize,
    pub(super) skipped: usize,
    pub(super) updated: usize,
    pub(super) merged: usize,
    pub(super) archived: usize,
    pub(super) refused: usize,
}

impl ApplyReport {
    pub(super) fn describe(&self) -> String {
        format!(
            "verified={} skipped={} updated={} merged={} archived={} refused={}",
            self.verified, self.skipped, self.updated, self.merged, self.archived, self.refused
        )
    }
}

/// Keep an id only when it belongs to the batch, rejecting a repeat — the
/// reference's `assertNoDuplicateManifestIds`. Unknown ids are dropped and
/// counted (the reference logs and drops them); a duplicate makes the whole
/// manifest inconsistent, so it is recorded in `error` and the caller turns it
/// into a `parse_failed`.
pub(super) fn keep_batch_id(
    id: &str,
    batch_ids: &[&str],
    known: &mut Vec<String>,
    unknown: &mut usize,
    error: &mut Option<String>,
) -> bool {
    if !batch_ids.contains(&id) {
        *unknown += 1;
        return false;
    }
    if known.iter().any(|seen| seen == id) {
        *error = Some(format!("manifest contains duplicate id {id}"));
        return false;
    }
    known.push(id.to_owned());
    true
}

/// The `verify`/`curate` safety predicate, ported from
/// `dreamer/memory-claim-safety.ts` (all five patterns, one combined regex):
/// only explicit workflow language inside `PROJECT_RULES` counts as a
/// directive. Declarative implementation claims such as "binds use spread
/// args" deliberately stay outside the gate and remain verifiable.
pub(super) fn directive_shaped_project_rule(category: &str, content: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    if category != CATEGORIES[0] {
        return false;
    }
    let text = content.trim();
    if text.is_empty() {
        return false;
    }
    // A clone drops its lazy-DFA cache with this call (see
    // `historian_validate.rs`'s regex section).
    let re = RE.get_or_init(|| {
        // POLICY_SENTENCE_START alternation 3 (`do not`) and WORKFLOW_IMPERATIVE
        // are both case-sensitive in the reference; the rest are insensitive.
        // ox compiles one case-insensitive regex and spells the two remaining
        // branches in lower case, which is a strict superset of the gate.
        Regex::new(
            r"(?i)(?:^|[.!?]\s+|\n\s*)(?:[-*]\s+|\d+[.)]\s+)?(?:you\s+|we\s+)?(?:must(?:\s+not)?|never|always|do\s+not|don't|shall\s+not)\b|\b(?:you|we|agents?|masons?|workers?|operators?|users?|maintainers?)\s+(?:must(?:\s+not)?|should(?:\s+not)?|need\s+to|shall(?:\s+not)?|cannot|can't|may\s+not)\b|\bwhen\s+(?:you(?:'re|\s+are)?|we(?:'re|\s+are)?|told|asked|requested|working|debugging|reviewing|checking|verifying|investigating|using|running|editing|changing)\b|(?:^|[.!?]\s+|\n\s*)(?:[-*]\s+|\d+[.)]\s+)?(?:please\s+)?(?:run|use|check|ask|avoid|prefer|ensure|keep|follow|brief|report|inspect|search|open|read|review|validate|confirm|delegate|stop|start|remember)\b|\b(?:the\s+)?(?:user|operator|maintainer|owner)\s+(?:decides|chooses|approves|has\s+(?:the\s+)?final\s+say)\b",
        )
        .expect("directive-shaped project rule regex")
    })
    .clone();
    re.is_match(text)
}

// ---------------------------------------------------------------------------
// Manifest lexing
// ---------------------------------------------------------------------------

/// The body of `<root>…</root>`, or an error. Ported from
/// `dreamer/manifest-parser.ts::extractCompleteManifestBody`: a missing
/// closing root is truncation and rejects the whole output, so a length-capped
/// response can never apply a prefix of its verdicts. Like the reference, the
/// search is unanchored (prose around a complete manifest is tolerated) and
/// takes the first complete root.
pub(super) fn complete_manifest_body(text: &str, root: &str) -> Result<String, String> {
    let regex = match root {
        "verify" => verify_root_regex(),
        _ => curate_root_regex(),
    };
    regex
        .captures(text)
        .and_then(|captures| captures.get(1))
        .map(|body| body.as_str().to_owned())
        .ok_or_else(|| {
            format!(
                "{root} manifest missing a complete root element \
                 (truncated output, or the wrong task's manifest)"
            )
        })
}

// Regex accessors return a clone so the lazy-DFA cache is dropped after the
// call instead of living on per worker thread (see `historian_validate.rs`).
fn verify_root_regex() -> Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<verify\b[^>]*>(.*?)</verify\s*>").expect("verify root regex")
    })
    .clone()
}

fn curate_root_regex() -> Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<curate\b[^>]*>(.*?)</curate\s*>").expect("curate root regex")
    })
    .clone()
}

/// The `name="value"` attributes of one element's attribute string.
pub(super) fn attr_of(attrs: &str, name: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE
        .get_or_init(|| Regex::new(r#"(\w+)\s*=\s*"([^"]*)""#).expect("xml attr regex"))
        .clone();
    re.captures_iter(attrs)
        .find(|captures| captures.get(1).is_some_and(|key| key.as_str() == name))
        .and_then(|captures| captures.get(2))
        .map(|value| value.as_str().to_owned())
}

pub(super) fn verified_regex() -> Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<verified\b([^>]*)>").expect("verified element regex"))
        .clone()
}

pub(super) fn update_regex() -> Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<update\b([^>]*?)(?:/>|>(.*?)</update\s*>)")
            .expect("update element regex")
    })
    .clone()
}

pub(super) fn archive_regex() -> Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<archive\b([^>]*)>").expect("archive element regex"))
        .clone()
}

pub(super) fn skip_regex() -> Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<skip\b([^>]*)>").expect("skip element regex"))
        .clone()
}

pub(super) fn merge_regex() -> Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<merge\b([^>]*?)(?:/>|>(.*?)</merge\s*>)").expect("merge element regex")
    })
    .clone()
}
