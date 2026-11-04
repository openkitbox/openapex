//! Prepared images: what the terminal has been handed, and what to forget.
//!
//! Two limits hold at once — bytes of encoded payload, and how many entries of
//! each size class stay resident — so a transcript full of thumbnails cannot
//! evict the one image a dialog is showing, and one absurd payload cannot take
//! the whole budget. Both limits are settled before an insert: the class count
//! first, because it is a limit on that class alone, then the byte budget over
//! whatever is still resident.
//!
//! Kitty keeps a transmitted image in its own memory until it is deleted, so an
//! evicted entry owes the terminal a delete or the image leaks for the life of
//! the terminal session. A frame's slot list may still hold the evicted image's
//! [`Arc`] and paint its placeholders during the frame that evicted it, so the
//! delete waits until no living clone is left: a [`Weak`] reference records the
//! debt and [`Cache::retire`] pays it, per frame, once the strong count drops to
//! zero. The invariant is that no placeholder cell is ever painted for an id
//! that has already been deleted. A sixel has no such debt, because its escape
//! lives in the frame's cells rather than in the terminal between frames.
//!
//! What an entry is addressed by depends on the protocol. A kitty `U=1` virtual
//! placement is scaled by the terminal to whatever grid of placeholder cells
//! draws it, so the media alone identifies it and the size class only decides
//! retention. A sixel is a raster encoded for one cell grid, so the grid is part
//! of its identity: [`Lookup::Cells`] finds only the encode made for that size,
//! and [`Lookup::Media`] never matches a sixel.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui_image::protocol::StatefulProtocol;

use crate::graphics::Queue;
use crate::graphics::kitty;

/// How large a view of the image is. Only the retention policy cares, but a
/// fetch is filed by it too, so it has to name itself in a set.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum SizeClass {
    /// A transcript row's thumbnail.
    Thumbnail,
    /// The one image a dialog or viewer shows at once.
    Dialog,
}

/// Resident entries per size class.
const THUMBNAILS: usize = 8;
const DIALOGS: usize = 1;

/// Payload bytes the cache may hold.
const BYTE_BUDGET: usize = 48 * 1024 * 1024;

