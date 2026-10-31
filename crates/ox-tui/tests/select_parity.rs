//! Select dialog vs the upstream opencode 2.2.3 command palette
//! (`tests/parity/gold/gold-select-30x100.json`, cells `[char, fg, bg, flags]`).
//!
//! The gold is upstream's command palette over the session view. The pure
//! Select render is given the same title and items; the compared region is the
//! panel (rows 8..=22, cols 20..=79) plus the dimmed backdrop's background
//! everywhere else in those rows (the backdrop text is the session view, which
//! this test does not draw). One `#[test]`: the palette is process-global.
//!
//! The real palette builder (`ui::palette::select` with `app::keymap::hint`)
//! omits the gold's `Connect an integration`, `Open settings` and `Queue
//! prompt` rows (ox has no such features), so it is checked row by row: every
//! row it shares with the gold must render with the gold's exact text.

use ox_tui::app::keymap;
use ox_tui::theme::terminal::{Rgb, TerminalColors};
use ox_tui::ui::dialog::{Dialog, Item, Select, render};
use ox_tui::ui::palette::{self, Context};
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

const PANEL: (u16, u16, u16, u16) = (20, 8, 80, 23);

fn commands() -> Select {
    Select::new(
        "Commands",
        vec![
            Item::new("session.list", "Switch session")
                .category("Suggested")
                .footer("ctrl+x l"),
            Item::new("session.new", "New session")
                .category("Suggested")
                .footer("ctrl+x n"),
            Item::new("model.list", "Switch model")
                .category("Suggested")
                .footer("ctrl+x m"),
            Item::new("provider.connect", "Connect an integration").category("Suggested"),
            Item::new("settings", "Open settings").category("Suggested"),
            Item::new("prompt.queue", "Queue prompt")
                .category("Prompt")
                .footer("ctrl+x enter"),
        ],
    )
}

fn render_select(select: Select, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let dialog = Dialog::Select(select);
    terminal
        .draw(|frame| render(frame, frame.area(), &dialog))
        .unwrap();
    terminal.backend().buffer().clone()
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
/// cells ignore fg and flags). Outside the panel only the background counts.
fn diff(gold: &str, buf: &Buffer) -> Vec<String> {
    let gold: serde_json::Value = serde_json::from_str(gold).unwrap();
    let (x0, y0, x1, y1) = PANEL;
    let mut out = Vec::new();
    for y in y0..y1 {
        for x in 0..buf.area.width {
            let inside = (x0..x1).contains(&x);
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
            if inside && gc != cell.symbol() {
                bad.push(format!("char {gc:?} vs {:?}", cell.symbol()));
            }
            if inside && !blank && gfg != color_name(cell.fg) {
                bad.push(format!("fg {gfg} vs {}", color_name(cell.fg)));
            }
            if gbg != color_name(cell.bg) {
                bad.push(format!("bg {gbg} vs {}", color_name(cell.bg)));
            }
            let flags = flags_of(cell.modifier);
            if inside && !blank && gflags != flags {
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
fn select_matches_the_upstream_command_palette_gold() {
    // SAFETY: this file has a single test, so nothing else touches the env.
    unsafe { std::env::set_var("COLORTERM", "truecolor") };
    ox_tui::theme::init(Some(gold_terminal()));

    let buf = render_select(commands(), 100, 30);
    let bad = diff(include_str!("parity/gold/gold-select-30x100.json"), &buf);
    assert!(
        bad.is_empty(),
        "{} cells differ:\n{}\n--- ox rendered:\n{}",
        bad.len(),
        bad.join("\n"),
        text_rows(&buf)
    );
    real_palette_shares_its_rows_with_the_gold(include_str!("parity/gold/gold-select-30x100.json"));
}

/// Panel text of every row, trimmed at the right.
fn panel_lines(buf: &Buffer) -> Vec<String> {
    let (x0, _, x1, _) = PANEL;
    (0..buf.area.height)
        .map(|y| {
            let row: String = (x0..x1).map(|x| buf[(x, y)].symbol()).collect();
            row.trim_end().to_owned()
        })
        .collect()
}

fn gold_panel_lines(gold: &str) -> Vec<String> {
    let gold: serde_json::Value = serde_json::from_str(gold).unwrap();
    let (x0, _, x1, _) = PANEL;
    gold["cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let text: String = (x0..x1)
                .map(|x| row[x as usize][0].as_str().unwrap())
                .collect();
            text.trim_end().to_owned()
        })
        .collect()
}

/// The real palette builder against the gold's rows it shares.
fn real_palette_shares_its_rows_with_the_gold(gold_json: &str) {
    let select = palette::select(
        Context {
            in_session: true,
            has_queue: false,
        },
        keymap::hint,
    );
    let ox = panel_lines(&render_select(select, 100, 30));
    let gold = gold_panel_lines(gold_json);
    for shared in [
        "Commands",
        "Search",
        "Suggested",
        "Switch session",
        "New session",
        "Switch model",
    ] {
        let in_gold = gold.iter().find(|line| line.contains(shared));
        let in_ox = ox.iter().find(|line| line.contains(shared));
        assert_eq!(in_ox, in_gold, "row {shared:?}\n{}", ox.join("\n"));
    }
}
