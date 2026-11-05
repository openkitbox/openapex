//! The enlarged image dialog (upstream `component/dialog-image-preview.tsx`).
//!
//! [`Preview`] holds one strip's images and which of them is on screen; the
//! bytes are not here. A terminal that draws images gets the current one
//! painted into the panel's body by the graphics layer, and a terminal that
//! cannot says `No preview` instead of pretending — the same shape as upstream,
//! whose `media.failed()` swaps the footer line.
//!
//! Deviation from upstream: no mouse input. Upstream's arrows step the strip
//! and a click on the backdrop closes it; ox's dialogs are keyboard-only while
//! open (the pointer gate closes with the modal, `App::pointer_enabled`), so the
//! arrows are labels and Esc closes. The thumbnail that opened this one is the
//! click surface that matters.

use std::borrow::Cow;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::Modifier;
use ratatui::widgets::{Clear, Widget};
use unicode_width::UnicodeWidthStr;

use super::dialog::{DialogOutcome, TEXT_INSET, dim_backdrop, put};
use super::media;
use crate::graphics::Graphics;
use crate::graphics::Slot;
use crate::theme::Palette;

/// `Dialog::tag` of the preview.
pub const TAG: &str = "image-preview";
/// `dialogWidth("xlarge")` (`ui/dialog.tsx:30-31`).
const PANEL_WIDTH: u16 = 116;
/// Rows the dialog chrome takes around the image: upstream asks for
/// `max(3, height - 8)` (`dialog-image-preview.tsx:20`), leaving eight rows for
/// the padding, the header and the footer.
const CHROME_ROWS: u16 = 8;
/// The image never shrinks below this many rows (upstream `Math.max(3, …)`).
const MIN_IMAGE_ROWS: u16 = 3;
const PREVIOUS: &str = "\u{2190} previous";
const NEXT: &str = "next \u{2192}";
const NO_PREVIEW: &str = "No preview";

/// One image the preview can step through.
#[derive(Debug, Clone)]
pub struct Image {
    /// The prepared-image cache key of the bytes.
    pub key: String,
    pub bytes: media::Bytes,
    /// The name the message mentioned it by, shown in the footer.
    pub name: Option<Arc<str>>,
}

/// One strip's images and which of them is on screen.
#[derive(Debug, Clone)]
pub struct Preview {
    images: Vec<Image>,
    index: usize,
    /// Keys whose bytes the app could not fetch. A failure is terminal: the
    /// app never asks twice, so the image shows `No preview` from then on.
    failed: Vec<String>,
}

impl PartialEq for Preview {
    /// Two previews are the same view of the same strip at the same image: the
    /// sources carry session ids, so the cache key is what identifies an image.
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
            && self.failed == other.failed
            && self.images.len() == other.images.len()
            && self
                .images
                .iter()
                .zip(&other.images)
                .all(|(left, right)| left.key == right.key)
    }
}

impl Preview {
    /// A preview of `images` starting at `initial`, clamped to the strip.
    #[must_use]
    pub fn new(images: Vec<Image>, initial: usize) -> Self {
        let index = initial.min(images.len().saturating_sub(1));
        Self {
            images,
            index,
            failed: Vec::new(),
        }
    }

    /// Record that `key`'s bytes will never arrive, so the image reads
    /// `No preview` instead of waiting forever for a fetch nobody repeats.
    pub fn mark_failed(&mut self, key: &str) {
        if !self.failed.iter().any(|entry| entry == key) {
            self.failed.push(key.to_owned());
        }
    }

    /// Whether the image on screen has no bytes coming.
    #[must_use]
    fn is_failed(&self) -> bool {
        self.current()
            .is_some_and(|image| self.failed.contains(&image.key))
    }

    /// The image on screen, or `None` for an empty strip.
    #[must_use]
    pub fn current(&self) -> Option<&Image> {
        self.images.get(self.index)
    }

    pub fn on_key(&mut self, key: KeyEvent) -> DialogOutcome {
        match key.code {
            KeyCode::Esc => DialogOutcome::Cancelled,
            KeyCode::Left => self.step(-1),
            KeyCode::Right => self.step(1),
            _ => DialogOutcome::Idle,
        }
    }

