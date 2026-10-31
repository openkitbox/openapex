//! Home screen vs the upstream opencode 2.2.3 gold grids
//! (`tests/parity/gold/gold-home-*.json`, cells `[char, fg, bg, flags]`).
//!
//! The palette is process-global, so this file holds exactly one `#[test]`:
//! it installs the terminal the gold was captured under (the `system` theme
//! built from `~/.local/state/opencode/latest/tui/system-theme.json`), then
//! renders Home at both sizes and compares every cell the way `compare.py`
//! does (blank cells ignore fg). That terminal palette is test data; the
//! product code derives every colour from whatever the terminal reports.

use ox_tui::theme::terminal::{Rgb, TerminalColors};
use ox_tui::ui::home;
use ox_tui::ui::prompt::{Chrome, Footer, McpSummary, Prompt, placeholder_line};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
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

fn chrome() -> Chrome {
    Chrome {
        agent: Some("Build".into()),
        model: Some("k3-256k".into()),
        provider: Some("Cardinal".into()),
        variant: None,
        // What the server reports for `--directory /tmp/parity-cwd`.
        directory: "/private/tmp/parity-cwd".into(),
        branch: None,
        mcp: Some(McpSummary {
            connected: 4,
            failed: 0,
        }),
    }
}

fn render(width: u16, height: u16, placeholder: usize) -> Buffer {
    let mut prompt = Prompt::new(&placeholder_line(placeholder), chrome(), Footer::Home);
    // The cursor is a blinking block: capture it in its hidden phase.
    prompt.tick(Instant::now() + Duration::from_millis(600));
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| home::render(frame, frame.area(), &mut prompt))
        .unwrap();
    terminal.backend().buffer().clone()
}

/// Cells that differ from the gold, as `(row, col, why)`. `ignore` marks the
/// cells whose *character* legitimately differs (the version string).
fn diff(gold: &str, buf: &Buffer, ignore: &dyn Fn(u16, u16) -> bool) -> Vec<String> {
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
            let flags = if cell.modifier.contains(Modifier::BOLD) {
                "b"
            } else {
                ""
            };
            let mut bad = Vec::new();
            if gc != cell.symbol() && !ignore(y, x) {
                bad.push(format!("char {gc:?} vs {:?}", cell.symbol()));
            }
            if !blank && gfg != color_name(cell.fg) {
                bad.push(format!("fg {gfg} vs {}", color_name(cell.fg)));
            }
            if gbg != color_name(cell.bg) {
                bad.push(format!("bg {gbg} vs {}", color_name(cell.bg)));
            }
            if gflags != flags && !blank {
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
fn home_matches_the_upstream_gold_grids() {
    // SAFETY: this file has a single test, so nothing else touches the env.
    unsafe { std::env::set_var("COLORTERM", "truecolor") };
    ox_tui::theme::init(Some(gold_terminal()));

    // 30x100: placeholder #0, version in the last five cells of row 28.
    let buf = render(100, 30, 0);
    let bad = diff(
        include_str!("parity/gold/gold-home-30x100.json"),
        &buf,
        &|y, x| y == 28 && x >= 93,
    );
    assert!(
        bad.is_empty(),
        "30x100: {} cells differ:\n{}",
        bad.len(),
        bad.join("\n")
    );

    // 24x80: placeholder #2, version in the last five cells of row 22.
    let buf = render(80, 24, 2);
    let bad = diff(
        include_str!("parity/gold/gold-home-24x80.json"),
        &buf,
        &|y, x| y == 22 && x >= 73,
    );
    assert!(
        bad.is_empty(),
        "24x80: {} cells differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
