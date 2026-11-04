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

#[cfg(test)]
mod tests {
    use super::*;

    use image::ImageEncoder as _;

    fn one_pixel() -> image::RgbaImage {
        image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]))
    }

    #[test]
    fn a_png_payload_is_passed_on_without_being_re_encoded() {
        let mut png = Vec::new();
        one_pixel()
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("a 1x1 PNG encodes");
        let png: Arc<[u8]> = Arc::from(png);
        let same = ensure(Arc::clone(&png)).expect("PNG bytes are already PNG");
        assert!(
            Arc::ptr_eq(&same, &png),
            "the passthrough must not copy a payload the terminal can take as it is"
        );
    }

    #[test]
    fn a_stored_jpeg_is_re_encoded_before_the_terminal_sees_it() {
        let rgb = image::DynamicImage::ImageRgba8(one_pixel()).to_rgb8();
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .write_image(rgb.as_raw(), 1, 1, image::ExtendedColorType::Rgb8)
            .expect("a 1x1 JPEG encodes");
        let png = ensure(Arc::from(jpeg)).expect("a JPEG is an image this build decodes");
        assert!(
            png.starts_with(&SIGNATURE),
            "what kitty is handed under `f=100` has to be PNG"
        );
    }

    #[test]
    fn bytes_that_are_not_an_image_are_refused() {
        assert!(
            ensure(Arc::from(&b"not an image at all"[..])).is_none(),
            "transmitting these would draw nothing, so the caller shows no preview"
        );
    }
}
