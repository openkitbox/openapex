//! Terminal graphics: the probe, the two protocol encoders, the prepared-image
//! cache, and the per-frame queue of escape sequences.
//!
//! Kitty draws nothing until the image itself is on the terminal, and the cells
//! that *show* an image are ordinary ratatui cells written by `terminal.draw`,
//! so the order inside a frame is fixed: [`Graphics::paint`] writes the
//! placeholder cells into the buffer and queues whatever transmission they need,
//! then the escapes are flushed, then the frame is drawn. [`Graphics::flush`]
//! writes the queue to stdout from the draw loop's own thread and the same file
//! descriptor crossterm's backend uses, so nothing can interleave with a frame.
//! A sixel needs no queue at all: its escape is the cell symbol itself, written
//! by [`Graphics::paint`] as part of the ordinary frame.
//!
//! A terminal that reported neither kitty nor sixel is left alone: no
//! placeholder rows, no fallback that would misrepresent the image.

use std::io;
use std::io::Write;
use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::layout::Size;

use probe::Capabilities;
use probe::Protocol;

pub mod cache;
pub mod kitty;
pub mod png;
pub mod probe;
pub mod sixel;

pub use cache::Cache;
pub use cache::Lookup;
pub use cache::Prepared;
pub use cache::SizeClass;

/// One image the frame wants on screen: where it goes, and what to draw there.
pub struct Slot<'a> {
    pub area: Rect,
    pub image: &'a Prepared,
}

/// Escape sequences that must reach the terminal before the next draw.
///
/// It is one flat byte buffer because the write is one syscall: a transmission
/// can be several megabytes and must not be split by a half-drawn frame.
///
/// The queue also owns the passthrough decision: under tmux or screen every
/// kitty escape needs the multiplexer's DCS wrapper, for the transmissions it
/// carries and for the deletes it sends, so it is settled once here rather than
/// at each escape.
pub struct Queue {
    bytes: Vec<u8>,
    wrapper: kitty::Wrapper,
}

impl Queue {
    #[must_use]
    pub fn new(wrapper: kitty::Wrapper) -> Self {
        Self {
            bytes: Vec::new(),
            wrapper,
        }
    }

    /// How kitty escapes leave this queue; [`Cache::insert_kitty`] wraps the
    /// payload it builds for this queue with it.
    #[must_use]
    pub fn wrapper(&self) -> kitty::Wrapper {
        self.wrapper
    }

    /// Queue deleting image `id`, so the terminal stops holding it.
    pub fn delete(&mut self, id: u32) {
        self.bytes
            .extend_from_slice(&kitty::delete(id, self.wrapper));
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Write everything queued, keeping it if the write fails so the next frame
    /// retries rather than silently dropping an image the cells still name.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.bytes.is_empty() {
            return Ok(());
        }
        let mut stdout = io::stdout().lock();
        stdout.write_all(&self.bytes)?;
        stdout.flush()?;
        self.bytes.clear();
        Ok(())
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }
}

/// The TUI's graphics state: the probe's answer, the cache, and this frame's
/// queue.
pub struct Graphics {
    capabilities: &'static Capabilities,
    cache: Cache,
    queue: Queue,
}

impl Graphics {
    /// Reads the probe result, which [`probe::init`] must have stored at
    /// startup; without it this is the unsupported state.
    #[must_use]
    pub fn new() -> Self {
        let capabilities = probe::capabilities();
        Self {
            capabilities,
            cache: Cache::new(),
            queue: Queue::new(wrapper(capabilities)),
        }
    }

