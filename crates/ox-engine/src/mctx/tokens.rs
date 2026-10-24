//! Per-family exact token counters for the context-loading budget walks.
//!
//! Three model families have an exact tokenizer on disk (assets are
//! machine-local, seeded once per Task 4: `ox_home()/tokenizers/<family>/`,
//! sha256s and repo revisions in `~/.ox/tokenizers/PROVENANCE.md`):
//!
//! - **GLM / DeepSeek** — HF `tokenizer.json`, loaded through the `tokenizers`
//!   crate pinned to the `onig` regex backend. That is the backend the golden
//!   oracle was generated with (the Python `tokenizers` wheel defaults to
//!   oniguruma), so the golden counts below lock the exact engine, not just
//!   the vocab. The crate's only non-onig backend selection is
//!   `unstable_wasm`, which also flips `getrandom/wasm_js` on a native build,
//!   so there is no pure-Rust alternative worth pinning. Deviation from the
//!   plan sketch, recorded here: the sketch implied the fancy-regex backend.
//! - **Kimi** — a tiktoken BPE built from `tiktoken.model` (`.tiktoken`
//!   format: one `<base64> <rank>` line per mergeable rank) through
//!   `tiktoken_rs::CoreBPE::new`. The pattern string and the 256 reserved
//!   special slots are verbatim `tokenization_kimi.py:52-63`; counting goes
//!   through `count_ordinary`, the same ordinary-text semantics the golden
//!   generator used (`encode_ordinary`, `allowed_special=set()`).
//!
//! Anything else — an unknown family, or a vocab file that is missing or
//! fails to load — falls back to [`Counter::Heuristic`], the pre-existing
//! `bytes/4` estimate. The fallback is fail-open by design: an estimator
//! error must never block a turn, and a missing vocab must never crash the
//! server. A load failure warns once (the failed outcome is cached in the
//! same per-family slot) and keeps serving today's behavior.
//!
//! Construction is expensive (a 20 MB vocab parse), so [`counter_for`]
//! builds each family's counter at most once per process and hands out
//! `&'static`. Call sites resolve it once at a fold/pass boundary and inject
//! it into the pure budget walks; [`Counter::count`] itself is pure and
//! takes `&str` — no I/O, no text copies at call time.

use base64::Engine as _;
use std::path::Path;
use std::sync::OnceLock;

/// The model families with an exact tokenizer on disk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Family {
    Glm,
    Kimi,
    DeepSeek,
}

impl Family {
    /// The asset directory name under `ox_home()/tokenizers/`.
    fn dir_name(self) -> &'static str {
        match self {
            Family::Glm => "glm",
            Family::Kimi => "kimi",
            Family::DeepSeek => "deepseek",
        }
    }

    /// The vocab file inside [`Family::dir_name`].
    fn vocab_file(self) -> &'static str {
        match self {
            Family::Glm | Family::DeepSeek => "tokenizer.json",
            Family::Kimi => "tiktoken.model",
        }
    }
}

/// Route a served model id to its family, or `None` when no exact tokenizer
/// applies (the caller stays on [`Counter::Heuristic`]).
///
/// The mapping is a pure name match over the lowercased id (the id → family
/// table is recorded in `~/.ox/tokenizers/PROVENANCE.md`):
///
/// - contains `glm` → GLM, contains `deepseek` → DeepSeek — the substring
///   rules from the plan, matching ids like `glm-5.3-flash` and
///   `deepseek-flash` in any casing;
/// - contains `kimi` → Kimi, **or** a `/`-segment starts with a Kimi
///   generation (`k2`, `k3`, `k4`) followed by `-` or the segment end — the
///   served Kimi ids are bare generation names (`k2-256k`, `k3-256k`,
///   `k4-1m`), so a `contains("kimi")`-only rule would never route any real
///   Kimi traffic. A `k2`/`k3`/`k4` sequence mid-segment (`gpt-4k2`) does not
///   route.
///
/// Deviation from the plan sketch recorded here: the sketch's sketch-code
/// only checked the three substrings.
pub(crate) fn family_of(model_id: &str) -> Option<Family> {
    let id = model_id.to_ascii_lowercase();
    if id.contains("glm") {
        Some(Family::Glm)
    } else if id.contains("kimi") || is_kimi_generation(&id) {
        Some(Family::Kimi)
    } else if id.contains("deepseek") {
        Some(Family::DeepSeek)
    } else {
        None
    }
}

