//! Conservative lexical analysis of shell commands for permission extraction.
//! No shell is spawned and no path is resolved.

/// The whole-command resource and the patterns offered to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCommands {
    pub resources: Vec<String>,
    pub save: Vec<String>,
}

/// Split a command into permission resources. Only commands whose safety is
/// evident lexically get an exact `save` pattern; everything else is gated as
/// one indivisible resource with no "Always" option.
#[must_use]
pub fn analyze(command: &str) -> ShellCommands {
    let trimmed = command.trim();
    if trimmed.is_empty() || is_composite(trimmed) {
        let resource = if trimmed.is_empty() {
            "*".to_owned()
        } else {
            command.to_owned()
        };
        return ShellCommands {
            resources: vec![resource],
            save: Vec::new(),
        };
    }
    let normalized = normalize(trimmed);
    // A saved pattern is matched with wildcards by the engine; never let a
    // command containing wildcard syntax become an "Always" rule.
    let save = if normalized.contains(['*', '?']) {
        Vec::new()
    } else {
        vec![normalized.clone()]
    };
    ShellCommands {
        resources: vec![normalized],
        save,
    }
}

fn is_composite(command: &str) -> bool {
    if command.chars().any(|ch| {
        matches!(
            ch,
            ';' | '`' | '|' | '\n' | '\r' | '(' | ')' | '&' | '<' | '>'
        )
    }) {
        return true;
    }
    let mut words = command.split_whitespace();
    let program = words.next().unwrap_or_default();
    if (program == "rm" || program.ends_with("/rm"))
        && words.any(|word| word.starts_with('-') && (word.contains('r') || word.contains('R')))
    {
        return true;
    }
    let mut quote = None;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            escaped = false;
        } else if ch == '\\' && quote != Some('\'') {
            escaped = true;
        } else if let Some(delimiter) = quote {
            if ch == delimiter {
                quote = None;
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        }
    }
    quote.is_some() || escaped
}

/// Collapse whitespace outside quoted strings; preserve quoted text verbatim.
fn normalize(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut quote = None;
    let mut escaped = false;
    let mut pending_space = false;
    for ch in command.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
            pending_space = false;
            continue;
        }
        if quote.is_none() && ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
        if quote.is_none() && ch == '\\' {
            escaped = true;
        } else if let Some(delimiter) = quote {
            if ch == delimiter {
                quote = None;
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        }
    }
    out
}
