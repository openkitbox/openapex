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

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(prefix: &str, n: usize) -> String {
        let lines: Vec<String> = (1..=n).map(|i| format!("{prefix} {i}")).collect();
        lines.join("\n")
    }

    // Upstream test/cli/tui/collapse-tool-output.test.ts:
    // "limits command input and output to the same line budget"
    #[test]
    fn limits_command_input_and_output_to_the_same_line_budget() {
        let command = numbered("command", 8);
        let output = numbered("output", 4);
        let joined = format!("$ {command}\n\n{output}");
        let collapsed = collapse_tool_output(&joined, 10, 1_000);

        assert!(collapsed.overflow, "13 lines exceed a 10-line budget");
        assert_eq!(
            collapsed.output.split('\n').count(),
            10,
            "{:?}",
            collapsed.output
        );
        assert!(
            collapsed.output.contains("$ command 1"),
            "{:?}",
            collapsed.output
        );
        assert!(
            collapsed.output.contains("command 8\n\noutput 1…"),
            "{:?}",
            collapsed.output
        );
        assert!(
            !collapsed.output.contains("output 2"),
            "{:?}",
            collapsed.output
        );
    }

    // ---- ox-added cases (derived from the upstream source) ----

    #[test]
    fn within_budget_is_returned_unchanged_and_borrowed() {
        let got = collapse_shell_output("$ ls", "a\nb", 10, 1_000);
        assert!(!got.overflow);
        assert!(matches!(got.input, Cow::Borrowed("$ ls")), "{got:?}");
        assert!(matches!(got.output, Cow::Borrowed("a\nb")), "{got:?}");
    }

    #[test]
    fn three_line_command_is_cut_to_two_lines() {
        let got = collapse_shell_output("$ a\nb\nc", "", 10, 1_000);
        assert!(got.overflow);
        assert_eq!(got.input, "$ a\nb…");
        assert_eq!(got.output, "");
    }

    #[test]
    fn long_output_keeps_tail_with_plural_label() {
        // command uses 1 line -> output budget 10 - 1 - 1 = 8 lines.
        let output = numbered("o", 12);
        let got = collapse_shell_output("$ x", &output, 10, 1_000);
        assert!(got.overflow);
        assert_eq!(got.input, "$ x");
        // count = 12 - 7 = 5; last 7 lines kept.
        assert_eq!(
            got.output,
            "(5 earlier lines)\no 6\no 7\no 8\no 9\no 10\no 11\no 12"
        );
    }

    #[test]
    fn one_earlier_line_uses_singular_label() {
        // no command: budget 3 lines; 3 lines + char overflow forces tail with count 1.
        let got = collapse_shell_output("", "aaaa\nbbbb\ncccc", 3, 12);
        assert!(got.overflow);
        // count = max(1, 3 - 2) = 1; available = 12 - 16 - 1 <= 0 -> label only.
        assert_eq!(got.output, "(1 earlier line)");
        assert_eq!(got.input, "");
    }

    #[test]
    fn char_overflow_trims_tail_by_chars() {
        let got = collapse_shell_output("", "abcdefghijklmnopqrstuvwxyz0123456789", 10, 30);
        assert!(got.overflow);
        // count = 1, label "(1 earlier line)" = 16 chars, available = 13.
        assert_eq!(got.output, "(1 earlier line)\nxyz0123456789");
    }

    #[test]
    fn tool_output_char_overflow_truncates_head() {
        let got = collapse_tool_output("abcdefghij", 10, 5);
        assert!(got.overflow);
        assert_eq!(got.output, "abcd…");
    }

    #[test]
    fn single_line_budget_returns_label_only() {
        let got = collapse_shell_output("", "a\nb\nc", 1, 1_000);
        assert!(got.overflow);
        assert_eq!(got.output, "(3 earlier lines)");
    }

    #[test]
    fn cjk_command_wraps_by_display_width() {
        // max_chars 40 / max_lines 10 -> 4 columns per command line, 2 lines.
        // Each CJK char is 2 columns: "你好" fills line 1, "世界" line 2, "啊" overflows.
        let got = collapse_shell_output("你好世界啊", "", 10, 40);
        assert!(got.overflow);
        // width == line width at overflow -> last visible grapheme popped.
        assert_eq!(got.input, "你好世…");
    }

    #[test]
    fn cjk_counts_as_one_char_each_for_char_budget() {
        // 6 code points, budget 6: no overflow (code points, not bytes or columns).
        let got = collapse_tool_output("一二三四五六", 10, 6);
        assert!(!got.overflow);
        let got = collapse_tool_output("一二三四五六七", 10, 6);
        assert_eq!(got.output, "一二三四五…");
    }
}