    /// Whether images can be drawn at all. When false the transcript shows
    /// images as text and never reserves placeholder rows.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.capabilities.draws_images()
    }

    /// The prepared form of `png` for media `key`, transmitting it on first use.
    ///
    /// Kitty only: one transmission serves every size, so this is cheap enough
    /// for the UI thread. A sixel encodes per grid, so it goes through
    /// [`sixel::prepare`] on a blocking thread and returns with [`Self::adopt`].
    ///
    /// `png` must already be PNG — the transmission says `f=100` — which is what
    /// [`png::ensure`] hands a fetched or pasted payload before it gets here.
    /// `None` when this terminal cannot draw images or the payload alone is
    /// larger than the cache's whole budget.
    pub fn image(&mut self, key: &str, class: SizeClass, png: &[u8]) -> Option<Arc<Prepared>> {
        if self.capabilities.protocol != Protocol::Kitty {
            return None;
        }
        self.cache.insert_kitty(key, class, png, &mut self.queue)
    }

    /// Adopt a sixel image [`sixel::prepare`] encoded for media `key`.
    ///
    /// `None` when this terminal does not draw sixel images, or when the cache
    /// dropped the encode because it alone exceeds the byte budget.
    pub fn adopt(
        &mut self,
        key: &str,
        class: SizeClass,
        prepared: Prepared,
    ) -> Option<Arc<Prepared>> {
        if self.capabilities.protocol != Protocol::Sixel {
            return None;
        }
        self.cache.insert(key, class, prepared, &mut self.queue)
    }

    /// The cached image `lookup` names, marking it most recently used.
    pub fn cached(&mut self, lookup: Lookup<'_>) -> Option<Arc<Prepared>> {
        if !self.enabled() {
            return None;
        }
        self.cache.get(lookup)
    }

    /// The cached image for media `key` drawn over `cells`, or `None` while it
    /// is not cached yet.
    ///
    /// The grid only belongs to the key for sixel, whose encode cannot be
    /// reused at another size; a kitty transmission is scaled by the terminal
    /// and answers any grid, so callers pass the one they are about to paint
    /// and need not know which protocol answered.
    pub fn cached_for(&mut self, key: &str, cells: Size) -> Option<Arc<Prepared>> {
        match self.capabilities.protocol {
            Protocol::Sixel => self.cached(Lookup::Cells(key, cells)),
            _ => self.cached(Lookup::Media(key)),
        }
    }

    /// Whether an image must be encoded for its grid before it can be cached,
    /// in which case a fetch has to carry the grid it is for.
    #[must_use]
    pub fn encodes_sixel(&self) -> bool {
        self.capabilities.protocol == Protocol::Sixel
    }

    /// Forget every image: the cells that named them are gone (another session
    /// was attached), and their deletes leave with the next frame's queue.
    pub fn clear(&mut self) {
        self.cache.clear(&mut self.queue);
    }

    /// Keep `thumbnails` thumbnails resident, as many as a frame of this screen
    /// can show (`ui::media::capacity`).
    pub fn reserve_thumbnails(&mut self, thumbnails: usize) {
        self.cache.set_thumbnails(thumbnails);
    }

    /// Forget every image and hand the terminal their deletes now.
    ///
    /// Kitty keeps a transmitted image until it is told to drop it, and no frame
    /// follows the one that ends the loop, so the last frame's queue is not
    /// enough: the deletes have to be settled and written here or every image
    /// the session ever showed outlives the TUI.
    pub fn release(&mut self) -> io::Result<()> {
        self.cache.retire(&mut self.queue);
        self.cache.clear(&mut self.queue);
        // `clear` defers an id whose image a slot still holds; by now nothing
        // does, so one more pass pays every debt it just recorded.
        self.cache.retire(&mut self.queue);
        self.flush()
    }

    /// Draw this frame's images into `buf` and queue the transmissions they need.
    /// Call it while building the frame, then [`Self::flush`] before
    /// `terminal.draw`.
    ///
    /// A slot with no visible cell draws nothing, so it queues nothing either:
    /// transmitting a multi-megabyte payload for an image that is scrolled off
    /// or squeezed to zero cells would cost the terminal a transfer it can never
    /// show. That also leaves the payload untransmitted, so a slot that becomes
    /// visible on a later frame still claims it exactly once.
    ///
    /// The deletes eviction deferred are queued first, so an id is only removed
    /// once no image in this frame's slots can still paint it.
    pub fn paint(&mut self, buf: &mut Buffer, slots: &[Slot<'_>]) {
        if !self.enabled() {
            return;
        }
        self.cache.retire(&mut self.queue);
        for slot in slots {
            if buf.area.intersection(slot.area).is_empty() {
                continue;
            }
            if let Some(payload) = slot.image.claim_transmit() {
                self.queue.push(payload);
            }
            slot.image.paint(slot.area, buf);
        }
    }

    /// Write the queued escapes to stdout.
    pub fn flush(&mut self) -> io::Result<()> {
        self.queue.flush()
    }
}

impl Default for Graphics {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Graphics {
    /// A draw error or a panic leaves the loop without reaching
    /// [`Self::release`], and a kitty image outlives the TUI in the terminal
    /// unless it is deleted, so the last thing this type does is try anyway.
    fn drop(&mut self) {
        if let Err(err) = self.release() {
            tracing::warn!(%err, "image deletes not written on shutdown");
        }
    }
}

/// How kitty escapes must leave this terminal.
///
/// tmux and screen drop escape sequences they do not understand unless they
/// arrive inside their passthrough wrapper (`\x1bPtmux;` … `\x1b\\`, inner
/// escapes doubled), and graphics support inside tmux arrives that way too.
/// Decided once from the probe so no encoder or the draw path has to look at
/// the environment.
fn wrapper(capabilities: &Capabilities) -> kitty::Wrapper {
    if capabilities.tmux {
        kitty::Wrapper::Tmux
    } else {
        kitty::Wrapper::Direct
    }
}