/// Whether any `/`-segment of the (lowercased) id names a Kimi generation.
fn is_kimi_generation(id: &str) -> bool {
    id.split('/').any(|segment| {
        ["k2", "k3", "k4"].iter().any(|generation| {
            segment
                .strip_prefix(generation)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
        })
    })
}

/// Which backend an exact [`Counter`] drives. Large payloads are boxed at
/// the type level: the enum sits behind `&'static` and must not inflate
/// every `Counter` binding to the tokenizer's size.
pub(crate) enum Backend {
    /// GLM / DeepSeek: an HF `tokenizer.json` through the `tokenizers` crate.
    Json(Box<tokenizers::Tokenizer>),
    /// Kimi: a tiktoken BPE over the `.tiktoken` ranks.
    Tiktoken(Box<tiktoken_rs::CoreBPE>),
}

/// A token counter: exact where a vocab loaded, heuristic otherwise.
pub(crate) enum Counter {
    /// The family's real tokenizer.
    Exact(Backend),
    /// The pre-existing estimate: `text.len().div_ceil(4)`.
    Heuristic,
}

impl Counter {
    /// Count tokens in `text`. Pure: no I/O, no allocation, no text copy.
    pub(crate) fn count(&self, text: &str) -> usize {
        match self {
            Counter::Exact(Backend::Json(tokenizer)) => tokenizer
                .encode(text, false)
                .map(|encoding| encoding.get_ids().len())
                // An encode failure on a `&str` is not a known state; the
                // budget walks only need a same-shaped number, so the
                // heuristic keeps them running. Deviation from the plan
                // sketch: no per-call warn — this is hot-path work and the
                // failure would repeat on every row.
                .unwrap_or_else(|_| text.len().div_ceil(4)),
            // `count_ordinary` is the golden oracle's `encode_ordinary`:
            // ordinary text only, special tokens never match.
            Counter::Exact(Backend::Tiktoken(bpe)) => bpe.count_ordinary(text),
            Counter::Heuristic => text.len().div_ceil(4),
        }
    }
}

/// One cache slot per family: the counter is built at most once per process
/// (a 20 MB vocab parse) and handed out `&'static` afterwards. A failed load
/// is cached in the same slot, which is what makes its warn once-only.
static GLM_COUNTER: OnceLock<Counter> = OnceLock::new();
static KIMI_COUNTER: OnceLock<Counter> = OnceLock::new();
static DEEPSEEK_COUNTER: OnceLock<Counter> = OnceLock::new();

/// The family's counter, loading the vocab on first use.
pub(crate) fn counter_for(family: Family) -> &'static Counter {
    match family {
        Family::Glm => GLM_COUNTER.get_or_init(|| load_counter(Family::Glm)),
        Family::Kimi => KIMI_COUNTER.get_or_init(|| load_counter(Family::Kimi)),
        Family::DeepSeek => DEEPSEEK_COUNTER.get_or_init(|| load_counter(Family::DeepSeek)),
    }
}

/// The shared heuristic counter for model ids [`family_of`] cannot route —
/// today's behavior, unchanged for unknown families.
static HEURISTIC: Counter = Counter::Heuristic;

/// The model id's counter, resolved once at a fold/pass boundary and injected
/// into the pure budget walks: the family's exact counter when the vocab is
/// available, the heuristic otherwise (unknown family or failed load).
pub(crate) fn counter_for_model(model_id: &str) -> &'static Counter {
    match family_of(model_id) {
        Some(family) => counter_for(family),
        None => &HEURISTIC,
    }
}

/// Load the family's vocab from its `ox_home()` asset directory.
fn load_counter(family: Family) -> Counter {
    let dir = ox_core::config::ox_home()
        .join("tokenizers")
        .join(family.dir_name());
    load_counter_at(family, &dir)
}

/// Load the family's vocab from `dir`, falling back to [`Counter::Heuristic`]
/// with a once-per-family warn (the failed outcome is cached by
/// [`counter_for`]). Split from [`load_counter`] so tests can point the
/// loader at a tempdir instead of the machine's `ox_home()`.
fn load_counter_at(family: Family, dir: &Path) -> Counter {
    match try_load_backend(family, dir) {
        Ok(backend) => Counter::Exact(backend),
        Err(error) => {
            tracing::warn!(
                family = ?family,
                dir = %dir.display(),
                error = %error,
                "exact tokenizer unavailable; token estimates fall back to bytes/4"
            );
            Counter::Heuristic
        }
    }
}

