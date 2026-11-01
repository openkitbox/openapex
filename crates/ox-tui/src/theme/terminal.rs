//! Terminal colour discovery: OSC 10 (default foreground), OSC 11 (default
//! background) and OSC 4 (palette entries 0-15).
//!
//! Upstream opencode reads the same three probes inside `@opentui/core` and
//! builds its `system` theme from them (`packages/tui/src/theme/system.ts`).
//! ox has no terminal core to do that for it, so the queries are sent before
//! the TUI takes the terminal over: raw mode is enabled just long enough to
//! read the replies back, and the whole exchange is bounded by [`TIMEOUT`] so a
//! terminal that never answers costs a fifth of a second, not a hang.

use std::time::Duration;

/// Total budget for the whole exchange.
pub const TIMEOUT: Duration = Duration::from_millis(200);

/// Stop reading once the terminal has been quiet this long: a real terminal
/// answers in one burst, so waiting out the full budget only happens when
/// nothing is coming.
const QUIET: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// `base` moved `alpha` of the way to `overlay`, rounded per channel
    /// (upstream `tint`, `theme/color.ts:43`).
    #[must_use]
    pub fn tint(base: Self, overlay: Self, alpha: f64) -> Self {
        let mix = |from: u8, to: u8| {
            (f64::from(from) + (f64::from(to) - f64::from(from)) * alpha).round() as u8
        };
        Self::new(
            mix(base.r, overlay.r),
            mix(base.g, overlay.g),
            mix(base.b, overlay.b),
        )
    }

    /// Rec.601 luma on the 0-255 scale (upstream `system.ts:106`).
    #[must_use]
    pub fn luminance(self) -> f64 {
        0.299 * f64::from(self.r) + 0.587 * f64::from(self.g) + 0.114 * f64::from(self.b)
    }
}

/// xterm's defaults for palette slots 0-15, used when the terminal does not
/// report a slot (upstream `ansiToRgba`, `theme/color.ts:3-22`).
pub const ANSI: [Rgb; 16] = [
    Rgb::new(0x00, 0x00, 0x00),
    Rgb::new(0x80, 0x00, 0x00),
    Rgb::new(0x00, 0x80, 0x00),
    Rgb::new(0x80, 0x80, 0x00),
    Rgb::new(0x00, 0x00, 0x80),
    Rgb::new(0x80, 0x00, 0x80),
    Rgb::new(0x00, 0x80, 0x80),
    Rgb::new(0xc0, 0xc0, 0xc0),
    Rgb::new(0x80, 0x80, 0x80),
    Rgb::new(0xff, 0x00, 0x00),
    Rgb::new(0x00, 0xff, 0x00),
    Rgb::new(0xff, 0xff, 0x00),
    Rgb::new(0x00, 0x00, 0xff),
    Rgb::new(0xff, 0x00, 0xff),
    Rgb::new(0x00, 0xff, 0xff),
    Rgb::new(0xff, 0xff, 0xff),
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalColors {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub palette: [Option<Rgb>; 16],
}

impl TerminalColors {
    /// A palette slot, falling back to the xterm default like upstream's
    /// `col(i)` helper (`system.ts:17-21`).
    #[must_use]
    pub fn slot(&self, index: usize) -> Rgb {
        self.palette
            .get(index)
            .copied()
            .flatten()
            .unwrap_or(ANSI[index.min(15)])
    }
}

/// Ask the terminal for its colours. Returns `None` when stdin or stdout is not
/// a terminal (pipes, tests, `cargo run | cat`).
#[must_use]
pub fn query() -> Option<TerminalColors> {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return None;
    }

    // Raw mode so the reply is neither line-buffered nor echoed; restored
    // before the TUI takes over (`TerminalGuard` enables it again).
    crossterm::terminal::enable_raw_mode().ok()?;
    let mut replies = Vec::new();
    let written = {
        let mut stdout = std::io::stdout();
        QUERIES
            .iter()
            .all(|query| stdout.write_all(query).and_then(|_| stdout.flush()).is_ok())
    };
    if written {
        read_replies(&mut replies);
    }
    let _ = crossterm::terminal::disable_raw_mode();
    Some(parse(&replies))
}

