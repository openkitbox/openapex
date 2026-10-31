//! A block tool: upstream `BlockTool` (`routes/session/index.tsx:2883`). Pure:
//! the title, the spinner glyph, body lines and error borrowed in, rows out.
//!
//! The frame (`frame`) is shared with the user message
//! (`index.tsx:2370-2414`), which upstream draws as the same left-bordered,
//! padded box with its own rail colour: a `┃` rail, one blank row above and
//! below, the content two columns in from the rail, every cell on one band.
//!
//! Deviations:
//! - Hover uses `palette.panel` (`raised.high`) rather than computing
//!   `decrease(raised.base)` per frame (plan V7); the user block does the same.
//! - The rail is upstream's `background.base`. When the deck does not know
//!   that colour as RGB (16-colour, colourless) the rail is a blank cell on
//!   the band instead of a default-coloured bar.
//! - `path` titles (`FilePath` truncation) are not ported; callers pass a
//!   finished title string. Tool permission (warning-coloured title) is not
//!   ported either. `headerColor`/`errorColor` are [`BlockSpec::title_fg`] /
//!   [`BlockSpec::error_fg`].
//! - Without animations upstream shows `⋯` for the spinner; this always shows
//!   the frame the caller passes.

use std::borrow::Cow;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::theme;
use crate::ui::paste;

/// The left rail glyph (`SplitBorder.customBorderChars.vertical`).
pub const RAIL: &str = "\u{2503}";
/// Columns between the rail and the content (`paddingLeft={2}`).
const PAD: usize = 2;

/// What one block tool says, borrowed from the block it draws.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlockSpec<'a> {
    /// The header line (`title`), e.g. `# Questions` or `$ cargo test`.
    pub title: Option<&'a str>,
    /// The spinner glyph while the call runs; the title's leading `# ` is
    /// dropped next to it, as upstream does.
    pub spinner: Option<char>,
    /// The failure text, ANSI stripped and drawn last in the error colour.
    pub error: Option<&'a str>,
    /// The pointer is over a clickable block.
    pub hovered: bool,
    /// Upstream `headerColor`: the title colour; `None` is `text.muted`.
    pub title_fg: Option<Color>,
    /// Upstream `errorColor`: the error colour; `None` is the error token.
    pub error_fg: Option<Color>,
}

/// The band behind a block: `raised.base`, `raised.high` under the pointer.
#[must_use]
pub fn surface(palette: &theme::Palette, hovered: bool) -> Style {
    if hovered {
        palette.panel
    } else {
        palette.raised_base
    }
}

/// Columns left for content at `width` once the rail and padding are taken.
#[must_use]
pub fn content_width(width: u16) -> usize {
    usize::from(width).saturating_sub(1 + PAD).max(1)
}

/// Frame `rows` as a block: a blank row, each row behind the rail and the
/// padding, a blank row. Every span without its own background gets the band's and each row
/// is filled to `width` in `text` on the band.
#[must_use]
pub fn frame(
    rows: Vec<Line<'static>>,
    rail: &Span<'static>,
    band: Style,
    width: u16,
) -> Vec<Line<'static>> {
    let fill_style = theme::palette().text.patch(band);
    let inner = usize::from(width).saturating_sub(1);
    let row = |spans: Vec<Span<'static>>| -> Line<'static> {
        let used: usize = spans.iter().map(Span::width).sum();
        let mut out = Vec::with_capacity(spans.len() + 2);
        out.push(rail.clone());
        out.extend(spans);
        let fill = inner.saturating_sub(used);
        if fill > 0 {
            out.push(Span::styled(" ".repeat(fill), fill_style));
        }
        Line::from(out)
    };
    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(row(Vec::new()));
    for line in rows {
        let mut spans = Vec::with_capacity(line.spans.len() + 1);
        spans.push(Span::styled(" ".repeat(PAD), fill_style));
        for mut span in line.spans {
            // The band fills in what the span leaves unset; a span with its
            // own background (a badge) keeps it.
            span.style = band.patch(span.style);
            spans.push(span);
        }
        lines.push(row(spans));
    }
    lines.push(row(Vec::new()));
    lines
}

/// The block: title, a blank row, `body`, a blank row, the error; the gaps
/// only between parts that are present (`gap={1}`).
#[must_use]
pub fn lines(spec: &BlockSpec<'_>, body: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let band = surface(palette, spec.hovered);
    let usable = content_width(width);
    let mut parts: Vec<Vec<Line<'static>>> = Vec::with_capacity(3);
    if let Some(title) = spec.title {
        let head: Cow<'_, str> = match spec.spinner {
            Some(glyph) => {
                let title = title.strip_prefix("# ").unwrap_or(title);
                Cow::Owned(format!("{glyph} {title}"))
            }
            None => Cow::Borrowed(title),
        };
        let style = spec.title_fg.map_or(palette.dim, |fg| palette.dim.fg(fg));
        parts.push(styled_wrap(&head, style, usable));
    }
    if !body.is_empty() {
        parts.push(body);
    }
    if let Some(error) = spec.error {
        let clean = paste::normalize(error);
        if !clean.trim().is_empty() {
            let style = spec.error_fg.map_or(palette.warn, |fg| palette.warn.fg(fg));
            parts.push(styled_wrap(&clean, style, usable));
        }
    }
    let mut rows = Vec::new();
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            rows.push(Line::default());
        }
        rows.extend(part);
    }
    frame(rows, &rail(palette, band), band, width)
}

