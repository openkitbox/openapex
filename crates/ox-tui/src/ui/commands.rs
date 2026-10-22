//! The slash-command catalog and its matcher.
//!
//! Pure decision logic: no I/O, no terminal state, no rendering. Upstream keeps
//! this in `app.tsx` (command registry) + `component/prompt/autocomplete.tsx`
//! (matching); ox keeps both here so the menu, the tests and any future command
//! palette share one source.
//!
//! Only commands ox can actually execute are registered. Adding one is a single
//! entry in [`COMMANDS`]; the alias expansion, padding and matching are generic.

/// What the app should do when the user picks a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandAction {
    /// Leave the current session and go back to the home screen.
    New,
    /// Drop the transcript and go back to the home screen.
    Clear,
    /// Quit the app.
    Exit,
}

/// One registered command. `aliases` are rendered as their own menu rows,
/// mirroring upstream (`autocomplete.tsx:490-497`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub action: CommandAction,
}

/// The whole registry. Keep it alphabetical by primary name; the menu sorts by
/// display anyway, this is for humans reading the file.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "clear",
        aliases: &[],
        description: "Clear session",
        action: CommandAction::Clear,
    },
    Command {
        name: "exit",
        aliases: &["quit", "q"],
        description: "Exit the app",
        action: CommandAction::Exit,
    },
    Command {
        name: "new",
        aliases: &[],
        description: "New session",
        action: CommandAction::New,
    },
];

/// A single menu row: one command name or alias, already padded for the
/// two-column look upstream produces with `padEnd(max + 2)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// `/<name>` padded with trailing spaces to `padded_width()`.
    pub display: String,
    /// The name the user actually typed (primary name or alias).
    pub name: &'static str,
    pub description: &'static str,
    pub action: CommandAction,
    /// Higher is a better match; `0` means "filtered out".
    pub score: u32,
}

/// Width every `display` is padded to: the longest `/name` in the whole
/// registry plus two columns (upstream `commands()` memo).
pub fn padded_width() -> usize {
    COMMANDS
        .iter()
        .flat_map(|command| std::iter::once(command.name).chain(command.aliases.iter().copied()))
        .map(|name| name.len() + 1)
        .max()
        .unwrap_or(0)
        + 2
}

/// All rows for `query`, best match first.
///
/// An empty query returns the whole catalog in alphabetical order. Otherwise
/// rows are ordered by score (prefix beats substring beats subsequence), ties
/// broken alphabetically — upstream's fuzzysort with the `×2` prefix bonus,
/// reduced to a deterministic scale.
pub fn rows(query: &str) -> Vec<Row> {
    let query = query.trim().to_ascii_lowercase();
    let width = padded_width();
    let mut out: Vec<Row> = COMMANDS
        .iter()
        .flat_map(|command| {
            std::iter::once(command.name)
                .chain(command.aliases.iter().copied())
                .map(move |name| (name, command))
        })
        .filter_map(|(name, command)| {
            let score = score(&query, name, command.description)?;
            Some(Row {
                display: format!("/{name}").pad_to(width),
                name,
                description: command.description,
                action: command.action,
                score,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.display.cmp(&b.display))
    });
    out
}

/// Score one name against the query; `None` means it does not match at all.
fn score(query: &str, name: &str, description: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let name = name.to_ascii_lowercase();
    if name.starts_with(query) {
        return Some(3);
    }
    if name.contains(query) {
        return Some(2);
    }
    if is_subsequence(query, &name) {
        return Some(1);
    }
    // Description matches are the weakest signal (upstream only matches the
    // description field for slash commands for exactly this reason).
    if description.to_ascii_lowercase().contains(query) {
        return Some(1);
    }
    None
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|wanted| chars.any(|candidate| candidate == wanted))
}

trait PadTo {
    fn pad_to(self, width: usize) -> String;
}

impl PadTo for String {
    fn pad_to(self, width: usize) -> String {
        let len = self.chars().count();
        if len >= width {
            return self;
        }
        let mut out = self;
        out.extend(std::iter::repeat_n(' ', width - len));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{COMMANDS, CommandAction, padded_width, rows};

    #[test]
    fn empty_query_lists_every_row_alphabetically() {
        let all = rows("");
        let names: Vec<&str> = all.iter().map(|row| row.name).collect();
        assert_eq!(names, vec!["clear", "exit", "new", "q", "quit"]);
    }

    #[test]
    fn every_alias_is_its_own_row() {
        let all = rows("");
        assert_eq!(all.len(), 5);
        let quit = all.iter().find(|row| row.name == "quit").expect("quit row");
        assert_eq!(quit.action, CommandAction::Exit);
        assert_eq!(quit.description, "Exit the app");
    }

    #[test]
    fn prefix_and_substring_and_subsequence_are_ranked() {
        let matched = rows("q");
        let names: Vec<&str> = matched.iter().map(|row| row.name).collect();
        assert_eq!(names, vec!["q", "quit"]);

        let matched = rows("ui");
        let names: Vec<&str> = matched.iter().map(|row| row.name).collect();
        assert_eq!(names, vec!["quit"]);
    }

    #[test]
    fn descriptions_match_but_rank_below_names() {
        let matched = rows("app");
        assert_eq!(
            matched.len(),
            3,
            "all three exit rows share the description"
        );
        assert!(matched.iter().all(|row| row.action == CommandAction::Exit));
    }

    #[test]
    fn query_matches_case_insensitively() {
        assert_eq!(rows("NEW")[0].name, "new");
    }

    #[test]
    fn unmatched_query_is_empty() {
        assert!(rows("zzz").is_empty());
    }

    #[test]
    fn display_is_padded_to_the_catalog_width() {
        let width = padded_width();
        assert_eq!(width, "/clear".len() + 2);
        for row in rows("") {
            assert_eq!(row.display.chars().count(), width, "{:?}", row.display);
        }
        assert_eq!(rows("")[0].display, "/clear  ");
    }

    #[test]
    fn registry_has_unique_names_and_aliases() {
        let mut seen = std::collections::HashSet::new();
        for command in COMMANDS {
            for name in std::iter::once(command.name).chain(command.aliases.iter().copied()) {
                assert!(seen.insert(name), "duplicate command name: {name}");
            }
        }
    }
}