    /// Wrap to the next or previous image; a single image stays put.
    fn step(&mut self, direction: isize) -> DialogOutcome {
        if self.images.len() < 2 {
            return DialogOutcome::Idle;
        }
        let count = self.images.len() as isize;
        self.index = (self.index as isize + direction).rem_euclid(count) as usize;
        DialogOutcome::Changed
    }
}

/// Draw the preview over `area` and paint the current image, returning the
/// bytes still to fetch.
pub fn draw(
    buf: &mut Buffer,
    area: Rect,
    preview: &Preview,
    pal: &Palette,
    graphics: &mut Graphics,
) -> Vec<media::Request> {
    dim_backdrop(buf, area, pal);
    let count = preview.images.len();
    if count == 0 {
        return Vec::new();
    }
    let width = PANEL_WIDTH.min(area.width.saturating_sub(2));
    if width <= TEXT_INSET * 2 || area.height < CHROME_ROWS {
        return Vec::new();
    }
    // Padding, header, gap, the image, gap, footer: the panel is as tall as
    // those need and sits in the middle, as upstream's centered dialog does.
    let body_h = area.height.saturating_sub(CHROME_ROWS).max(MIN_IMAGE_ROWS);
    let panel_h = CHROME_ROWS.saturating_add(body_h).min(area.height);
    let panel = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - panel_h) / 2,
        width,
        height: panel_h,
    };
    // Clear before filling: the panel covers the transcript, whose glyphs would
    // otherwise show through every cell this one does not write.
    Clear.render(panel, buf);
    buf.set_style(panel, pal.raised_base);
    let text_x = panel.x + TEXT_INSET;
    let right = panel.right() - TEXT_INSET;
    let line_w = right - text_x;

    let esc_x = right - 3;
    put(
        buf,
        panel,
        text_x,
        panel.y + 1,
        esc_x.saturating_sub(text_x + 1),
        &format!("Image {} of {count}", preview.index + 1),
        pal.text.add_modifier(Modifier::BOLD),
    );
    put(buf, panel, esc_x, panel.y + 1, 3, "esc", pal.dim);

    let body = Rect {
        x: panel.x,
        y: panel.y + 3,
        width,
        height: body_h,
    };
    let footer_y = body.bottom() + 1;
    if count > 1 {
        put(buf, panel, text_x, footer_y, line_w, PREVIOUS, pal.dim);
        let next_w = u16::try_from(NEXT.width()).unwrap_or(line_w).min(line_w);
        put(buf, panel, right - next_w, footer_y, next_w, NEXT, pal.dim);
    }

    let Some(current) = preview.current() else {
        return Vec::new();
    };
    let mut requests = Vec::new();
    let has_bytes = graphics.enabled() && !body.is_empty() && !preview.is_failed();
    let label: Cow<'_, str> = if has_bytes {
        match &current.name {
            Some(name) => Cow::Borrowed(name),
            None => Cow::Owned(format!("Image {}", preview.index + 1)),
        }
    } else {
        Cow::Borrowed(NO_PREVIEW)
    };
    let label_style = if has_bytes { pal.dim } else { pal.warn };
    let label_w = u16::try_from(label.width()).unwrap_or(line_w).min(line_w);
    put(
        buf,
        panel,
        text_x + (line_w.saturating_sub(label_w)) / 2,
        footer_y,
        label_w,
        &label,
        label_style,
    );
    if !has_bytes {
        return requests;
    }
    // The escapes leave inside the draw closure, so they cannot interleave with
    // the cells the rest of the frame is still writing.
    match graphics.cached_for(
        &current.key,
        Size {
            width: body.width,
            height: body.height,
        },
    ) {
        Some(image) => {
            graphics.paint(
                buf,
                &[Slot {
                    area: body,
                    image: &image,
                }],
            );
            if let Err(err) = graphics.flush() {
                tracing::warn!(%err, "queued graphics escapes not written");
            }
        }
        None => requests.push(media::Request {
            key: current.key.clone(),
            bytes: current.bytes.clone(),
            cells: Size {
                width: body.width,
                height: body.height,
            },
            class: media::DIALOG,
        }),
    }
    requests
}
