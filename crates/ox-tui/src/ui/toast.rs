//! Toast notification (upstream `packages/tui/src/ui/toast.tsx`).
//!
//! Two halves with no shared state: [`Toast`] is the value (what to say, until
//! when), and [`render`] is a pure function from a toast and a [`Palette`] to
//! buffer cells. The app owns the single `Option<Toast>`; time enters only as
//! an `Instant` argument, so expiry is testable without sleeping.
//!
//! Deviations from upstream, all deliberate:
//! - No queue and no hover pause (`toast.tsx:121-197`): ox-tui has no mouse
//!   input, and a newer toast replaces the current one instead of waiting.
//! - No action affordance (`› label`, `toast.tsx:42`): nothing in ox-tui
//!   attaches an action, so the affordance is always the muted `x`.
//! - A terminal too narrow for the chrome draws nothing instead of overflowing.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Clear, Widget};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::Palette;

/// `toast.tsx:160`: `duration ?? 5000`.
pub const DEFAULT_DURATION: Duration = Duration::from_secs(5);

/// `toast.tsx:51`: `maxWidth = min(60, terminalWidth - 6)`.
const MAX_WIDTH: u16 = 60;
const WIDTH_MARGIN: u16 = 6;
/// `toast.tsx:49-50`: `top={1} right={2}`.
const TOP: u16 = 1;
const RIGHT: u16 = 2;
/// Left/right border (`toast.tsx:55`) plus `paddingLeft/Right={2}` (`:65-66`).
const CHROME: u16 = 6;
/// The affordance: `marginLeft={2}` plus the one-cell `x` (`toast.tsx:37,42`).
const AFFORDANCE: u16 = 3;
/// Upstream `SplitBorder.customBorderChars.vertical` (`ui/border.ts:17`).
const RAIL: &str = "\u{2503}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub variant: Variant,
    pub title: Option<String>,
    pub message: String,
    pub expires_at: Instant,
}

impl Toast {
    /// A toast shown at `now` for [`DEFAULT_DURATION`].
    pub fn new(
        variant: Variant,
        title: Option<String>,
        message: impl Into<String>,
        now: Instant,
    ) -> Self {
        Self {
            variant,
            title,
            message: message.into(),
            expires_at: now + DEFAULT_DURATION,
        }
    }

    /// An error toast with a title (`toast.show({ title, message, variant:
    /// "error" })`, e.g. `component/prompt/index.tsx:1203`).
    pub fn error(title: impl Into<String>, message: impl Into<String>, now: Instant) -> Self {
        Self::new(Variant::Error, Some(title.into()), message, now)
    }

    pub fn expired(&self, now: Instant) -> bool {
        now >= self.expires_at
    }
}

/// Border colour per variant (`toast.tsx:54`, `theme.text.feedback[variant]`).
fn rail_style(variant: Variant, pal: &Palette) -> Style {
    match variant {
        Variant::Info => pal.info,
        Variant::Success => pal.success,
        Variant::Warning => pal.warning,
        Variant::Error => pal.warn,
    }
}

/// The toast's text once it has been fitted to a width.
struct Fitted {
    width: u16,
    title: Option<String>,
    lines: Vec<String>,
}

impl Fitted {
    /// Size the toast the way flexbox sizes it upstream: shrink to content,
    /// capped at `max_width`; the message wraps to what the chrome leaves.
    /// `None` when not even one text cell fits.
    fn fit(toast: &Toast, max_width: u16) -> Option<Self> {
        let message_width = toast.message.lines().map(|l| l.width()).max().unwrap_or(0);
        let natural = match &toast.title {
            // `toast.tsx:83-92`: title + affordance on one row, message below.
            Some(title) => (title.width() + AFFORDANCE as usize).max(message_width),
            // `toast.tsx:75-79`: message and affordance share the row.
            None => message_width + AFFORDANCE as usize,
        };
        let width = u16::try_from(natural + CHROME as usize)
            .unwrap_or(u16::MAX)
            .min(max_width);
        let reserved = if toast.title.is_some() {
            CHROME
        } else {
            CHROME + AFFORDANCE
        };
        let text_width = width.checked_sub(reserved).filter(|w| *w > 0)?;
        let title = toast
            .title
            .as_deref()
            .map(|t| clip(t, usize::from((width - CHROME).saturating_sub(AFFORDANCE))));
        Some(Self {
            width,
            title,
            lines: wrap(&toast.message, usize::from(text_width)),
        })
    }

    fn height(&self) -> u16 {
        // paddingTop/Bottom 1 each; a title adds its row and `marginBottom={1}`.
        let title_rows = if self.title.is_some() { 2 } else { 0 };
        2 + title_rows + self.lines.len() as u16
    }
}

