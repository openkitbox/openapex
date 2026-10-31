//! The slash-command catalog and its matcher.
//!
//! Pure decision logic: no I/O, no terminal state, no rendering. Upstream keeps
//! this in `app.tsx` (command registry) + `component/prompt/autocomplete.tsx`
//! (matching); ox keeps both here so the menu, the tests and any future command
//! palette share one source.
//!
//! The command palette (`ui/palette.rs`) lists the same registry.
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
    /// Ask the server for the MCP server list and open the status dialog
    /// (upstream `dialog-status.tsx`).
    Status,
    /// Fetch the model catalogue and open the model picker.
    Models,
    /// Fetch the agents and open the agent picker.
    Agents,
    /// Ask the server for the session list and open the switcher
    /// (upstream `dialog-session-list.tsx`).
    Sessions,
    /// Fork the current session into a new one and switch to it.
    Fork,
    /// Open the rename prompt for the current session.
    Rename,
    /// Open the message timeline and jump to the chosen message.
    Timeline,
}

/// One registered command. `aliases` are rendered as their own menu rows,
/// mirroring upstream (`autocomplete.tsx:490-497`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    /// Row title in the command palette (upstream `title`).
    pub title: &'static str,
    /// Palette group header (upstream `group`).
    pub group: &'static str,
    pub action: CommandAction,
}

/// The whole registry. Keep it alphabetical by primary name; the menu sorts by
/// display anyway, this is for humans reading the file.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "agents",
        aliases: &[],
        description: "Switch agent",
        title: "Switch agent",
        group: "Agent",
        action: CommandAction::Agents,
    },
    Command {
        name: "clear",
        aliases: &[],
        description: "Clear session",
        title: "Clear session",
        group: "Session",
        action: CommandAction::Clear,
    },
    Command {
        name: "exit",
        aliases: &["quit", "q"],
        description: "Exit the app",
        title: "Exit the app",
        group: "System",
        action: CommandAction::Exit,
    },
    Command {
        name: "models",
        aliases: &[],
        description: "Switch model",
        title: "Switch model",
        group: "Agent",
        action: CommandAction::Models,
    },
    Command {
        name: "fork",
        aliases: &[],
        description: "Fork session",
        title: "Fork session",
        group: "Session",
        action: CommandAction::Fork,
    },
    Command {
        name: "new",
        aliases: &[],
        description: "New session",
        title: "New session",
        group: "Session",
        action: CommandAction::New,
    },
    Command {
        name: "rename",
        aliases: &[],
        description: "Rename session",
        title: "Rename session",
        group: "Session",
        action: CommandAction::Rename,
    },
    Command {
        name: "sessions",
        aliases: &["resume", "continue"],
        description: "Switch session",
        title: "Switch session",
        group: "Session",
        action: CommandAction::Sessions,
    },
    Command {
        name: "timeline",
        aliases: &[],
        description: "Jump to message",
        title: "Jump to message",
        group: "Session",
        action: CommandAction::Timeline,
    },
    Command {
        name: "status",
        aliases: &[],
        description: "Show MCP server status",
        title: "View status",
        group: "System",
        action: CommandAction::Status,
    },
];

use super::fuzzy;

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
            // Description matches are the weakest signal (upstream only matches
            // the description field for slash commands for exactly this reason).
            let score = fuzzy::score(&query, name).or_else(|| {
                command
                    .description
                    .to_ascii_lowercase()
                    .contains(&query)
                    .then_some(1)
            })?;
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
        assert_eq!(
            names,
            vec![
                "agents", "clear", "continue", "exit", "fork", "models", "new", "q", "quit",
                "rename", "resume", "sessions", "status", "timeline"
            ]
        );
    }

    #[test]
    fn every_alias_is_its_own_row() {
        let all = rows("");
        assert_eq!(all.len(), 14);
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
    fn sessions_answers_to_resume_and_continue() {
        for name in ["sessions", "resume", "continue"] {
            let row = rows(name).into_iter().find(|row| row.name == name);
            assert_eq!(row.map(|row| row.action), Some(CommandAction::Sessions));
        }
        assert_eq!(rows("fork")[0].action, CommandAction::Fork);
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
        assert_eq!(width, "/continue".len() + 2);
        for row in rows("") {
            assert_eq!(row.display.chars().count(), width, "{:?}", row.display);
        }
        assert_eq!(rows("")[0].display, "/agents    ");
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
