//! The sixel graphics protocol: decode, scale and encode, for one cell grid at
//! a time.
//!
//! Sixel is immediate mode. The escape carries the pixels themselves, encoded
//! as bands of six vertical pixels, so there is nothing to transmit once and
//! let the terminal rescale: every cell grid needs its own decode, resize and
//! encode, and the finished escape is written into a buffer cell at paint time
//! by [`super::Prepared`]. That is also why a sixel entry's cache key includes
//! the cell grid while kitty's does not.
//!
//! The encode is CPU-bound and grows with the area, so it must never run on the
//! UI thread: the frontend asks for the media, the executor runs [`prepare`]
//! under `spawn_blocking`, and the frontend adopts the result with
//! [`super::Graphics::adopt`]. Nothing here talks to the terminal; the picker
//! carries the answer [`super::probe`] already collected.
//!
//! The escape that ends up in a cell is not only the DCS: `ratatui-image` writes
//! its erase-cells prefix (one `ECH`/`CUD` pair per row and a `CUU` back) ahead
//! of it, because the cells the escape covers are marked skipped and a stale
//! glyph behind them would otherwise show through. That prefix is left as the
//! library renders it.

use anyhow::Context as _;
use ratatui::layout::Size;
use ratatui_image::Resize;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocolType;

use super::Prepared;

/// Decode `bytes`, fit the image into a grid of `size` cells, and encode that
/// as sixel.
///
/// Blocking: this decodes the whole image and runs the sixel encoder over the
/// fitted pixels, so call it from `spawn_blocking` and hand the result back to
/// the UI thread. An undecodable payload or a failed encode is an error, which
/// the caller shows as its no-preview state rather than as an empty image.
pub fn prepare(picker: &Picker, bytes: &[u8], size: Size) -> anyhow::Result<Prepared> {
    use ratatui_image::ResizeEncodeRender as _;

    let image = image::load_from_memory(bytes).context("decoding an image for a sixel terminal")?;
    let mut protocol = picker.new_resize_protocol(image);
    protocol.resize_encode(&Resize::Fit(None), size);
    match protocol.last_encoding_result() {
        Some(Ok(())) => {}
        Some(Err(error)) => {
            return Err(anyhow::Error::new(error).context("encoding a sixel image"));
        }
        // A zero-width or zero-height grid is not encoded at all, so there would
        // be no escape to draw.
        None => anyhow::bail!(
            "nothing was encoded for a {}x{} cell image",
            size.width,
            size.height
        ),
    }
    let StatefulProtocolType::Sixel(sixel) = protocol.protocol_type() else {
        anyhow::bail!("the graphics probe did not select sixel for this terminal");
    };
    let encoded = sixel.data.len();
    Ok(Prepared::sixel(protocol, size, encoded))
}