/// Word wrap (`wrapMode="word"`), keeping explicit newlines as line breaks.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let options = textwrap::Options::new(width)
        .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
        .word_splitter(textwrap::WordSplitter::NoHyphenation);
    let mut out = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            out.push(String::new());
        } else {
            out.extend(textwrap::wrap(line, &options).into_iter().map(String::from));
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// `text` cut to at most `width` terminal cells.
fn clip(text: &str, width: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        out.push(ch);
    }
    out
}

/// Draw `toast` at the top right of `area` (`toast.tsx:49-50`), over whatever
/// is there. Draws nothing when `area` is too small for the chrome.
pub fn render(buf: &mut Buffer, area: Rect, toast: &Toast, pal: &Palette) {
    let max_width = area.width.saturating_sub(WIDTH_MARGIN).min(MAX_WIDTH);
    let Some(fitted) = Fitted::fit(toast, max_width) else {
        return;
    };
    let y = area.y.saturating_add(TOP);
    if y >= area.bottom() {
        return;
    }
    let rect = Rect {
        x: area
            .right()
            .saturating_sub(RIGHT + fitted.width)
            .max(area.x),
        y,
        width: fitted.width,
        height: fitted.height().min(area.bottom() - y),
    };

    Clear.render(rect, buf);
    let surface = pal.panel;
    let text = surface.patch(pal.text);
    let rail = rail_style(toast.variant, pal);
    let right = rect.right() - 1;
    for row in rect.y..rect.bottom() {
        buf.set_string(rect.x, row, RAIL, rail);
        buf.set_string(right, row, RAIL, rail);
        buf.set_style(
            Rect {
                x: rect.x + 1,
                y: row,
                width: rect.width - 2,
                height: 1,
            },
            surface,
        );
    }

    let left = rect.x + 3;
    let affordance_x = right - 2 - 1;
    let mut row = rect.y + 1;
    let put = |buf: &mut Buffer, row: u16, x: u16, s: &str, style: Style| {
        if row < rect.bottom() - 1 {
            buf.set_string(x, row, s, style);
        }
    };
    if let Some(title) = &fitted.title {
        put(buf, row, left, title, text.add_modifier(Modifier::BOLD));
        put(buf, row, affordance_x, "x", surface.patch(pal.dim));
        row += 2;
    } else {
        put(buf, row, affordance_x, "x", surface.patch(pal.dim));
    }
    for line in &fitted.lines {
        put(buf, row, left, line, text);
        row += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};

    use super::{DEFAULT_DURATION, Toast, Variant, render};
    use crate::theme::{Deck, Palette};

    fn pal() -> Palette {
        Palette::for_deck(Deck::Truecolor)
    }

    fn toast(variant: Variant, title: Option<&str>, message: &str) -> Toast {
        Toast::new(variant, title.map(String::from), message, Instant::now())
    }

    fn draw(width: u16, height: u16, toast: &Toast) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, toast, &pal());
        buf
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn expires_exactly_at_the_deadline() {
        let t0 = Instant::now();
        let t = Toast::new(Variant::Error, None, "boom", t0);
        assert_eq!(DEFAULT_DURATION, Duration::from_millis(5000));
        assert!(!t.expired(t0));
        assert!(!t.expired(t0 + DEFAULT_DURATION - Duration::from_millis(1)));
        assert!(t.expired(t0 + DEFAULT_DURATION));
        assert!(t.expired(t0 + DEFAULT_DURATION * 2));
    }

    #[test]
    fn short_message_sits_top_right_like_upstream() {
        // Gold: tests/parity/gold/gold-toast-30x100.json rows 1-3, cols 68-97.
        let buf = draw(
            100,
            8,
            &toast(Variant::Error, None, "Directory is required"),
        );
        assert_eq!(row(&buf, 0).trim(), "");
        assert_eq!(
            row(&buf, 1).trim_end(),
            format!("{}\u{2503}{}\u{2503}", " ".repeat(68), " ".repeat(28))
        );
        assert_eq!(
            &row(&buf, 2)[68..],
            "\u{2503}  Directory is required  x  \u{2503}  "
        );
        assert_eq!(row(&buf, 4).trim(), "");
    }

    #[test]
    fn long_messages_wrap_at_max_width_minus_the_affordance() {
        let message = "word ".repeat(30);
        let buf = draw(100, 14, &toast(Variant::Error, None, message.trim()));
        // Width is capped at 60 (cols 38..=97); text gets 60 - 6 - 3 = 51.
        assert_eq!(buf[(38, 1)].symbol(), "\u{2503}");
        assert_eq!(buf[(97, 1)].symbol(), "\u{2503}");
        assert_eq!(buf[(37, 1)].symbol(), " ");
        let first: String = (41..92).map(|x| buf[(x, 2)].symbol()).collect();
        assert_eq!(first.trim_end(), "word ".repeat(10).trim_end());
        assert_eq!(buf[(94, 2)].symbol(), "x");
        // 149 chars of text over 51-wide lines: 3 rows (2-4), padding row 5, no more.
        assert_eq!(buf[(38, 5)].symbol(), "\u{2503}");
        assert_eq!(buf[(38, 6)].symbol(), " ");
    }

    #[test]
    fn explicit_newlines_stay_line_breaks() {
        let buf = draw(100, 8, &toast(Variant::Info, None, "one\ntwo"));
        assert!(row(&buf, 2).contains("one"));
        assert!(row(&buf, 3).contains("two"));
        assert_eq!(buf[(98, 4)].symbol(), " ");
        assert!(
            row(&buf, 4).contains('\u{2503}'),
            "padding row 4 is the last"
        );
        assert_eq!(row(&buf, 5).trim(), "");
    }

    #[test]
    fn title_gets_its_own_bold_row_and_a_blank_row_below() {
        let buf = draw(
            100,
            10,
            &toast(
                Variant::Error,
                Some("Creating a session failed"),
                "server down",
            ),
        );
        let title = row(&buf, 2);
        assert!(title.contains("Creating a session failed"));
        assert!(title.trim_end().ends_with("x  \u{2503}"));
        let start = title.chars().position(|c| c == 'C').unwrap() as u16;
        assert!(buf[(start, 2)].modifier.contains(Modifier::BOLD));
        assert_eq!(
            row(&buf, 3).trim_matches(|c| c == ' ' || c == '\u{2503}'),
            ""
        );
        assert!(row(&buf, 4).contains("server down"));
        assert!(!buf[(start, 4)].modifier.contains(Modifier::BOLD));
        assert_eq!(row(&buf, 6).trim(), "");
    }

    #[test]
    fn each_variant_has_its_own_rail_colour() {
        let p = pal();
        let rail = |variant| {
            let buf = draw(100, 6, &toast(variant, None, "m"));
            // Rail cell and surface cell.
            (buf[(88, 2)].fg, buf[(89, 2)].bg)
        };
        let colours: Vec<Color> = [
            Variant::Info,
            Variant::Success,
            Variant::Warning,
            Variant::Error,
        ]
        .into_iter()
        .map(|v| rail(v).0)
        .collect();
        assert_eq!(colours[0], p.info.fg.unwrap());
        assert_eq!(colours[1], p.success.fg.unwrap());
        assert_eq!(colours[2], p.warning.fg.unwrap());
        assert_eq!(colours[3], p.warn.fg.unwrap());
        for (i, a) in colours.iter().enumerate() {
            for b in &colours[i + 1..] {
                assert_ne!(a, b, "variants must be distinguishable");
            }
        }
        // The surface is the raised panel; the rail keeps the terminal background.
        assert_eq!(rail(Variant::Error).1, p.panel.bg.unwrap());
        let buf = draw(100, 6, &toast(Variant::Error, None, "m"));
        assert_eq!(buf[(88, 2)].bg, Color::Reset);
    }

    #[test]
    fn narrow_terminals_shrink_the_toast_or_skip_it() {
        // 30 cols: max width 24, text width 24 - 9 = 15.
        let buf = draw(30, 8, &toast(Variant::Error, None, "Directory is required"));
        assert_eq!(buf[(4, 1)].symbol(), "\u{2503}");
        assert_eq!(buf[(27, 1)].symbol(), "\u{2503}");
        assert!(row(&buf, 2).contains("Directory is"));
        assert!(row(&buf, 3).contains("required"));
        // 15 cols: nothing fits next to the chrome, so nothing is drawn.
        let buf = draw(15, 8, &toast(Variant::Error, None, "Directory is required"));
        assert!((0..8).all(|y| row(&buf, y).trim().is_empty()));
        // A one-row area has no room below the top margin.
        let buf = draw(100, 1, &toast(Variant::Error, None, "x"));
        assert!(row(&buf, 0).trim().is_empty());
    }

    #[test]
    fn a_tall_toast_is_clipped_to_the_area() {
        let message = "line\n".repeat(20);
        let buf = draw(100, 5, &toast(Variant::Warning, None, &message));
        assert_eq!(buf[(97, 4)].symbol(), "\u{2503}");
    }

    #[test]
    fn long_titles_are_clipped_not_overflowed() {
        let title = "t".repeat(100);
        let buf = draw(100, 10, &toast(Variant::Error, Some(&title), "m"));
        assert_eq!(buf[(97, 2)].symbol(), "\u{2503}");
        assert_eq!(buf[(94, 2)].symbol(), "x");
        assert_eq!(buf[(98, 2)].symbol(), " ");
    }

    #[test]
    fn no_color_deck_still_draws_the_rail_and_text() {
        let area = Rect::new(0, 0, 100, 6);
        let mut buf = Buffer::empty(area);
        let pal = Palette::for_deck(Deck::NoColor);
        render(&mut buf, area, &toast(Variant::Error, None, "m"), &pal);
        assert_eq!(buf[(88, 2)].symbol(), "\u{2503}");
        assert!(row(&buf, 2).contains("m  x"));
    }
}
