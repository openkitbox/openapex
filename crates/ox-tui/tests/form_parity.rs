//! Form dialog vs the upstream opencode 2.2.3 form prompt
//! (`tests/parity/gold/gold-form-30x100.json`, cells `[char, fg, bg, flags]`).
//!
//! The gold is a pending one-field form (string with two options, one with a
//! description, `custom: true`) shown in an otherwise empty session. Upstream
//! replaces the prompt with the form, so the compared region is the bottom
//! 14 rows: the form box and the blank rows around it. One `#[test]`: the
//! palette is process-global.

use ox_client::form::FormInfo;
use ox_tui::theme::terminal::{Rgb, TerminalColors};
use ox_tui::ui::dialog::{Dialog, render};
use ox_tui::ui::form::FormState;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

fn hex(value: u32) -> Rgb {
    Rgb::new((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

/// What the capture machine's terminal reported to upstream.
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

fn flags_of(modifier: Modifier) -> String {
    let mut out = String::new();
    if modifier.contains(Modifier::BOLD) {
        out.push('b');
    }
    if modifier.contains(Modifier::UNDERLINED) {
        out.push('u');
    }
    out
}

const FIRST_ROW: u16 = 16;

fn gold_form() -> FormInfo {
    serde_json::from_value(serde_json::json!({
        "id": "frm_gold1", "sessionID": "ses_gold", "title": "Pick a stack",
        "fields": [{
            "type": "string", "key": "stack", "title": "Stack",
            "description": "Which stack do you want?",
            "options": [
                {"value": "rust", "label": "Rust", "description": "fast and safe"},
                {"value": "go", "label": "Go"}
            ],
            "custom": true
        }]
    }))
    .unwrap()
}

fn text_rows(buf: &Buffer) -> String {
    (0..buf.area.height)
        .map(|y| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            format!("{:>2}|{}", y, row.trim_end())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Cells that differ from the gold, judged the way `compare.py` does (blank
/// cells ignore fg and flags).
fn diff(gold: &str, buf: &Buffer) -> Vec<String> {
    let gold: serde_json::Value = serde_json::from_str(gold).unwrap();
    let mut out = Vec::new();
    for y in FIRST_ROW..buf.area.height {
        for x in 0..buf.area.width {
            let g = &gold["cells"][y as usize][x as usize];
            let (gc, gfg, gbg, gflags) = (
                g[0].as_str().unwrap(),
                g[1].as_str().unwrap(),
                g[2].as_str().unwrap(),
                g[3].as_str().unwrap(),
            );
            let cell = &buf[(x, y)];
            let blank = gc == " " && cell.symbol() == " ";
            let mut bad = Vec::new();
            if gc != cell.symbol() {
                bad.push(format!("char {gc:?} vs {:?}", cell.symbol()));
            }
            if !blank && gfg != color_name(cell.fg) {
                bad.push(format!("fg {gfg} vs {}", color_name(cell.fg)));
            }
            if gbg != color_name(cell.bg) {
                bad.push(format!("bg {gbg} vs {}", color_name(cell.bg)));
            }
            let flags = flags_of(cell.modifier);
            if !blank && gflags != flags {
                bad.push(format!("flags {gflags:?} vs {flags:?}"));
            }
            if !bad.is_empty() {
                out.push(format!("({y},{x}) {}", bad.join(", ")));
            }
        }
    }
    out
}

#[test]
fn form_matches_the_upstream_form_prompt_gold() {
    // SAFETY: this file has a single test, so nothing else touches the env.
    unsafe { std::env::set_var("COLORTERM", "truecolor") };
    ox_tui::theme::init(Some(gold_terminal()));

    let dialog = Dialog::Form(Box::new(FormState::new(&gold_form())));
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| render(frame, frame.area(), &dialog))
        .unwrap();
    let buf = terminal.backend().buffer().clone();

    let bad = diff(include_str!("parity/gold/gold-form-30x100.json"), &buf);
    assert!(
        bad.is_empty(),
        "{} cells differ:\n{}\n--- ox rendered:\n{}",
        bad.len(),
        bad.join("\n"),
        text_rows(&buf)
    );
}
