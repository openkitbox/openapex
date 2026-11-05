//! The kitty graphics protocol: transmit the image once, then show it with
//! unicode placeholder cells.
//!
//! The placeholder scheme is ported from `ratatui-image` 11.1.0
//! `src/protocol/kitty.rs` (MIT licensed, Copyright (c) 2023 Benjamin Große),
//! which in turn takes the diacritic table from kitty's
//! `rowcolumn-diacritics.txt`. Deliberate deviations from it:
//!
//! - the transmission is not embedded in the first cell's symbol. It goes
//!   through [`super::Queue`] instead, because a multi-kilobyte escape inside a
//!   cell is measured as symbol width by ratatui and has to be re-sent whenever
//!   that cell is rewritten.
//! - the payload is the original PNG (`f=100`, PNG passthrough) rather than raw
//!   RGBA, so nothing is decoded or re-encoded here and the terminal keeps the
//!   compressed bytes.
//! - the control keys appear only in the first escape of a chunked
//!   transmission; every continuation chunk carries its `m` flag and the payload
//!   and nothing else, which is what the protocol defines and what the
//!   reference gets for free by writing the keys only when `i == 0`. `q=2`
//!   therefore stops at the first chunk as well; replies are suppressed for the
//!   whole transmission, not per chunk.
//! - tmux and screen get every escape inside their DCS passthrough wrapper with
//!   each inner `ESC` doubled, for the transmission and for the delete, because
//!   a multiplexer forwards neither otherwise.
//!
//! Only the pure parts matter to a test: chunk boundaries, the escape framing,
//! the diacritic index arithmetic and the id counter. Nothing here touches the
//! terminal.

use std::fmt::Write as _;
use std::num::NonZeroU16;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ratatui::buffer::Buffer;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::style::Color;

/// The character kitty reserves for image placeholders.
const PLACEHOLDER: char = '\u{10EEEE}';

/// Base64 characters per escape sequence: the protocol's limit per chunk.
const CHARS_PER_CHUNK: usize = 4096;

/// Bytes per chunk: base64 of 3072 bytes is exactly 4096 characters.
const CHUNK_BYTES: usize = (CHARS_PER_CHUNK / 4) * 3;

/// Allocates image ids.
///
/// Ids are monotonic and never reused, so deleting an evicted image can never
/// remove a newer image that happened to take the same id.
#[derive(Debug)]
pub struct Ids {
    /// The next id, or `None` once the space is exhausted.
    next: Option<u32>,
}

impl Ids {
    /// Id 0 means "unspecified" in the protocol, so allocation starts at 1.
    #[must_use]
    pub const fn new() -> Self {
        Self { next: Some(1) }
    }

    /// The next unused id, or `None` when every id has been handed out.
    ///
    /// A session would have to transmit four billion images to get here, but
    /// wrapping or saturating would hand out a live id again and let a new
    /// image silently overwrite the terminal's copy of an old one, so the
    /// exhausted case refuses to allocate instead.
    pub fn take(&mut self) -> Option<u32> {
        let id = self.next?;
        self.next = id.checked_add(1);
        Some(id)
    }
}

impl Default for Ids {
    fn default() -> Self {
        Self::new()
    }
}

/// How a kitty escape reaches the terminal.
///
/// A terminal multiplexer passes unknown escape sequences through to nobody:
/// tmux and screen only forward them inside their own DCS passthrough wrapper,
/// with every `ESC` of the inner sequence doubled. The probe decides which arm
/// applies once, at startup, so nothing on the draw path looks at the
/// environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wrapper {
    /// The escape goes to the terminal as it is.
    Direct,
    /// The escape travels through tmux's or screen's passthrough.
    Tmux,
}

/// The three fragments a wrapped escape is assembled from.
struct Framing {
    /// What opens the wrapper.
    start: &'static str,
    /// The escape introducer, doubled inside the wrapper.
    escape: &'static str,
    /// What closes the wrapper.
    end: &'static str,
}

impl Wrapper {
    /// From `ratatui-image` 11.1.0 `picker/cap_parser.rs::tmux_start_escape_end`:
    /// `\x1bPtmux;` … `\x1b\\`, with every inner `\x1b` doubled.
    const fn framing(self) -> Framing {
        match self {
            Self::Direct => Framing {
                start: "",
                escape: "\x1b",
                end: "",
            },
            Self::Tmux => Framing {
                start: "\x1bPtmux;",
                escape: "\x1b\x1b",
                end: "\x1b\\",
            },
        }
    }
}

