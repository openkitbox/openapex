//! Attachment thumbnails, shared by the transcript and the composer.
//!
//! One strip is a row band of equal cells: at most three images, each twice as
//! wide as it is tall (upstream `SessionImages`,
//! `routes/session/index.tsx:2656-2712`), then a `+N more` label when the
//! message carries more. The band is drawn as blank rows by the block itself —
//! an image is pixels, not text — and the cells they occupy are what the
//! graphics layer paints into, so this module owns the one place their
//! geometry is computed.

use std::sync::Arc;

use ox_client::store::MediaMeta;
use ox_client::store::MediaSource;
use ratatui::layout::Rect;
use ratatui::layout::Size;

use crate::graphics::SizeClass;
use crate::graphics::probe;

/// The cache class of every strip thumbnail, so a dialog's full-size image does
/// not evict the row it was opened from (`graphics::cache::SizeClass`).
pub const THUMBNAIL: SizeClass = SizeClass::Thumbnail;
/// The cache class of the one image a dialog shows at a time.
pub const DIALOG: SizeClass = SizeClass::Dialog;

/// Images a strip shows before it counts the rest (`slice(0, 3)`).
pub const MAX_IMAGES: usize = 3;

/// Columns the `+N more` label takes, matching its upstream box (`width={8}`).
pub const MORE_WIDTH: u16 = 8;

/// Columns between two boxes of a strip (`gap={1}`).
pub const GAP: u16 = 1;

/// Columns between the rail and the strip's first box: the block's content
/// inset (`block_tool::content_width`), which the images share with the text.
pub const INSET: u16 = 3;

/// Rows a strip can be squeezed to: [`height`] clamps to this, so a strip, plus
/// the line its block draws above the images, is the smallest thing a screen row
/// budget has to hold.
const MIN_STRIP_ROWS: u16 = 4;

/// How many thumbnails a screen `terminal_height` rows tall can show at once.
///
/// The cache retains this many so a frame never asks for an image it evicted a
/// moment earlier — which would fetch, transmit and evict again on every frame.
///
/// Every strip is at least [`MIN_STRIP_ROWS`] rows plus the line its block draws
/// above it, so a screen this tall holds `terminal_height / (MIN_STRIP_ROWS + 1)`
/// of them; one more covers a strip clipped by the viewport's edge, and the
/// composer's band above the textarea is one more again. Each shows at most
/// [`MAX_IMAGES`].
#[must_use]
pub fn capacity(terminal_height: u16) -> usize {
    let strips = usize::from(terminal_height / (MIN_STRIP_ROWS + 1)) + 1;
    (strips + 1) * MAX_IMAGES
}

/// Thumbnail rows: upstream
/// `Math.max(4, Math.min(8, Math.floor(terminal.height / 4)))`, so a tall
/// terminal draws larger previews and a short one still shows a recognisable
/// image.
#[must_use]
pub fn height(terminal_height: u16) -> u16 {
    (terminal_height / 4).clamp(4, 8)
}

/// Where one image's bytes come from.
///
/// A transcript attachment is stored in a message, so its bytes come from the
/// ox-only media endpoint on demand. A composer attachment was pasted or picked
/// locally and is already in hand, so its bytes never touch the wire.
#[derive(Debug, Clone)]
pub enum Bytes {
    /// Stored in a session message; fetched from the media endpoint.
    Stored(MediaSource),
    /// Decoded from a `data:` URI the composer holds.
    Inline(Arc<[u8]>),
}

/// Whether this terminal draws images at all. Nothing reserves picture rows on
/// one that cannot, so unsupported terminals read as plain text.
#[must_use]
pub fn supported() -> bool {
    probe::capabilities().draws_images()
}

/// The cache key of one stored attachment: where its bytes come from, which is
/// also what the media endpoint is asked for.
#[must_use]
pub fn key(source: &MediaSource) -> String {
    match source {
        MediaSource::Prompt { message, index } => format!("p:{message}:{index}"),
        MediaSource::Tool {
            message,
            part,
            index,
        } => format!("t:{message}:{part}:{index}"),
    }
}

/// The bytes of one attachment the app does not have yet, as the app needs to
/// ask for them: which image (its cache key and source), and the cell grid a
/// sixel encode would have to be made for.
pub struct Request {
    pub key: String,
    pub bytes: Bytes,
    pub cells: Size,
    /// Which part of the cache the image belongs to, so the answer is filed
    /// where the dialog and the strip each look for it.
    pub class: SizeClass,
}

/// One block's thumbnail row: the cells it reserves and what each cell shows.
///
/// Built once per layout, not per frame: the keys are strings, and rebuilding
/// them for every drawn frame would allocate on the draw path.
pub struct Thumbs {
    pub strip: Strip,
    /// Every image of the message, in strip order: the strip draws the first
    /// `strip.images` of them and the dialog pages through all of them, so a
    /// `+N more` label is not a dead end.
    pub images: Vec<Image>,
}

/// One drawn thumbnail: what to fetch, what to look up, and the name the
/// enlarged dialog puts under it.
#[derive(Debug, Clone)]
pub struct Image {
    pub key: String,
    pub bytes: Bytes,
    pub name: Option<Arc<str>>,
}

