//! The one place terminal graphics support is decided.
//!
//! [`init`] runs ratatui-image's single stdio query — protocol, cell pixel
//! size, capabilities, tmux detection — and everything else reads the stored
//! answer, so nothing on the draw path talks to the terminal. The mapping from
//! the query's [`ProtocolType`] to [`Protocol`] is a plain function and
//! [`Capabilities`] is plain data, so both can be exercised without a terminal.

use std::io::IsTerminal;
use std::sync::OnceLock;

use ratatui_image::FontSize;
use ratatui_image::picker::Picker;
use ratatui_image::picker::ProtocolType;

/// What ox can draw with.
///
/// iTerm2's inline images and half-blocks are deliberately absent: there is no
/// half-block fallback, and an unsupported terminal gets no placeholder rows at
/// all rather than a low-fidelity approximation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Kitty,
    Sixel,
    Unsupported,
}

impl From<ProtocolType> for Protocol {
    fn from(protocol: ProtocolType) -> Self {
        match protocol {
            ProtocolType::Kitty => Self::Kitty,
            ProtocolType::Sixel => Self::Sixel,
            ProtocolType::Iterm2 | ProtocolType::Halfblocks => Self::Unsupported,
        }
    }
}

/// The probe's answer: which protocol to draw with, how large a cell is, and
/// whether escapes travel through a terminal multiplexer.
#[derive(Clone, Debug)]
pub struct Capabilities {
    pub protocol: Protocol,
    /// The cell size the query reported, or the fallback the library derived
    /// from the window size; `None` only when nothing was probed at all.
    pub font: Option<FontSize>,
    /// The query result itself, for the encoders that need to resize against
    /// the cell size.
    pub picker: Option<Picker>,
    /// Whether the query saw tmux (or screen) around the terminal. Kitty escapes
    /// must then be wrapped in the multiplexer's passthrough, or it drops them
    /// silently; see [`super::kitty::Wrapper`].
    pub tmux: bool,
}

impl Capabilities {
    /// Whether this terminal can draw pixels at all. The surfaces that reserve
    /// rows and the graphics layer that paints them read this one predicate, so
    /// they cannot disagree about what a supported terminal is.
    #[must_use]
    pub const fn draws_images(&self) -> bool {
        matches!(self.protocol, Protocol::Kitty | Protocol::Sixel)
    }

    /// Nothing is drawable. Also the answer when [`init`] never ran, so a
    /// missing probe fails closed instead of guessing a protocol.
    #[must_use]
    pub fn unsupported() -> Self {
        Self {
            protocol: Protocol::Unsupported,
            font: None,
            picker: None,
            tmux: false,
        }
    }
}

impl From<Picker> for Capabilities {
    fn from(picker: Picker) -> Self {
        let font = picker.font_size();
        Self {
            protocol: Protocol::from(picker.protocol_type()),
            font: Some(font),
            tmux: picker.tmux_detected(),
            picker: Some(picker),
        }
    }
}

static CAPABILITIES: OnceLock<Capabilities> = OnceLock::new();

/// Run the one terminal query and keep its answer.
///
/// Call it once at startup, in the same pre-TUI window as the colour query and
/// before the terminal event thread reads stdin: the query writes escape
/// sequences and reads the replies from the same stream as the keystrokes.
/// `Picker` enters and restores raw mode itself, so the two queries simply run
/// one after the other.
pub fn init() -> &'static Capabilities {
    CAPABILITIES.get_or_init(|| {
        // With either end on a pipe the query would write into it and then wait
        // out its own timeout; a pipe cannot answer with a protocol anyway.
        let capabilities = if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            match Picker::from_query_stdio() {
                Ok(picker) => Capabilities::from(picker),
                Err(error) => {
                    tracing::debug!(error = %error, "terminal graphics probe failed");
                    Capabilities::unsupported()
                }
            }
        } else {
            Capabilities::unsupported()
        };
        let cell = capabilities.font.map(|font| (font.width, font.height));
        tracing::debug!(
            protocol = ?capabilities.protocol,
            cell = ?cell,
            "terminal graphics probe"
        );
        capabilities
    })
}

/// The probe's answer; unsupported when [`init`] never ran.
#[must_use]
pub fn capabilities() -> &'static Capabilities {
    CAPABILITIES.get_or_init(Capabilities::unsupported)
}