/// Hand `png` to the terminal as image `id`.
///
/// `f=100` is PNG passthrough: the bytes go out exactly as they arrived. `U=1`
/// creates a virtual placement, which is what the placeholder cells reference
/// and what lets a single transmission serve every size — the terminal scales
/// it to the grid of cells the placeholders occupy. `q=2` silences the
/// terminal's replies so they never reach the TUI's input stream.
///
/// The control keys are sent once, in the first escape. Kitty reads a
/// continuation chunk as more payload for the transmission just opened, so it
/// carries nothing but its `m` flag: repeating `i` there is not the protocol,
/// and a terminal that treats an unexpected key as a new transmission drops the
/// image. `m=1`/`m=0` stays on every chunk because that is how the terminal
/// learns the transmission is complete.
/// The bytes [`transmit`] would produce for `png`: the base64 expansion, plus
/// the header, escape and wrapper bytes each chunk adds.
///
/// The cache asks before it allocates an image id, so an image it would refuse
/// is never numbered or transmitted at all.
#[must_use]
pub fn payload_bytes(png: &[u8]) -> usize {
    let chunks = png.len().div_ceil(CHUNK_BYTES).max(1);
    png.len().div_ceil(3) * 4 + chunks * 64
}

#[must_use]
pub fn transmit(id: u32, png: &[u8], wrapper: Wrapper) -> Vec<u8> {
    let framing = wrapper.framing();
    let chunks = png.len().div_ceil(CHUNK_BYTES);
    // Base64 expands by 4/3; each chunk adds its header, the escape tail and a
    // wrapper's own bytes.
    let mut out = String::with_capacity(payload_bytes(png));
    for (index, chunk) in png.chunks(CHUNK_BYTES).enumerate() {
        let more = u8::from(index + 1 < chunks);
        out.push_str(framing.start);
        out.push_str(framing.escape);
        if index == 0 {
            write!(out, "_Gf=100,t=d,a=t,U=1,i={id},q=2,m={more};")
        } else {
            write!(out, "_Gm={more};")
        }
        .expect("writing into a String cannot fail");
        STANDARD.encode_string(chunk, &mut out);
        out.push_str(framing.escape);
        out.push('\\');
        out.push_str(framing.end);
    }
    out.into_bytes()
}

/// Draw image `id` over `area` with placeholder cells.
///
/// Every cell in the area takes the placeholder symbol; the first column also
/// carries three combining diacritics (row, column, the id's high byte), and
/// every cell's foreground colour carries the id's low three bytes. That is how
/// the terminal finds the image behind the cells. Indices past the diacritic
/// table cannot be encoded, so an area larger than the table is drawn truncated
/// rather than at the wrong offset.
pub fn place(area: Rect, buf: &mut Buffer, id: u32) {
    let [id_extra, red, green, blue] = id.to_be_bytes();
    let foreground = Color::Rgb(red, green, blue);
    let forced = forced_width();
    let limit = DIACRITICS.len() as u16;
    let width = area.width.min(limit);
    let height = area.height.min(limit);
    let mut symbol = String::with_capacity(8);
    for y in 0..height {
        for x in 0..width {
            let Some(cell) = buf.cell_mut(Position::new(area.left() + x, area.top() + y)) else {
                continue;
            };
            symbol.clear();
            symbol.push(PLACEHOLDER);
            if x == 0 {
                symbol.push(diacritic(y));
                symbol.push(diacritic(x));
                symbol.push(diacritic(u16::from(id_extra)));
            }
            cell.set_symbol(&symbol)
                .set_fg(foreground)
                .set_diff_option(forced);
        }
    }
}

/// The escapes that make the terminal forget image `id`.
///
/// A delete carries the same wrapper as the transmission: one that tmux does not
/// pass through would leave the image resident for the life of the terminal
/// session, which is the leak the delete exists to prevent.
#[must_use]
pub fn delete(id: u32, wrapper: Wrapper) -> Vec<u8> {
    let Framing { start, escape, end } = wrapper.framing();
    format!("{start}{escape}_Ga=d,d=i,i={id},q=2;{escape}\\{end}").into_bytes()
}

