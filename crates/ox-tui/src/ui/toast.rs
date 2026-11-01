//! Toast notification (upstream `packages/tui/src/ui/toast.tsx`).
//!
//! Two halves with no shared state: [`Toast`] is the value (what to say, until
//! when), and [`render`] is a pure function from a toast and a [`Palette`] to
//! buffer cells. The app owns the single `Option<Toast>`; time enters only as
//! an `Instant` argument, so expiry is testable without sleeping.
//!
//! Deviations from upstream, all deliberate:
//! - No queue and no hover pause (`toast.tsx:121-197`): a newer toast replaces
//!   the current one instead of waiting, and hovering the toast does not
//!   stop its timer.
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

    #[must_use]
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
