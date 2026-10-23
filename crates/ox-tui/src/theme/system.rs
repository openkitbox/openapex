//! The `system` theme: opencode's `generateSystem`
//! (`packages/tui/src/theme/system.ts:11-144`), ported.
//!
//! The palette is the terminal's own: the ANSI hues are used by name, the body
//! text stays the terminal default, and only the surfaces that have to be
//! distinguishable from the background (panel, menu, borders, muted text) are
//! derived from the reported default background as a 12-step gray ramp.
//!
//! Deliberate deviations from the reference:
//!
//! 1. The polarity is derived from the reported background alone. Upstream
//!    takes it from the renderer (or a pinned config mode); ox has neither yet,
//!    so `light` means "the terminal background is light".
//! 2. A light background whose luma rounds to 0 would divide by zero upstream
//!    (NaN grays); here it takes the same fixed ramp as the near-black case.
//! 3. Only the grays and the muted text are derived. Upstream also splits a
//!    markdown/syntax palette and tints diff backgrounds from these same hues;
//!    ox has no markdown or diff surface yet, so those stay out until they do.

use super::terminal::{Rgb, TerminalColors};

/// The ramp is 1-based upstream (`grays[1..=12]`); slot 0 is unused and kept so
/// the indices line up with the reference.
const GRAYS: usize = 13;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct System {
    pub background: Rgb,
    pub muted: Rgb,
    pub grays: [Rgb; GRAYS],
    pub is_light: bool,
}

impl System {
    /// `None` when the terminal reported no background at all: there is nothing
    /// to derive the ramp from, so the caller keeps its static palette.
    pub fn derive(colors: &TerminalColors) -> Option<Self> {
        let background = colors.background.or(colors.palette[0])?;
        let is_light = background.luminance() > 127.5;
        Some(Self {
            background,
            muted: muted_text(background, !is_light),
            grays: gray_scale(background, !is_light),
            is_light,
        })
    }
}

/// 12 steps lifted (dark) or lowered (light) from the background, scaled by
/// 0.4 of the way to the opposite end of the range (`system.ts:101-133`).
fn gray_scale(background: Rgb, is_dark: bool) -> [Rgb; GRAYS] {
    let luminance = background.luminance();
    let fixed = |factor: f64| -> u8 {
        let value = if is_dark {
            factor * 0.4 * 255.0
        } else {
            255.0 - factor * 0.4 * 255.0
        };
        value.floor() as u8
    };

    let mut grays = [background; GRAYS];
    for (step, slot) in grays.iter_mut().enumerate().skip(1) {
        let factor = step as f64 / 12.0;
        // Near-black and near-white backgrounds have no room to scale into, so
        // the reference switches to a fixed ramp. `luminance <= 0` is the same
        // corner (upstream would divide by zero).
        if luminance <= 0.0 || (is_dark && luminance < 10.0) || (!is_dark && luminance > 245.0) {
            let gray = fixed(factor);
            *slot = Rgb::new(gray, gray, gray);
            continue;
        }
        let next = if is_dark {
            luminance + (255.0 - luminance) * factor * 0.4
        } else {
            luminance * (1.0 - factor * 0.4)
        };
        let ratio = next / luminance;
        *slot = Rgb::new(
            channel(f64::from(background.r) * ratio),
            channel(f64::from(background.g) * ratio),
            channel(f64::from(background.b) * ratio),
        );
    }
    grays
}

/// Muted text: a gray that reads on the background without competing with body
/// text (`system.ts:135-144`).
fn muted_text(background: Rgb, is_dark: bool) -> Rgb {
    let luminance = background.luminance();
    let gray = if is_dark {
        if luminance < 10.0 {
            180.0
        } else {
            (160.0 + luminance * 0.3).min(200.0).floor()
        }
    } else if luminance > 245.0 {
        75.0
    } else {
        (100.0 - (255.0 - luminance) * 0.2).max(60.0).floor()
    };
    let gray = gray.clamp(0.0, 255.0) as u8;
    Rgb::new(gray, gray, gray)
}

fn channel(value: f64) -> u8 {
    value.clamp(0.0, 255.0).floor() as u8
}
