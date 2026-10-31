//! Session view vs the upstream opencode 2.2.3 gold grids
//! (`tests/parity/gold/gold-conv-*.json`, cells `[char, fg, bg, flags]`).
//!
//! Input is the session's message list exactly as the server returns it
//! (`fixtures/conv-messages.json`, newest first): user prompt, an assistant
//! step with reasoning + a `read` call, and the markdown answer. It goes
//! through the production path (`history::parse_messages` → store →
//! `blocks_from_store` → `Transcript`) and is drawn with the same layout as
//! `App::run`. One `#[test]`: the palette is process-global.

use ox_client::history::parse_messages;
use ox_client::store::DataStore;
use ox_schema::message::Message;
use ox_tui::pipeline::blocks_from_store;
use ox_tui::theme::terminal::{Rgb, TerminalColors};
use ox_tui::ui::dock::{StatusMode, status_line};
use ox_tui::ui::layout;
use ox_tui::ui::prompt::{Chrome, Footer, Prompt};
use ox_tui::ui::timeline::Transcript;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::widgets::Paragraph;
use std::time::{Duration, Instant};

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

fn store() -> DataStore {
    let raw = include_str!("parity/fixtures/conv-messages.json");
    let mut messages: Vec<Message> = serde_json::from_str(raw).expect("wire messages");
    messages.reverse(); // the endpoint is newest first
    let mut store = DataStore::default();
    store.replace_items(parse_messages(&messages));
    store
}

fn render(width: u16, height: u16) -> Buffer {
    let store = store();
    let chrome = Chrome {
        directory: "/tmp/parity-cwd".into(),
        ..Chrome::default()
    };
    let mut prompt = Prompt::new("", chrome, Footer::Session);
    prompt.tick(Instant::now() + Duration::from_millis(600));
    prompt.set_ctx(store.context_tokens().map(ox_tui::ui::dock::format_tokens));

    let mut transcript = Transcript::new(width, height);
    transcript.sync(&blocks_from_store(&store), 0);

    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let full = frame.area();
            let prompt_h = prompt.height(full.height).min(full.height - 1);
            let (timeline, prompt_area, dock) = layout::split(full, prompt_h);
            prompt.render(frame, layout::padded(prompt_area, full.width), full, false);
            transcript.render(frame, layout::padded(timeline, full.width));
            let line = status_line(&store, StatusMode::Normal);
            frame.render_widget(Paragraph::new(line), dock);
        })
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

/// Cells that differ from the gold, as `(row, col, why)`, judged the way
/// `compare.py` does (blank cells ignore fg and flags).
fn diff(gold: &str, buf: &Buffer) -> Vec<String> {
    let gold: serde_json::Value = serde_json::from_str(gold).unwrap();
    let mut out = Vec::new();
    for y in 0..buf.area.height {
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
fn session_matches_the_upstream_gold_grids() {
    // SAFETY: this file has a single test, so nothing else touches the env.
    unsafe { std::env::set_var("COLORTERM", "truecolor") };
    ox_tui::theme::init(Some(gold_terminal()));

    for (width, height, gold) in [
        (100, 30, include_str!("parity/gold/gold-conv-30x100.json")),
        (80, 24, include_str!("parity/gold/gold-conv-24x80.json")),
    ] {
        let buf = render(width, height);
        let bad = diff(gold, &buf);
        assert!(
            bad.is_empty(),
            "{height}x{width}: {} cells differ:\n{}\n--- ox rendered:\n{}",
            bad.len(),
            bad.join("\n"),
            text_rows(&buf)
        );
    }
}