/// What a caller is asking the cache for.
///
/// A kitty transmission is scaled by the terminal, so the media alone is the
/// key. A sixel encode is only valid for the grid it was made for, so the grid
/// is part of the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup<'media> {
    /// Any size: only a kitty transmission answers this.
    Media(&'media str),
    /// This exact cell grid: only a sixel encode answers this.
    Cells(&'media str, Size),
}

/// An image in the form its protocol can draw.
pub struct Prepared {
    kind: Kind,
}

/// The protocol-specific state, which is all a [`Prepared`] is.
enum Kind {
    /// The transmission escapes, and the id they name.
    Kitty {
        id: u32,
        payload: Vec<u8>,
        /// Set when the escapes were queued for the terminal: later frames draw
        /// the same cells and must not queue a multi-kilobyte payload again.
        transmitted: AtomicBool,
    },
    /// A sixel encode: the escape is the whole image, so it stays here and is
    /// written into the buffer when the frame draws it. Boxed because the
    /// stateful protocol is an order of magnitude larger than the kitty arm, and
    /// every cache entry would otherwise carry the difference.
    Sixel(Box<Encode>),
}

/// A sixel encode, which is valid for exactly one cell grid.
struct Encode {
    /// Guarded because drawing walks the stateful protocol mutably while the
    /// frame holds only a shared borrow of the image.
    protocol: Mutex<StatefulProtocol>,
    /// The grid it was encoded for; also part of its cache key.
    cells: Size,
    /// The length of the escape it holds. The protocol is never re-encoded after
    /// [`super::sixel::prepare`], so this cannot drift.
    bytes: usize,
}

impl Prepared {
    /// A kitty image whose escapes have not been queued yet.
    fn kitty(id: u32, payload: Vec<u8>) -> Self {
        Self {
            kind: Kind::Kitty {
                id,
                payload,
                transmitted: AtomicBool::new(false),
            },
        }
    }

    /// A sixel image encoded for `cells`, `bytes` of escape payload.
    pub(super) fn sixel(protocol: StatefulProtocol, cells: Size, bytes: usize) -> Self {
        Self {
            kind: Kind::Sixel(Box::new(Encode {
                protocol: Mutex::new(protocol),
                cells,
                bytes,
            })),
        }
    }

    /// The grid a sixel encode is valid for; `None` for a kitty transmission,
    /// which the terminal scales itself.
    fn cells(&self) -> Option<Size> {
        match &self.kind {
            Kind::Kitty { .. } => None,
            Kind::Sixel(encode) => Some(encode.cells),
        }
    }

    /// The image id the terminal knows this by, when there is one to delete.
    fn id(&self) -> Option<u32> {
        match &self.kind {
            Kind::Kitty { id, .. } => Some(*id),
            Kind::Sixel(_) => None,
        }
    }

    fn bytes(&self) -> usize {
        match &self.kind {
            Kind::Kitty { payload, .. } => payload.len(),
            Kind::Sixel(encode) => encode.bytes,
        }
    }

    /// The transmission escapes, on the first call only.
    ///
    /// Kitty only: a sixel writes itself into the buffer instead. The flag is
    /// set when the escapes are queued, not when they are written; the queue is
    /// flushed into the same draw loop right after, so the only way to observe
    /// the difference is a broken stdout.
    pub(super) fn claim_transmit(&self) -> Option<&[u8]> {
        let Kind::Kitty {
            payload,
            transmitted,
            ..
        } = &self.kind
        else {
            return None;
        };
        if transmitted.swap(true, Ordering::Relaxed) {
            None
        } else {
            Some(payload)
        }
    }

    /// Draw the image over `area` in the cells the terminal will read.
    ///
    /// The two protocols could not differ more: kitty draws placeholder cells
    /// that name an image the terminal already holds, while sixel puts the whole
    /// escape in the first cell and marks the rest skipped.
    pub(super) fn paint(&self, area: Rect, buf: &mut Buffer) {
        use ratatui_image::ResizeEncodeRender as _;

        match &self.kind {
            Kind::Kitty { id, .. } => kitty::place(area, buf, *id),
            Kind::Sixel(encode) => {
                let mut protocol = encode
                    .protocol
                    .lock()
                    .expect("no frame panics while holding the render lock");
                protocol.render(area, buf);
            }
        }
    }
}

/// The prepared images, least recently used first.
pub struct Cache {
    ids: kitty::Ids,
    entries: Vec<Entry>,
    /// Evicted kitty images whose id is still owed to the terminal, with the
    /// only reference left that could still paint them.
    retired: Vec<(u32, Weak<Prepared>)>,
    bytes: usize,
    /// Entries of the thumbnail class to keep. The caller owns the screen, so it
    /// knows how many thumbnails a frame can show and sizes the class to that:
    /// a class smaller than one frame's demand turns every frame into a fetch,
    /// a transmission and an eviction of an image still on screen.
    thumbnails: usize,
}

/// One cached image and the bookkeeping the retention policy needs.
struct Entry {
    media: String,
    class: SizeClass,
    image: Arc<Prepared>,
}

impl Entry {
    fn matches(&self, lookup: Lookup<'_>) -> bool {
        match lookup {
            Lookup::Media(media) => self.media == media && self.image.cells().is_none(),
            Lookup::Cells(media, cells) => self.media == media && self.image.cells() == Some(cells),
        }
    }
}

impl Cache {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ids: kitty::Ids::new(),
            entries: Vec::new(),
            retired: Vec::new(),
            bytes: 0,
            thumbnails: THUMBNAILS,
        }
    }

    /// Keep `thumbnails` entries of the thumbnail class, which must cover the
    /// thumbnails one frame can show (`ui::media::capacity`).
    pub fn set_thumbnails(&mut self, thumbnails: usize) {
        self.thumbnails = thumbnails.max(1);
    }

    /// The prepared image `lookup` names, marking it most recently used.
    pub fn get(&mut self, lookup: Lookup<'_>) -> Option<Arc<Prepared>> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.matches(lookup))?;
        let entry = self.entries.remove(index);
        let image = Arc::clone(&entry.image);
        self.entries.push(entry);
        Some(image)
    }

    /// Queue the deletes eviction deferred, for every id nothing can paint any
    /// more.
    ///
    /// Call it once per frame, before the frame's slots paint: a clone that is
    /// still alive means a slot list, and possibly this frame's cells, still
    /// name the image. An id is deleted only when its image is unreachable, so
    /// no placeholder cell can outlive the image it draws.
    pub fn retire(&mut self, queue: &mut Queue) {
        self.retired.retain(|(id, image)| {
            if image.strong_count() == 0 {
                queue.delete(*id);
                false
            } else {
                true
            }
        });
    }

    /// Drop every entry, queueing the deletes of the ids nothing else can paint.
    ///
    /// Unlike eviction this is not a budget decision: the caller knows the cells
    /// naming these images left the screen (another session was attached), so
    /// the terminal must forget them now rather than after the next eviction.
    pub fn clear(&mut self, queue: &mut Queue) {
        for entry in std::mem::take(&mut self.entries) {
            match entry.image.id() {
                Some(id) if Arc::strong_count(&entry.image) == 1 => queue.delete(id),
                Some(id) => self.retired.push((id, Arc::downgrade(&entry.image))),
                None => {}
            }
        }
        self.bytes = 0;
    }

    /// The kitty form of `png` for media `media`, transmitting it on first use.
    ///
    /// `None` when the payload alone exceeds the byte budget (caching it would
    /// evict everything else and still break the budget) or when every image id
    /// has been used. Any id an eviction owes the terminal is queued here or
    /// deferred until [`Self::retire`] can pay it.
    pub fn insert_kitty(
        &mut self,
        media: &str,
        class: SizeClass,
        png: &[u8],
        queue: &mut Queue,
    ) -> Option<Arc<Prepared>> {
        if let Some(existing) = self.get(Lookup::Media(media)) {
            return Some(existing);
        }
        // The budget is settled before an id leaves the pool: ids are never
        // reused, so one spent on an image the cache then drops is lost for the
        // life of the process (and the terminal would hold a transmission
        // nothing can draw or delete).
        if kitty::payload_bytes(png) > BYTE_BUDGET {
            return None;
        }
        let id = self.ids.take()?;
        let payload = kitty::transmit(id, png, queue.wrapper());
        self.insert(media, class, Prepared::kitty(id, payload), queue)
    }

    /// One prepared image, in whichever form its protocol draws, transmitting it
    /// on first paint.
    ///
    /// `None` when it cannot be kept: a sixel payload can only be measured after
    /// the fact, so an oversized encode is dropped here and the caller shows its
    /// no-preview state. Returns the resident entry when its key is already here,
    /// so an encode is never stored twice for one grid.
    pub fn insert(
        &mut self,
        media: &str,
        class: SizeClass,
        prepared: Prepared,
        queue: &mut Queue,
    ) -> Option<Arc<Prepared>> {
        let lookup = match &prepared.kind {
            Kind::Kitty { .. } => Lookup::Media(media),
            Kind::Sixel(encode) => Lookup::Cells(media, encode.cells),
        };
        if let Some(existing) = self.get(lookup) {
            return Some(existing);
        }
        let incoming = Incoming {
            class,
            bytes: prepared.bytes(),
        };
        if incoming.bytes > BYTE_BUDGET {
            return None;
        }
        let resident: Vec<(SizeClass, usize)> = self
            .entries
            .iter()
            .map(|entry| (entry.class, entry.image.bytes()))
            .collect();
        for index in evictions(&resident, incoming, self.thumbnails).iter().rev() {
            let evicted = self.entries.remove(*index);
            self.bytes -= evicted.image.bytes();
            if let Some(id) = evicted.image.id() {
                // A slot list may still hold this image and paint it during
                // this or a later frame, so the id may only go once nothing
                // references it. `retire` pays the debt then.
                if Arc::strong_count(&evicted.image) == 1 {
                    queue.delete(id);
                } else {
                    self.retired.push((id, Arc::downgrade(&evicted.image)));
                }
            }
        }
        let image = Arc::new(prepared);
        self.bytes += incoming.bytes;
        self.entries.push(Entry {
            media: media.to_owned(),
            class,
            image: Arc::clone(&image),
        });
        Some(image)
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

/// One image arriving at the cache.
#[derive(Clone, Copy)]
struct Incoming {
    class: SizeClass,
    bytes: usize,
}

/// Which resident entries an incoming image pushes out, least recently used
/// first.
///
/// Pure: the caller turns the indices into removals and deletes. `resident` is
/// ordered least to most recently used. The two limits are applied in turn
/// because they answer different questions: the class count only frees slots of
/// the incoming image's own class, and the byte budget then frees whatever is
/// left, oldest first, until the payload fits. Settling the count first is what
/// keeps a full class from overflowing just because an entry of another class
/// happened to sort ahead of it.
#[must_use]
fn evictions(resident: &[(SizeClass, usize)], incoming: Incoming, thumbnails: usize) -> Vec<usize> {
    let cap = |class| match class {
        SizeClass::Thumbnail => thumbnails,
        SizeClass::Dialog => DIALOGS,
    };
    let limit = cap(incoming.class);
    let mut live = vec![true; resident.len()];
    let mut class_count = resident
        .iter()
        .filter(|(class, _)| *class == incoming.class)
        .count();
    let mut doomed = Vec::new();

    // One entry of the class makes room for the incoming one.
    for (index, (class, _)) in resident.iter().enumerate() {
        if class_count < limit {
            break;
        }
        if *class != incoming.class {
            continue;
        }
        live[index] = false;
        class_count -= 1;
        doomed.push(index);
    }

    let mut bytes: usize = resident
        .iter()
        .enumerate()
        .filter(|(index, _)| live[*index])
        .map(|(_, (_, entry_bytes))| entry_bytes)
        .sum();
    for (index, (_, entry_bytes)) in resident.iter().enumerate() {
        if bytes + incoming.bytes <= BYTE_BUDGET {
            break;
        }
        if !live[index] {
            continue;
        }
        live[index] = false;
        bytes -= entry_bytes;
        doomed.push(index);
    }
    doomed
}

#[cfg(test)]
mod tests {
    use super::*;

    const THUMBNAIL: SizeClass = SizeClass::Thumbnail;
    const DIALOG: SizeClass = SizeClass::Dialog;

    /// Eviction takes the least recently used entry, frees a class slot only
    /// with an entry of that class, and keeps the budget.
    #[test]
    fn eviction_order_is_least_recently_used_first() {
        let wanted = |resident: &[(SizeClass, usize)], class, bytes| {
            evictions(resident, Incoming { class, bytes }, THUMBNAILS)
        };

        // Room left in the class and in the budget: nothing goes.
        assert!(
            wanted(&[(THUMBNAIL, 10), (DIALOG, 10)], THUMBNAIL, 10).is_empty(),
            "a resident set under both limits evicts nothing"
        );

        // A full thumbnail class evicts exactly its least recently used entry.
        let full: Vec<(SizeClass, usize)> =
            std::iter::repeat_n((THUMBNAIL, 10), THUMBNAILS).collect();
        assert_eq!(wanted(&full, THUMBNAIL, 10), vec![0]);

        // The single dialog slot goes to the newest dialog image, whatever is
        // resident behind it.
        assert_eq!(
            wanted(&[(DIALOG, 10), (THUMBNAIL, 10)], DIALOG, 10),
            vec![0]
        );

        // A full budget evicts the oldest entry even when its class has room.
        let heavy: Vec<(SizeClass, usize)> = vec![(THUMBNAIL, BYTE_BUDGET), (DIALOG, 0)];
        assert_eq!(wanted(&heavy, THUMBNAIL, 1), vec![0]);
    }

    /// A full thumbnail class must still evict its own oldest entry when a
    /// dialog sits ahead of it and the byte budget already fits. Breaking out of
    /// the eviction pass at the first entry of another class is how the ninth
    /// thumbnail used to get in.
    #[test]
    fn a_full_class_evicts_its_own_entry_behind_another_class() {
        let mut resident: Vec<(SizeClass, usize)> = vec![(DIALOG, 10)];
        resident.extend(std::iter::repeat_n((THUMBNAIL, 10), THUMBNAILS));

        let doomed = evictions(
            &resident,
            Incoming {
                class: THUMBNAIL,
                bytes: 10,
            },
            THUMBNAILS,
        );
        assert_eq!(
            doomed,
            vec![1],
            "only the oldest thumbnail goes: {} thumbnails are resident",
            THUMBNAILS
        );
        assert_eq!(resident[doomed[0]].0, THUMBNAIL, "a dialog is not a slot");
    }

    /// A kitty entry answers any size and a sixel entry only its own grid, which
    /// is the whole reason the two protocols are keyed differently.
    #[test]
    fn a_sixel_encode_only_matches_its_own_grid() {
        use ratatui_image::FontSize;
        use ratatui_image::protocol::StatefulProtocolType;
        use ratatui_image::protocol::sixel::Sixel;

        fn sixel(cells: Size) -> Prepared {
            let protocol = StatefulProtocol::new(
                image::DynamicImage::new_rgba8(1, 1),
                FontSize::new(10, 20),
                None,
                StatefulProtocolType::Sixel(Sixel::default()),
            );
            Prepared::sixel(protocol, cells, 8)
        }

        let size = Size::new(40, 10);
        let other = Size::new(80, 20);
        let entry = |kind: Prepared| Entry {
            media: "media-1".to_owned(),
            class: THUMBNAIL,
            image: Arc::new(kind),
        };
        let kitty = entry(Prepared::kitty(1, vec![0; 8]));
        let sixel = entry(sixel(size));

        assert!(kitty.matches(Lookup::Media("media-1")));
        assert!(!kitty.matches(Lookup::Cells("media-1", size)));
        assert!(sixel.matches(Lookup::Cells("media-1", size)));
        assert!(
            !sixel.matches(Lookup::Cells("media-1", other)),
            "another grid needs its own encode"
        );
        assert!(
            !sixel.matches(Lookup::Media("media-1")),
            "a sixel must never answer a size-independent lookup"
        );
        assert!(!sixel.matches(Lookup::Cells("media-2", size)));
    }
}
