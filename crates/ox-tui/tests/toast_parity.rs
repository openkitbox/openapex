//! Toast vs the upstream opencode 2.2.3 gold grid
//! (`tests/parity/gold/gold-toast-30x100.json`).
//!
//! Captured by running `/cd ` with an empty argument in upstream's Home screen
//! (`component/prompt/index.tsx:272-275`: `toast.show({ message: "Directory is
//! required", variant: "error" })`) under the same `system` theme terminal as
//! the Home and session golds. The palette is process-global, so this file
//! holds exactly one `#[test]`. Only rows 0-4 are compared: the rest of that
//! screen is the Home view, which `home_parity` already pins.

use ox_tui::theme::terminal::{Rgb, TerminalColors};
use ox_tui::ui::toast::{Toast, Variant, render};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use std::time::Instant;

fn hex(value: u32) -> Rgb {
    Rgb::new((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

fn gold_terminal() -> TerminalColors {
    let ansi = [
        0x1d1f21, 0xcc6666, 0xb5bd68, 0xf0c674, 0x81a2be, 0xb294bb, 0x8abeb7, 0xc5c8c6, 0x666666,
        0xd54e53, 0xb9ca4a, 0xe7c547, 0x7aa6da, 0xc397d8, 0x70c0b1, 0xeaeaea,
    ];
    let mut palette = [None; 16];
    for (slot, value) in palette.iter_mut().zip(ansi) {
        *slot = Some(hex(value));
    }
    TerminalColors {
        foreground: Some(hex(0xa6b2c0)),
        background: Some(hex(0x1b2738)),
        palette,
    }
}

fn color_name(color: Color) -> String {
    match color {
        Color::Reset => "default".into(),
        Color::Rgb(r, g, b) => format!("{r:02x}{g:02x}{b:02x}"),
        other => format!("{other:?}"),
    }
}

#[test]
fn error_toast_matches_the_upstream_gold_grid() {
    // SAFETY: this file has a single test, so nothing else touches the env.
    unsafe { std::env::set_var("COLORTERM", "truecolor") };
    ox_tui::theme::init(Some(gold_terminal()));

    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    let toast = Toast::new(
        Variant::Error,
        None,
        "Directory is required",
        Instant::now(),
    );
    render(&mut buf, area, &toast, ox_tui::theme::palette());

    let gold: serde_json::Value =
        serde_json::from_str(include_str!("parity/gold/gold-toast-30x100.json")).unwrap();
    let mut bad = Vec::new();
    for y in 0..5u16 {
        for x in 0..100u16 {
            let g = &gold["cells"][y as usize][x as usize];
            let (gc, gfg, gbg, gflags) = (
                g[0].as_str().unwrap(),
                g[1].as_str().unwrap(),
                g[2].as_str().unwrap(),
                g[3].as_str().unwrap(),
            );
            let cell = &buf[(x, y)];
            let blank = gc == " " && cell.symbol() == " ";
            let flags = if cell.modifier.contains(Modifier::BOLD) {
                "b"
            } else {
                ""
            };
            if gc != cell.symbol() {
                bad.push(format!("({y},{x}) char {gc:?} vs {:?}", cell.symbol()));
            }
            if !blank && gfg != color_name(cell.fg) {
                bad.push(format!("({y},{x}) fg {gfg} vs {}", color_name(cell.fg)));
            }
            if gbg != color_name(cell.bg) {
                bad.push(format!("({y},{x}) bg {gbg} vs {}", color_name(cell.bg)));
            }
            if !blank && gflags != flags {
                bad.push(format!("({y},{x}) flags {gflags:?} vs {flags:?}"));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} cells differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
