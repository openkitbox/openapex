//! The prompt footer's running indicator: upstream's "knight rider" scanner
//! (`ui/spinner.ts` `createFrames` / `createColors`, `style: "blocks"`,
//! `inactiveFactor: 0.6`, `minAlpha: 0.3`, 40ms per frame).
//!
//! Everything here is pure: a frame index in, eight cells out. The clock lives
//! in `Prompt`, which owns the frame counter.
//!
//! Deviation: opentui blends translucent foregrounds over the cell
//! background itself. A terminal cannot, so [`cells`] blends against the
//! palette's backdrop; with no known backdrop (16-colour or `NO_COLOR` decks)
//! the caller falls back to the plain agent / dim styles.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use crate::theme::Palette;
use crate::theme::terminal::Rgb;

/// Characters in the scanner.
const WIDTH: usize = 8;
/// Frames held at the far end / at the start, and the trail length.
const HOLD_END: usize = 9;
const HOLD_START: usize = 30;
const TRAIL: usize = 6;
const MIN_ALPHA: f64 = 0.3;
const INACTIVE_ALPHA: f64 = 0.6;

/// Forward + hold + backward + hold: one full cycle.
pub const FRAMES: usize = WIDTH + HOLD_END + (WIDTH - 1) + HOLD_START;
/// Upstream's `interval={40}`.
pub const INTERVAL_MS: u128 = 40;

/// Where the scanner head is and how far the current phase has run.
struct State {
    position: usize,
    holding: bool,
    progress: usize,
    total: usize,
    forward: bool,
}

fn state(frame: usize) -> State {
    let backward = WIDTH - 1;
    if frame < WIDTH {
        State {
            position: frame,
            holding: false,
            progress: frame,
            total: WIDTH,
            forward: true,
        }
    } else if frame < WIDTH + HOLD_END {
        State {
            position: WIDTH - 1,
            holding: true,
            progress: frame - WIDTH,
            total: HOLD_END,
            forward: true,
        }
    } else if frame < WIDTH + HOLD_END + backward {
        let index = frame - WIDTH - HOLD_END;
        State {
            position: WIDTH - 2 - index,
            holding: false,
            progress: index,
            total: backward,
            forward: false,
        }
    } else {
        State {
            position: 0,
            holding: true,
            progress: frame - WIDTH - HOLD_END - backward,
            total: HOLD_START,
            forward: false,
        }
    }
}

/// `calculateColorIndex`: the trail step a character sits on, if any.
fn trail_index(state: &State, ch: usize) -> Option<usize> {
    let distance = if state.forward {
        state.position as isize - ch as isize
    } else {
        ch as isize - state.position as isize
    };
    let index = if state.holding {
        distance + state.progress as isize
    } else if distance > 0 && (distance as usize) < TRAIL {
        distance
    } else if distance == 0 {
        0
    } else {
        return None;
    };
    (0..TRAIL as isize)
        .contains(&index)
        .then_some(index as usize)
}

/// `deriveTrailColors`: (alpha, brightness) per trail step.
fn trail_weight(step: usize) -> (f64, f64) {
    match step {
        0 => (1.0, 1.0),
        1 => (0.9, 1.15),
        n => (0.65f64.powi(n as i32 - 1), 1.0),
    }
}

/// One character of one frame: the glyph and the (alpha, brightness) it is
/// drawn with over the backdrop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    pub glyph: char,
    pub alpha: f64,
    pub brightness: f64,
}

/// The eight cells of `frame` (wrapped into one cycle).
pub fn cells(frame: usize) -> [Cell; WIDTH] {
    let state = state(frame % FRAMES);
    // The fade applied to inactive dots (`createKnightRiderTrail`).
    let fade = if state.holding && state.total > 0 {
        let progress = (state.progress as f64 / state.total as f64).min(1.0);
        (1.0 - progress * (1.0 - MIN_ALPHA)).max(MIN_ALPHA)
    } else if !state.holding && state.total > 0 {
        let progress =
            (state.progress as f64 / (state.total.saturating_sub(1)).max(1) as f64).min(1.0);
        MIN_ALPHA + progress * (1.0 - MIN_ALPHA)
    } else {
        1.0
    };
    std::array::from_fn(|ch| match trail_index(&state, ch) {
        Some(step) => {
            let (alpha, brightness) = trail_weight(step);
            Cell {
                glyph: '■',
                alpha,
                brightness,
            }
        }
        None => Cell {
            glyph: '⬝',
            alpha: INACTIVE_ALPHA * fade,
            brightness: 1.0,
        },
    })
}

