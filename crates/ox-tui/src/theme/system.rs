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

#[cfg(test)]
mod tests {
    use super::System;
    use crate::theme::terminal::{Rgb, TerminalColors};

    fn colors(background: Option<Rgb>) -> TerminalColors {
        TerminalColors {
            background,
            ..TerminalColors::default()
        }
    }

    fn derive(background: Rgb) -> System {
        System::derive(&colors(Some(background))).expect("background is present")
    }

    #[test]
    fn polarity_follows_background_luminance() {
        assert!(!derive(Rgb::new(0x1e, 0x1e, 0x2e)).is_light);
        assert!(derive(Rgb::new(0xfd, 0xf6, 0xe3)).is_light);
        // 0x808080 is luma 128, just above the reference threshold (> 127.5).
        assert!(derive(Rgb::new(0x80, 0x80, 0x80)).is_light);
    }

    #[test]
    fn near_black_background_uses_the_fixed_ramp() {
        let system = derive(Rgb::new(0, 0, 0));
        assert_eq!(system.grays[1], Rgb::new(8, 8, 8));
        assert_eq!(system.grays[2], Rgb::new(17, 17, 17));
        assert_eq!(system.grays[12], Rgb::new(102, 102, 102));
        assert_eq!(system.muted, Rgb::new(180, 180, 180));
    }

    #[test]
    fn dark_background_lifts_the_ramp_proportionally() {
        let system = derive(Rgb::new(16, 16, 16));
        // luminance 16: 16 + (255 - 16) * (2/12) * 0.4 = 31.93
        assert_eq!(system.grays[2], Rgb::new(31, 31, 31));
        // 16 + (255 - 16) * 0.4 = 111.6
        assert_eq!(system.grays[12], Rgb::new(111, 111, 111));
        assert_eq!(system.muted, Rgb::new(164, 164, 164));
        for pair in system.grays[1..].windows(2) {
            assert!(
                pair[0].r < pair[1].r,
                "ramp must ascend on a dark background"
            );
        }
    }

    #[test]
    fn ramp_keeps_the_background_hue() {
        let system = derive(Rgb::new(16, 24, 40));
        assert_eq!(system.grays[2], Rgb::new(26, 39, 66));
        assert!(system.grays[2].b > system.grays[2].g);
        assert!(system.grays[2].g > system.grays[2].r);
    }

    #[test]
    fn near_white_background_uses_the_fixed_ramp() {
        let system = derive(Rgb::new(0xfd, 0xf6, 0xe3));
        assert_eq!(system.grays[1], Rgb::new(246, 246, 246));
        assert_eq!(system.grays[12], Rgb::new(153, 153, 153));
        assert_eq!(system.muted, Rgb::new(75, 75, 75));
    }

    #[test]
    fn light_background_darkens_the_ramp_proportionally() {
        let system = derive(Rgb::new(240, 240, 240));
        // 240 * (1 - (2/12) * 0.4) = 224
        assert_eq!(system.grays[2], Rgb::new(224, 224, 224));
        assert_eq!(system.grays[12], Rgb::new(144, 144, 144));
        assert_eq!(system.muted, Rgb::new(97, 97, 97));
        for pair in system.grays[1..].windows(2) {
            assert!(
                pair[0].r > pair[1].r,
                "ramp must descend on a light background"
            );
        }
    }

    #[test]
    fn background_falls_back_to_palette_slot_zero() {
        let mut colors = TerminalColors::default();
        colors.palette[0] = Some(Rgb::new(0x11, 0x22, 0x33));
        let system = System::derive(&colors).expect("palette slot zero is enough");
        assert_eq!(system.background, Rgb::new(0x11, 0x22, 0x33));
    }

    #[test]
    fn no_background_means_no_system_theme() {
        assert_eq!(System::derive(&TerminalColors::default()), None);
    }
}