/// A placeholder cell is one column wide, whatever the appended diacritics
/// measure; without this ratatui would shift the rest of the row.
fn forced_width() -> CellDiffOption {
    let width = NonZeroU16::new(1).expect("1 is not zero");
    CellDiffOption::ForcedWidth(width)
}

/// The combining mark kitty reads as index `index`.
///
/// Out-of-range indices fall back to the first entry, as the reference does;
/// [`place`] clamps the area so it never asks for one.
fn diacritic(index: u16) -> char {
    *DIACRITICS.get(usize::from(index)).unwrap_or(&DIACRITICS[0])
}

/// From <https://sw.kovidgoyal.net/kitty/_downloads/1792bad15b12979994cd6ecc54c967a6/rowcolumn-diacritics.txt>
static DIACRITICS: [char; 297] = [
    '\u{305}',
    '\u{30D}',
    '\u{30E}',
    '\u{310}',
    '\u{312}',
    '\u{33D}',
    '\u{33E}',
    '\u{33F}',
    '\u{346}',
    '\u{34A}',
    '\u{34B}',
    '\u{34C}',
    '\u{350}',
    '\u{351}',
    '\u{352}',
    '\u{357}',
    '\u{35B}',
    '\u{363}',
    '\u{364}',
    '\u{365}',
    '\u{366}',
    '\u{367}',
    '\u{368}',
    '\u{369}',
    '\u{36A}',
    '\u{36B}',
    '\u{36C}',
    '\u{36D}',
    '\u{36E}',
    '\u{36F}',
    '\u{483}',
    '\u{484}',
    '\u{485}',
    '\u{486}',
    '\u{487}',
    '\u{592}',
    '\u{593}',
    '\u{594}',
    '\u{595}',
    '\u{597}',
    '\u{598}',
    '\u{599}',
    '\u{59C}',
    '\u{59D}',
    '\u{59E}',
    '\u{59F}',
    '\u{5A0}',
    '\u{5A1}',
    '\u{5A8}',
    '\u{5A9}',
    '\u{5AB}',
    '\u{5AC}',
    '\u{5AF}',
    '\u{5C4}',
    '\u{610}',
    '\u{611}',
    '\u{612}',
    '\u{613}',
    '\u{614}',
    '\u{615}',
    '\u{616}',
    '\u{617}',
    '\u{657}',
    '\u{658}',
    '\u{659}',
    '\u{65A}',
    '\u{65B}',
    '\u{65D}',
    '\u{65E}',
    '\u{6D6}',
    '\u{6D7}',
    '\u{6D8}',
    '\u{6D9}',
    '\u{6DA}',
    '\u{6DB}',
    '\u{6DC}',
    '\u{6DF}',
    '\u{6E0}',
    '\u{6E1}',
    '\u{6E2}',
    '\u{6E4}',
    '\u{6E7}',
    '\u{6E8}',
    '\u{6EB}',
    '\u{6EC}',
    '\u{730}',
    '\u{732}',
    '\u{733}',
    '\u{735}',
    '\u{736}',
    '\u{73A}',
    '\u{73D}',
    '\u{73F}',
    '\u{740}',
    '\u{741}',
    '\u{743}',
    '\u{745}',
    '\u{747}',
    '\u{749}',
    '\u{74A}',
    '\u{7EB}',
    '\u{7EC}',
    '\u{7ED}',
    '\u{7EE}',
    '\u{7EF}',
    '\u{7F0}',
    '\u{7F1}',
    '\u{7F3}',
    '\u{816}',
    '\u{817}',
    '\u{818}',
    '\u{819}',
    '\u{81B}',
    '\u{81C}',
    '\u{81D}',
    '\u{81E}',
    '\u{81F}',
    '\u{820}',
    '\u{821}',
    '\u{822}',
    '\u{823}',
    '\u{825}',
    '\u{826}',
    '\u{827}',
    '\u{829}',
    '\u{82A}',
    '\u{82B}',
    '\u{82C}',
    '\u{82D}',
    '\u{951}',
    '\u{953}',
    '\u{954}',
    '\u{F82}',
    '\u{F83}',
    '\u{F86}',
    '\u{F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];