/// OSC 10 (default fg), OSC 11 (default bg) and OSC 4 for palette slots 0-15.
/// A terminal that ignores a query simply leaves those fields empty; the caller
/// then falls back to the xterm defaults or to the static palette.
const QUERIES: [&[u8]; 3] = [
    b"\x1b]10;?\x07",
    b"\x1b]11;?\x07",
    b"\x1b]4;0;?;1;?;2;?;3;?;4;?;5;?;6;?;7;?;8;?;9;?;10;?;11;?;12;?;13;?;14;?;15;?\x07",
];

/// Poll-and-read until the terminal goes quiet or [`TIMEOUT`] runs out.
fn read_replies(out: &mut Vec<u8>) {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    let fd = std::io::stdin().as_raw_fd();
    let start = Instant::now();
    let mut last_data = start;
    let mut chunk = [0u8; 512];

    loop {
        let now = Instant::now();
        let elapsed = now.duration_since(start);
        if elapsed >= TIMEOUT {
            return;
        }
        if !out.is_empty() && now.duration_since(last_data) >= QUIET {
            return;
        }

        let wait = QUIET.min(TIMEOUT - elapsed);
        let mut polled = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd, no file descriptors are closed around it.
        let ready = unsafe { libc::poll(&mut polled, 1, wait.as_millis() as libc::c_int) };
        if ready <= 0 {
            continue;
        }
        match std::io::stdin().read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                out.extend_from_slice(&chunk[..read]);
                last_data = Instant::now();
            }
        }
    }
}

/// Parse the replies to the three queries out of a raw byte stream.
pub fn parse(bytes: &[u8]) -> TerminalColors {
    let mut colors = TerminalColors::default();
    let mut index = 0;

    while let Some(start) = find(bytes, index, b"\x1b]") {
        let body_start = start + 2;
        let Some((body, next)) = osc_body(bytes, body_start) else {
            // Unterminated: the rest of the buffer cannot be a reply.
            break;
        };
        index = next;
        let fields: Vec<&[u8]> = body.split(|byte| *byte == b';').collect();
        match fields
            .first()
            .and_then(|code| std::str::from_utf8(code).ok())
        {
            Some("10") => colors.foreground = fields.get(1).copied().and_then(color),
            Some("11") => colors.background = fields.get(1).copied().and_then(color),
            Some("4") => {
                for pair in fields[1..].chunks(2) {
                    let (Some(slot), Some(value)) = (pair.first(), pair.get(1)) else {
                        continue;
                    };
                    let Some(slot) = std::str::from_utf8(slot)
                        .ok()
                        .and_then(|slot| slot.parse::<usize>().ok())
                    else {
                        continue;
                    };
                    if let (Some(color), Some(slot)) = (color(value), colors.palette.get_mut(slot))
                    {
                        *slot = Some(color);
                    }
                }
            }
            _ => {}
        }
    }

    colors
}

fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| position + from)
}

/// The body of an OSC sequence starting at `from`, terminated by BEL or ST, and
/// the index just past the terminator.
fn osc_body(bytes: &[u8], from: usize) -> Option<(&[u8], usize)> {
    let rest = bytes.get(from..)?;
    let bel = rest.iter().position(|byte| *byte == 0x07);
    let st = find(rest, 0, b"\x1b\\");
    match (bel, st) {
        (Some(bel), Some(st)) if bel < st => Some((&rest[..bel], from + bel + 1)),
        (_, Some(st)) => Some((&rest[..st], from + st + 2)),
        (Some(bel), None) => Some((&rest[..bel], from + bel + 1)),
        (None, None) => None,
    }
}

/// `rgb:R/G/B` (or `rgba:`), each component 1-4 hex digits scaled to 8 bits.
fn color(field: &[u8]) -> Option<Rgb> {
    let field = std::str::from_utf8(field).ok()?;
    let rest = field
        .strip_prefix("rgb:")
        .or_else(|| field.strip_prefix("rgba:"))?;
    let mut parts = rest.split('/');
    Some(Rgb::new(
        component(parts.next()?)?,
        component(parts.next()?)?,
        component(parts.next()?)?,
    ))
}

fn component(hex: &str) -> Option<u8> {
    if hex.is_empty() || hex.len() > 4 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    let max = (1u32 << (4 * hex.len() as u32)) - 1;
    Some(((value * 255 + max / 2) / max) as u8)
}
