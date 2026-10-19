//! Minimal system prompt (plan §6).
//!
//! Upstream assembles a large instruction stack (instructions state machine,
//! agent prompt, skills, snapshots). ox's runner (task 6) needs a well-formed
//! starting point: an environment block plus the project's `AGENTS.md` when
//! one exists at the working directory. Later tasks extend this template.

use std::path::Path;

/// Build the system prompt for a step: an environment block (working
/// directory, platform, today's UTC date) plus `AGENTS.md` contents read
/// from `directory` when present (read fresh each step — small files only).
///
/// `mctx_tail` is the magic-context prompt tail (the project-memory
/// pinboard, rendered by [`crate::mctx::render`]); an empty tail appends
/// nothing. The caller renders it only when `magic_context.enabled`.
pub fn build(directory: &Path, mctx_tail: &str) -> String {
    let mut prompt = format!(
        "You are a coding agent running in ox, an opencode-compatible server.\n\n\
         <env>\n\
         Working directory: {}\n\
         Platform: {}\n\
         Today's date: {}\n\
         </env>",
        directory.display(),
        std::env::consts::OS,
        today_iso(),
    );
    if let Ok(agents) = std::fs::read_to_string(directory.join("AGENTS.md")) {
        prompt.push_str("\n\n<project_instructions>\n");
        prompt.push_str(agents.trim_end());
        prompt.push_str("\n</project_instructions>");
        // No AGENTS.md (or unreadable): the env block alone is the prompt.
    }
    let tail = mctx_tail.trim_end();
    if !tail.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(tail);
    }
    prompt
}

/// Today's UTC date as `YYYY-MM-DD` (civil-from-days; no chrono dependency).
fn today_iso() -> String {
    days_to_iso(crate::id::now_millis().div_euclid(86_400_000))
}

/// Civil date from days since the Unix epoch (Howard Hinnant's algorithm).
pub(crate) fn days_to_iso(days: i64) -> String {
    // Howard Hinnant's civil_from_days algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn today_iso_matches_known_epoch_anchors() {
        // Day 0 = 1970-01-01; leap-year and century boundaries stay exact.
        assert_eq!(days_to_iso(0), "1970-01-01");
        assert_eq!(days_to_iso(59), "1970-03-01");
        assert_eq!(days_to_iso(19_723), "2024-01-01");
    }

    #[test]
    fn build_includes_env_block() {
        let dir = std::env::temp_dir();
        let prompt = build(&dir, "");
        assert!(prompt.contains("Working directory: "));
        assert!(prompt.contains("Today's date: "));
    }

    #[test]
    fn build_appends_a_nonempty_mctx_tail_and_nothing_else() {
        let dir = std::env::temp_dir();
        let without = build(&dir, "");
        let with = build(
            &dir,
            "<project-memory>\n<ARCHITECTURE>\n#m: fact\n</ARCHITECTURE>\n</project-memory>\n",
        );
        assert!(!without.contains("<project-memory>"));
        assert!(
            with.ends_with(
                "<project-memory>\n<ARCHITECTURE>\n#m: fact\n</ARCHITECTURE>\n</project-memory>"
            ),
            "the tail is the last block, trailing whitespace trimmed"
        );
    }
}