fn try_load_backend(family: Family, dir: &Path) -> anyhow::Result<Backend> {
    match family {
        Family::Glm | Family::DeepSeek => {
            let vocab = dir.join(family.vocab_file());
            let tokenizer = tokenizers::Tokenizer::from_file(&vocab)
                .map_err(|error| anyhow::anyhow!("load {}: {error}", vocab.display()))?;
            Ok(Backend::Json(Box::new(tokenizer)))
        }
        Family::Kimi => Ok(Backend::Tiktoken(Box::new(load_kimi_bpe(
            &dir.join(family.vocab_file()),
        )?))),
    }
}

/// The reserved special-token slot count (verbatim `tokenization_kimi.py:52`).
const KIMI_RESERVED_SPECIAL_TOKENS: usize = 256;

/// The Kimi pre-tokenizer pattern (verbatim `tokenization_kimi.py:54-63`, the
/// same string the golden generator used). fancy-regex — tiktoken's engine —
/// supports both the `&&` class intersection and the `(?!\S)` lookahead.
const KIMI_PAT_STR: &str = concat!(
    r"[\p{Han}]+",
    "|",
    r#"[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]*[\p{Ll}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]+(?i:'s|'t|'re|'ve|'m|'ll|'d)?"#,
    "|",
    r#"[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]+[\p{Ll}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]*(?i:'s|'t|'re|'ve|'m|'ll|'d)?"#,
    "|",
    r"\p{N}{1,3}",
    "|",
    r" ?[^\s\p{L}\p{N}]+[\r\n]*",
    "|",
    r"\s*[\r\n]+",
    "|",
    r"\s+(?!\S)",
    "|",
    r"\s+",
);