impl Thumbs {
    /// The strip `files` draws at `at`, when it has any image at all.
    #[must_use]
    pub fn new(at: usize, files: &[MediaMeta], thumb_height: u16) -> Option<Self> {
        let drawn: Vec<&MediaMeta> = files
            .iter()
            .filter(|file| !file.is_dir() && file.mime.starts_with("image/"))
            .collect();
        let strip = Strip::new(at, drawn.len(), thumb_height)?;
        let images = drawn
            .into_iter()
            .map(|file| Image {
                key: key(&file.source),
                bytes: Bytes::Stored(file.source.clone()),
                name: file.name.clone(),
            })
            .collect();
        Some(Self { strip, images })
    }
}

/// The geometry of one strip inside `width` columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Strip {
    /// The row of the block's lines the strip's first image row occupies.
    pub at: usize,
    /// Rows one thumbnail is tall.
    pub height: u16,
    /// Columns one thumbnail is wide.
    pub width: u16,
    /// How many images are drawn (`min(MAX_IMAGES, total)`).
    pub images: u16,
    /// How many are left over, for the `+N more` label; zero when none are.
    pub more: usize,
}

/// What one strip shows in a band: how many cells it draws whole, and where the
/// `+N more` label goes when it fits (see [`Strip::layout`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Where the first cell's top-left corner lands.
    pub x: u16,
    /// The row the image band starts on.
    pub y: u16,
    /// Images drawn whole, from the first one.
    pub cells: u16,
    /// The `+N more` label's cell, `None` when nothing is left over or it does
    /// not fit.
    pub more: Option<Rect>,
}

impl Layout {
    /// Where image `index` lands in this layout: the draw paints it there and
    /// the pointer hit-tests the same rect.
    #[must_use]
    pub fn cell(&self, strip: &Strip, index: u16) -> Rect {
        strip.cell_at(self.x, self.y, index)
    }
}

impl Strip {
    /// The strip for `total` attachments at `thumb_height`, when `at` is where
    /// its image rows begin.
    #[must_use]
    pub fn new(at: usize, total: usize, thumb_height: u16) -> Option<Self> {
        if total == 0 {
            return None;
        }
        Some(Self {
            at,
            height: thumb_height,
            width: thumb_height * 2,
            images: u16::try_from(total.min(MAX_IMAGES)).unwrap_or(MAX_IMAGES as u16),
            more: total.saturating_sub(MAX_IMAGES),
        })
    }

    /// Where image `index` lands when the first cell starts at `(x, y)`: the
    /// block's content inset is the transcript's layout, not every surface's
    /// (the composer puts its own padding there), so [`Layout`] carries the
    /// origin and this does the arithmetic.
    #[must_use]
    pub fn cell_at(&self, x: u16, y: u16, index: u16) -> Rect {
        Rect {
            x: x.saturating_add(index.saturating_mul(self.width + GAP)),
            y,
            width: self.width,
            height: self.height,
        }
    }

    /// How many cells fit in a band `width` columns wide, measured from its left
    /// edge. A thumbnail is drawn whole or not at all — a kitty placeholder
    /// names a column inside the image — and the ones that do not fit are always
    /// the last, so a count is the whole answer.
    fn fitting(&self, width: u16) -> u16 {
        let fitted = (0..self.images)
            .filter(|index| self.cell_at(0, 0, *index).right() <= width)
            .count();
        u16::try_from(fitted).unwrap_or(self.images)
    }

    /// What a band `band` columns wide starting at `(x, y)` shows: the cells
    /// drawn whole, and the `+N more` label when it fits after them.
    ///
    /// Drawing and hit-testing both call this, so the pointer never names a
    /// cell the frame left blank and the label never sits under a cell.
    #[must_use]
    pub fn layout(&self, x: u16, y: u16, band: u16) -> Layout {
        let cells = self.fitting(band);
        let more = (self.more > 0)
            .then(|| Rect {
                x: x.saturating_add(cells.saturating_mul(self.width + GAP)),
                y,
                width: MORE_WIDTH,
                height: self.height,
            })
            .filter(|more| more.right() <= x.saturating_add(band));
        Layout { x, y, cells, more }
    }

    /// [`Self::layout`] for a block drawn at `area`: where the first cell starts
    /// (the block's content inset) is the strip's business, not the caller's, so
    /// every surface asks the same question and gets the same answer.
    #[must_use]
    pub fn layout_in(&self, area: Rect, y: u16) -> Layout {
        let x = area.x.saturating_add(INSET);
        self.layout(x, y, area.right().saturating_sub(x))
    }

    /// Whether a band `band` columns wide shows anything: a cell drawn whole or
    /// the `+N more` label. A band too narrow for either must reserve no rows at
    /// all — an empty strip is a blank hole where the text should be.
    #[must_use]
    pub fn draws_in(&self, band: u16) -> bool {
        let layout = self.layout(0, 0, band);
        layout.cells > 0 || layout.more.is_some()
    }

    /// The grid a fetch has to encode for: one image's cells, which is what
    /// sixel scales to.
    #[must_use]
    pub fn cells(&self) -> Size {
        Size::new(self.width, self.height)
    }
}
