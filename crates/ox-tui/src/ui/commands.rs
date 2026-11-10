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
    /// Change the active working directory.
    Cd,
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
    /// Use the `slim` logical model for the next submitted prompt only
    /// (ox-only: one turn, never a session-wide model switch).
    Slim,
    /// Fetch the agents and open the agent picker.
    Agents,
    /// Fetch the MCP servers and open the connect/disconnect dialog
    /// (upstream `mcp.list`, `dialog-mcp.tsx`).
    Mcps,
    /// Ask the server for the session list and open the switcher
    /// (upstream `dialog-session-list.tsx`).
    Sessions,
    /// Fork the current session into a new one and switch to it.
    Fork,
    /// Open the rename prompt for the current session.
    Rename,
    /// Open the message timeline and jump to the chosen message.
    Timeline,
    /// Fetch the session family and open the activity panel on its
    /// `Subagents` tab (upstream's composer tabs).
    Subagents,
    /// Open the activity panel on its Shell tab.
    Shells,
    /// Open the queued-prompts dialog (upstream `session.queued_prompts`).
    QueuedPrompts,
    /// Show the draft's image attachments at full size (upstream
    /// `prompt.images.view`, `<leader>i`).
    Images,
    /// Flip the session's memory switch (ox-only: `mctx::memory_mode`).
    Memory,
    /// Archive the project's active memories (ox-only).
    Forget,
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
        name: "cd",
        aliases: &[],
        description: "Change working directory",
        title: "Change working directory",
        group: "Session",
        action: CommandAction::Cd,
    },
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
        name: "mcps",
        aliases: &[],
        description: "MCP servers",
        title: "MCP servers",
        group: "Agent",
        action: CommandAction::Mcps,
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
    // ox-only: the session's long-term memory switch (read + extract).
    Command {
        name: "forget",
        aliases: &[],
        description: "Archive this project's memories",
        title: "Archive this project's memories",
        group: "Memory",
        action: CommandAction::Forget,
    },
    Command {
        name: "memory",
        aliases: &[],
        description: "Toggle memory for this session",
        title: "Toggle memory for this session",
        group: "Memory",
        action: CommandAction::Memory,
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
        name: "queued",
        aliases: &[],
        description: "View queued prompts",
        title: "View queued prompts",
        group: "Prompt",
        action: CommandAction::QueuedPrompts,
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
        aliases: &["session", "resume", "continue"],
        description: "Switch session",
        title: "Switch session",
        group: "Session",
        action: CommandAction::Sessions,
    },
    // ox-only: upstream has no `/shells`; it opens the activity panel's Shell
    // tab, like `<leader>b` (`app::keymap`).
    Command {
        name: "shells",
        aliases: &[],
        description: "Show background shells",
        title: "Show background shells",
        group: "Session",
        action: CommandAction::Shells,
    },
    // ox-only: one turn on the `slim` logical model. The engine resolves the
    // logical id; this command only records the intent for the next prompt.
    Command {
        name: "slim",
        aliases: &[],
        description: "Use slim for the next prompt (one turn)",
        title: "Use slim for the next prompt",
        group: "Agent",
        action: CommandAction::Slim,
    },
    Command {
        name: "images",
        aliases: &[],
        description: "View image attachments",
        title: "View image attachments",
        group: "Prompt",
        action: CommandAction::Images,
    },
    Command {
        name: "subagents",
        aliases: &[],
        description: "Show subagents",
        title: "Show subagents",
        group: "Session",
        action: CommandAction::Subagents,
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
#[must_use]
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
#[must_use]
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
