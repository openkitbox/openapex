//! What a kitty terminal is handed: PNG, and nothing else.
//!
//! A kitty transmission here is `f=100`, which *is* PNG data, and the terminal
//! scales it to whatever grid of placeholder cells draws it — that is why the
//! passthrough costs ox no decode at all. A stored attachment, though, is served
//! as the bytes it was pasted as, so a JPEG, GIF or WebP has to be re-encoded
//! before it can be transmitted at all. Sixel needs no such step:
//! [`super::sixel::prepare`] decodes whatever format arrived.
//!
//! This is the only place ox encodes an image it did not fetch as PNG, and it
//! runs off the UI thread: the terminal's scaling is worth more than the decode
//! for the one format that needs it.

use std::io::Cursor;
use std::sync::Arc;

/// The eight bytes every PNG starts with.
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// `bytes` as PNG: the payload itself when it already is one, otherwise decoded
/// and re-encoded.
///
/// `None` when the bytes are not an image this build can decode, which the
/// caller shows as its no-preview state rather than transmitting a payload the
/// terminal would draw as nothing. Blocking: it decodes the whole image, so call
/// it from `spawn_blocking`.
#[must_use]
pub fn ensure(bytes: Arc<[u8]>) -> Option<Arc<[u8]>> {
    if bytes.starts_with(&SIGNATURE) {
        return Some(bytes);
    }
    let image = image::load_from_memory(&bytes).ok()?;
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(Arc::from(png))
}