/// Upstream's rail: `background.base` on the band, or a blank cell when that
/// colour is unknown.
fn rail(palette: &theme::Palette, band: Style) -> Span<'static> {
    match palette.backdrop {
        Some(base) => Span::styled(RAIL, band.fg(Color::Rgb(base.r, base.g, base.b))),
        None => Span::styled(" ", band),
    }
}

/// Word-wrap `text` to `width` columns; explicit newlines stay breaks.
#[must_use]
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let options = textwrap::Options::new(width.max(1))
        .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
        .word_splitter(textwrap::WordSplitter::NoHyphenation);
    textwrap::wrap(text, options)
        .into_iter()
        .map(Cow::into_owned)
        .collect()
}

fn styled_wrap(text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    wrap(text, width)
        .into_iter()
        .map(|row| Line::from(Span::styled(row, style)))
        .collect()
}

#[cfg(test)]
mod tests {
    use ratatui::text::{Line, Span};

    use super::{BlockSpec, lines, surface};
    use crate::theme;

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn texts(rows: &[Line<'_>]) -> Vec<String> {
        rows.iter().map(text).collect()
    }

    #[test]
    fn a_titled_block_pads_and_fills_every_row_to_width() {
        let spec = BlockSpec {
            title: Some("# Todos"),
            ..BlockSpec::default()
        };
        let body = vec![Line::from("[ ] one")];
        let rows = lines(&spec, body, 14);
        let words: Vec<String> = texts(&rows)
            .iter()
            .map(|row| {
                row.chars()
                    .skip(1)
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect();
        assert_eq!(words, ["", "  # Todos", "", "  [ ] one", ""]);
        for row in &rows {
            assert_eq!(row.width(), 14, "every row fills the width: {row:?}");
        }
        let p = theme::palette();
        assert_eq!(
            rows[1].spans[2].style,
            p.dim.patch(p.raised_base),
            "muted title"
        );
    }

    #[test]
    fn a_spinner_replaces_the_hash_and_sits_before_the_title() {
        let spec = BlockSpec {
            title: Some("# Questions"),
            spinner: Some('\u{2819}'),
            ..BlockSpec::default()
        };
        let rows = lines(&spec, Vec::new(), 30);
        let title: String = text(&rows[1]).chars().skip(1).collect();
        assert_eq!(title.trim_end(), "  \u{2819} Questions");
        assert_eq!(
            rows.len(),
            3,
            "no body, no error: padding and the title only"
        );
    }

    #[test]
    fn an_error_is_stripped_of_ansi_and_drawn_last_in_the_error_colour() {
        let spec = BlockSpec {
            title: Some("$ false"),
            error: Some("\u{1b}[31mexit 1\u{1b}[0m"),
            ..BlockSpec::default()
        };
        let rows = lines(&spec, vec![Line::from("out")], 20);
        let words: Vec<String> = texts(&rows).iter().map(|r| r.trim().to_owned()).collect();
        let tail: Vec<&str> = words
            .iter()
            .map(|w| w.trim_start_matches(super::RAIL).trim())
            .collect();
        assert_eq!(tail, ["", "$ false", "", "out", "", "exit 1", ""]);
        let p = theme::palette();
        assert_eq!(rows[5].spans[2].style, p.warn.patch(p.raised_base));

        let blank = BlockSpec {
            error: Some("  "),
            ..BlockSpec::default()
        };
        assert_eq!(
            lines(&blank, vec![Line::from("x")], 20).len(),
            3,
            "a blank error adds nothing"
        );
    }

    #[test]
    fn title_and_error_colours_can_be_overridden() {
        use ratatui::style::Color;
        let p = theme::palette();
        let spec = BlockSpec {
            title: Some("# Patch failed"),
            error: Some("boom"),
            title_fg: Some(Color::Rgb(1, 2, 3)),
            error_fg: Some(Color::Rgb(4, 5, 6)),
            ..BlockSpec::default()
        };
        let rows = lines(&spec, Vec::new(), 30);
        assert_eq!(rows[1].spans[2].style.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(rows[1].spans[2].style.bg, p.raised_base.bg);
        assert_eq!(rows[3].spans[2].style.fg, Some(Color::Rgb(4, 5, 6)));
    }

    #[test]
    fn hovering_moves_every_cell_to_the_raised_high_band() {
        let p = theme::palette();
        assert_eq!(surface(p, false), p.raised_base);
        assert_eq!(surface(p, true), p.panel);
        let spec = BlockSpec {
            title: Some("t"),
            hovered: true,
            ..BlockSpec::default()
        };
        let rows = lines(&spec, vec![Line::from(Span::styled("b", p.text))], 10);
        for span in rows.iter().flat_map(|row| &row.spans) {
            assert_eq!(
                span.style.bg, p.panel.bg,
                "span {span:?} keeps the hover band"
            );
        }
    }

    #[test]
    fn a_narrow_block_wraps_the_title_and_never_overflows() {
        let spec = BlockSpec {
            title: Some("# 你好世界"),
            ..BlockSpec::default()
        };
        let rows = lines(&spec, Vec::new(), 7);
        let words: Vec<String> = texts(&rows)
            .iter()
            .map(|r| r.chars().skip(1).collect::<String>().trim_end().to_owned())
            .collect();
        assert_eq!(words, ["", "  # 你", "  好世", "  界", ""]);
        for row in &rows {
            assert!(row.width() <= 7, "row overflows: {row:?}");
        }
        let zero = lines(&spec, Vec::new(), 0);
        assert!(
            zero.len() >= 3,
            "zero width still frames the title, got {}",
            zero.len()
        );
    }
}
