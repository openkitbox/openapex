//! Collapse a shell block's command and output to a line/char budget.
//!
//! Verbatim port of upstream
//! `packages/tui/src/util/collapse-tool-output.ts` (`collapseToolOutput`,
//! `collapseShellOutput` and its private helpers). The caller is the shell
//! block in `packages/tui/src/routes/session/index.tsx`, which passes
//! `max_lines = 10` and `max_chars = 10 * max(20, width - 6 - (running ? 2 : 0))`.
//!
//! Deviations:
//! - Counting: upstream counts characters with `Array.from(s).length`, i.e.
//!   Unicode code points; Rust `str::chars()` counts the same unit, so this is
//!   an exact equivalent, not UTF-16 code units.
//! - Command wrapping segments with `unicode-segmentation` extended graphemes
//!   (upstream: `Intl.Segmenter`) and measures with `unicode-width`
//!   (upstream: the `string-width` package). Both agree on ASCII and CJK
//!   (wide = 2 columns); they may differ on rare emoji/ambiguous-width cases.
//! - Arithmetic that can go negative in JS uses saturating `usize` math with
//!   the same `max(1, …)` / `<= 0` guards, so results match for all inputs.

use std::borrow::Cow;

/// Result of [`collapse_tool_output`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCollapse<'a> {
    pub output: Cow<'a, str>,
    pub overflow: bool,
}

/// Result of [`collapse_shell_output`]: the command line(s) and output to
/// draw while collapsed, and whether anything was cut (block is expandable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCollapse<'a> {
    pub input: Cow<'a, str>,
    pub output: Cow<'a, str>,
    pub overflow: bool,
}

/// Keep the head of `output` within `max_lines` lines and `max_chars` chars.
#[must_use]
pub fn collapse_tool_output(output: &str, max_lines: usize, max_chars: usize) -> ToolCollapse<'_> {
    let lines: Vec<&str> = output.split('\n').collect();
    if lines.len() <= max_lines && output.chars().count() <= max_chars {
        return ToolCollapse {
            output: Cow::Borrowed(output),
            overflow: false,
        };
    }

    let visible = &lines[..max_lines.min(lines.len())];
    let mut preview = visible.join("\n");
    if lines.len() > max_lines && !visible.is_empty() {
        preview.push('…');
    }
    if preview.chars().count() > max_chars {
        let mut cut: String = preview.chars().take(max_chars.saturating_sub(1)).collect();
        cut.push('…');
        return ToolCollapse {
            output: Cow::Owned(cut),
            overflow: true,
        };
    }

    ToolCollapse {
        output: Cow::Owned(preview),
        overflow: true,
    }
}

/// Collapse a shell command (`input`, at most 2 lines) and its output (tail
/// kept, with an `(N earlier lines)` label) into one shared budget.
#[must_use]
pub fn collapse_shell_output<'a>(
    input: &'a str,
    output: &'a str,
    max_lines: usize,
    max_chars: usize,
) -> ShellCollapse<'a> {
    if input.is_empty() {
        let overflow = collapse_tool_output(output, max_lines, max_chars).overflow;
        return ShellCollapse {
            input: Cow::Borrowed(input),
            output: if overflow {
                collapse_tail(output, max_lines, max_chars)
            } else {
                Cow::Borrowed(output)
            },
            overflow,
        };
    }

    let command_lines = max_lines.min(2);
    let line_chars = (max_chars / max_lines.max(1)).max(1);
    let command = collapse_shell_command(input, command_lines, line_chars);
    if output.is_empty() {
        return ShellCollapse {
            input: command.output,
            output: Cow::Borrowed(output),
            overflow: command.overflow,
        };
    }

    let lines = max_lines.saturating_sub(command.lines + 1).max(1);
    let chars = max_chars
        .saturating_sub(command.output.chars().count() + 2)
        .max(1);
    let overflow = collapse_tool_output(output, lines, chars).overflow;
    ShellCollapse {
        input: command.output,
        output: if overflow {
            collapse_tail(output, lines, chars)
        } else {
            Cow::Borrowed(output)
        },
        overflow: command.overflow || overflow,
    }
}

struct CommandCollapse<'a> {
    output: Cow<'a, str>,
    overflow: bool,
    lines: usize,
}

fn collapse_shell_command(input: &str, max_lines: usize, line_width: usize) -> CommandCollapse<'_> {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    let mut visible: Vec<&str> = Vec::new();
    let mut lines = 1;
    let mut width = 0;
    let mut overflow = false;
    for segment in input.graphemes(true) {
        if segment == "\n" {
            if lines >= max_lines {
                overflow = true;
                break;
            }
            visible.push(segment);
            lines += 1;
            width = 0;
            continue;
        }

        let next = UnicodeWidthStr::width(segment);
        if width + next > line_width {
            if lines >= max_lines {
                overflow = true;
                break;
            }
            lines += 1;
            width = 0;
        }
        visible.push(segment);
        width += next;
    }
    if !overflow {
        return CommandCollapse {
            output: Cow::Borrowed(input),
            overflow,
            lines,
        };
    }

    // Upstream also decrements `width` here; the value is never read again.
    if width >= line_width {
        visible.pop();
    }
    let mut out = visible.concat();
    out.push('…');
    CommandCollapse {
        output: Cow::Owned(out),
        overflow,
        lines,
    }
}

fn collapse_tail(output: &str, max_lines: usize, max_chars: usize) -> Cow<'_, str> {
    let lines: Vec<&str> = output.split('\n').collect();
    if lines.len() <= max_lines && output.chars().count() <= max_chars {
        return Cow::Borrowed(output);
    }

    let count = lines
        .len()
        .saturating_sub(max_lines.saturating_sub(1))
        .max(1);
    let noun = if count == 1 { "line" } else { "lines" };
    let label = format!("({count} earlier {noun})");
    if max_lines <= 1 {
        return Cow::Owned(label);
    }

    let keep = max_lines - 1;
    let preview = lines[lines.len().saturating_sub(keep)..].join("\n");
    let Some(available) = max_chars
        .checked_sub(label.chars().count() + 1)
        .filter(|n| *n > 0)
    else {
        return Cow::Owned(label);
    };
    let total = preview.chars().count();
    let tail: String = preview
        .chars()
        .skip(total.saturating_sub(available))
        .collect();
    Cow::Owned(format!("{label}\n{tail}"))
}