fn scale(rgb: Rgb, brightness: f64) -> Rgb {
    let up = |value: u8| (f64::from(value) * brightness).round().min(255.0) as u8;
    Rgb::new(up(rgb.r), up(rgb.g), up(rgb.b))
}

/// The footer spinner as spans. Truecolor blends the agent colour over the
/// backdrop like opentui; otherwise active cells take the agent style and the
/// rest the dim style.
pub fn spans(frame: usize, pal: &Palette) -> Vec<Span<'static>> {
    let blend = match (pal.agent.fg, pal.backdrop) {
        (Some(Color::Rgb(r, g, b)), Some(backdrop)) => Some((Rgb::new(r, g, b), backdrop)),
        _ => None,
    };
    cells(frame)
        .into_iter()
        .map(|cell| {
            let style = match blend {
                Some((agent, backdrop)) => {
                    let color = Rgb::tint(backdrop, scale(agent, cell.brightness), cell.alpha);
                    Style::default().fg(Color::Rgb(color.r, color.g, color.b))
                }
                None if cell.glyph == '■' => Style::default()
                    .fg(pal.agent.fg.unwrap_or(Color::Reset))
                    .add_modifier(pal.agent.add_modifier),
                None => pal.dim.remove_modifier(Modifier::BOLD),
            };
            Span::styled(cell.glyph.to_string(), style)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{FRAMES, INTERVAL_MS, cells, spans};
    use crate::theme::{Deck, Palette};

    fn glyphs(frame: usize) -> String {
        cells(frame).iter().map(|cell| cell.glyph).collect()
    }

    #[test]
    fn a_cycle_is_54_frames_of_40ms() {
        assert_eq!(FRAMES, 54);
        assert_eq!(INTERVAL_MS, 40);
    }

    #[test]
    fn the_head_sweeps_forward_with_a_trail_behind_it() {
        assert_eq!(glyphs(0), "■⬝⬝⬝⬝⬝⬝⬝");
        assert_eq!(glyphs(3), "■■■■⬝⬝⬝⬝");
        // Trail length 6: position 7 lights positions 2..=7.
        assert_eq!(glyphs(7), "⬝⬝■■■■■■");
    }

    #[test]
    fn the_head_holds_at_the_far_end_and_the_trail_runs_out() {
        // Frames 8..17 hold at position 7; the index shifts by hold progress.
        assert_eq!(glyphs(8), "⬝⬝■■■■■■");
        assert_eq!(glyphs(9), "⬝⬝⬝■■■■■");
        assert_eq!(glyphs(12), "⬝⬝⬝⬝⬝⬝■■");
        assert_eq!(glyphs(13), "⬝⬝⬝⬝⬝⬝⬝■");
        assert_eq!(glyphs(14), "⬝⬝⬝⬝⬝⬝⬝⬝");
    }

    #[test]
    fn the_head_comes_back_trailing_to_the_right() {
        // Backward starts at position 6 (frame 17).
        assert_eq!(glyphs(17), "⬝⬝⬝⬝⬝⬝■■");
        assert_eq!(glyphs(23), "■■■■■■⬝⬝");
    }

    #[test]
    fn the_cycle_wraps() {
        assert_eq!(glyphs(FRAMES), glyphs(0));
        assert_eq!(glyphs(FRAMES + 5), glyphs(5));
    }

    #[test]
    fn the_lead_is_brightest_and_inactive_dots_fade_during_the_hold() {
        let first = cells(0);
        assert_eq!((first[0].alpha, first[0].brightness), (1.0, 1.0));
        // Frame 30 sits in the long hold at the start: dots have faded to the
        // floor reached at the end of the hold, not the 0.6 ceiling.
        let held = cells(FRAMES - 1);
        let dot = held[7];
        assert_eq!(dot.glyph, '⬝');
        assert!(dot.alpha < 0.6 * 0.4, "{}", dot.alpha);
        assert!(dot.alpha >= 0.6 * 0.3 - 1e-9, "{}", dot.alpha);
        // Bloom step: slightly brighter, slightly dimmer than the lead.
        let bloom = cells(1)[0];
        assert_eq!(bloom.alpha, 0.9);
        assert_eq!(bloom.brightness, 1.15);
    }

    #[test]
    fn truecolor_blends_over_the_backdrop_and_16_colour_falls_back_to_styles() {
        let truecolor = Palette::for_deck(Deck::Truecolor);
        assert_eq!(spans(0, &truecolor).len(), 8);
        let plain = Palette::for_deck(Deck::NoColor);
        let line = spans(0, &plain);
        assert_eq!(line.len(), 8);
        assert_eq!(line[0].content, "■");
        assert_eq!(line[1].content, "⬝");
    }
}