/// Build the Kimi BPE from a `.tiktoken` vocab: one `<base64> <rank>` line
/// per mergeable rank, plus the 256 reserved special tokens that follow the
/// base ranks (verbatim `tokenization_kimi.py:101-105` — their names never
/// match ordinary text, but `CoreBPE::new` needs the set to build its
/// special-token scanner).
fn load_kimi_bpe(path: &Path) -> anyhow::Result<tiktoken_rs::CoreBPE> {
    let contents = std::fs::read_to_string(path)
        .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
    let mut encoder: rustc_hash::FxHashMap<Vec<u8>, tiktoken_rs::Rank> =
        rustc_hash::FxHashMap::default();
    for (index, line) in contents.lines().enumerate() {
        let Some((token, rank)) = line.split_once(' ') else {
            anyhow::bail!(
                "{}:{index}: expected `<base64> <rank>`, got {line:?}",
                path.display()
            );
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(token)
            .map_err(|error| {
                anyhow::anyhow!("{}:{index}: bad base64 token: {error}", path.display())
            })?;
        let rank: tiktoken_rs::Rank = rank
            .parse()
            .map_err(|error| anyhow::anyhow!("{}:{index}: bad rank: {error}", path.display()))?;
        encoder.insert(bytes, rank);
    }
    let base = encoder.len() as tiktoken_rs::Rank;
    let special_tokens: rustc_hash::FxHashMap<String, tiktoken_rs::Rank> = (base..base
        + KIMI_RESERVED_SPECIAL_TOKENS as tiktoken_rs::Rank)
        .map(|rank| (format!("<|reserved_token_{rank}|>"), rank))
        .collect();
    tiktoken_rs::CoreBPE::new(encoder, special_tokens, KIMI_PAT_STR)
        .map_err(|error| anyhow::anyhow!("build kimi BPE: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab_dir(family: Family) -> std::path::PathBuf {
        ox_core::config::ox_home()
            .join("tokenizers")
            .join(family.dir_name())
    }

    #[test]
    fn family_of_routes_three_families_and_unknown() {
        assert_eq!(family_of("GLM-5.X"), Some(Family::Glm));
        assert_eq!(family_of("glm-5.3-flash"), Some(Family::Glm));
        assert_eq!(family_of("kimi3.1pro"), Some(Family::Kimi));
        assert_eq!(family_of("DeepSeek-V4.1-Flash"), Some(Family::DeepSeek));
        assert_eq!(family_of("deepseek-flash"), Some(Family::DeepSeek));
        // The served Kimi ids are generation names with no "kimi" substring
        // (PROVENANCE.md): k2-256k, k3-256k, k4-1m.
        assert_eq!(family_of("k2-256k"), Some(Family::Kimi));
        assert_eq!(family_of("k3-256k"), Some(Family::Kimi));
        assert_eq!(family_of("k4-1m"), Some(Family::Kimi));
        assert_eq!(family_of("qwen3-max"), None);
        // A generation prefix only counts at a segment start.
        assert_eq!(family_of("gpt-4k2"), None);
    }

    /// The four golden samples, byte-identical to
    /// `~/.ox/tokenizers/golden.json` (generated by
    /// `~/.ox/tokenizers/generate_golden.py`; vocab provenance in
    /// `~/.ox/tokenizers/PROVENANCE.md`). Inlined because golden.json is
    /// never read at runtime.
    const ENGLISH_PROSE: &str = "The background fold compresses the transcript silently: no events are emitted, the checkpoint lands in the database, and the next request is about 40% of the window.";
    const CHINESE_PARAGRAPH: &str = "上下文管理分为四级：逐字活尾、段级摘要、会话基线与跨会话记忆。折叠只在步边界检查用量，达到百分之六十就静默压缩，客户端零感知。";
    const MIXED_CODE: &str = "fn recent_text(tail: &[(i64, Value)]) -> String {\n    let budget = RECENT_KEEP_TOKENS * 4;\n    tail.iter().rev().filter(|(_, r)| !r.is_null()).count().to_string() // 注释: 按 token 装填\n}";
    const EMOJI_WHITESPACE: &str =
        "ok  ✅  done — spacing   preserved… \t tabs\tand\nnewlines 🎉🚀  {}[]()<>~!@#$%^&*()";

    #[test]
    fn golden_counts_match_hf() {
        let golden: [(Family, &str, [usize; 4]); 3] = [
            (Family::Glm, "glm", [34, 44, 62, 36]),
            (Family::Kimi, "kimi", [34, 39, 63, 34]),
            (Family::DeepSeek, "deepseek", [34, 40, 66, 36]),
        ];
        let samples: [(&str, &str); 4] = [
            ("english_prose", ENGLISH_PROSE),
            ("chinese_paragraph", CHINESE_PARAGRAPH),
            ("mixed_code", MIXED_CODE),
            ("emoji_whitespace", EMOJI_WHITESPACE),
        ];
        for (family, name, expected) in golden {
            let dir = vocab_dir(family);
            let vocab = dir.join(family.vocab_file());
            assert!(
                vocab.exists(),
                "exact tokenizer asset missing at {} — seed it per ~/.ox/tokenizers/PROVENANCE.md",
                vocab.display()
            );
            let counter = load_counter_at(family, &dir);
            for ((sample, text), expected) in samples.into_iter().zip(expected) {
                assert_eq!(
                    counter.count(text),
                    expected,
                    "{name} {sample}: exact count must match the golden oracle exactly"
                );
            }
        }
    }

    #[test]
    fn heuristic_counter_is_bytes_div_four() {
        let count = |text: &str| Counter::Heuristic.count(text);
        assert_eq!(count(""), 0);
        assert_eq!(count("abc"), 1);
        assert_eq!(count("abcd"), 1);
        assert_eq!(count("abcde"), 2);
        let mixed = "上下文 ok ✅";
        assert_eq!(count(mixed), mixed.len().div_ceil(4));
    }

    #[test]
    fn counter_for_model_caches_per_family_and_defaults_to_heuristic() {
        // An unrouteable id stays on today's heuristic.
        assert!(matches!(counter_for_model("qwen3-max"), Counter::Heuristic));
        // Kimi ids — substring and generation forms — share one cached slot:
        // a repeat call never rebuilds the tokenizer.
        assert!(std::ptr::eq(
            counter_for_model("kimi3.1pro"),
            counter_for_model("k3-256k"),
        ));
        assert!(std::ptr::eq(
            counter_for_model("k4-1m"),
            counter_for_model("k3-256k"),
        ));
    }

    #[test]
    fn missing_vocab_falls_back_to_bytes4() {
        let missing = tempfile::tempdir().expect("tempdir");
        for family in [Family::Glm, Family::Kimi, Family::DeepSeek] {
            let counter = load_counter_at(family, missing.path());
            assert_eq!(
                counter.count("上下文管理分为四级"),
                Counter::Heuristic.count("上下文管理分为四级"),
                "{family:?}: a missing vocab must fall back to the bytes/4 heuristic"
            );
        }
    }
}
