//! Pipe tables through the public transcript and compaction paths.

use ox_client::store::CompactionStatus;
use ox_tui::ui::compaction::{View, lines};
use ox_tui::ui::timeline::{Block, Transcript};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::CellWidth;
use unicode_width::UnicodeWidthStr;

const TABLE_TEXT: &str =
    "intro\n\n| 名称 | 说明 |\n|---|---|\n| foo | `bar` |\n| baz | **qux** |\n\nafter";

/// Draw the transcript on a `TestBackend` and read the buffer back as rows,
/// skipping the continuation cells of wide glyphs.
fn screen(t: &mut Transcript, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| t.render(frame, frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            let mut row = String::new();
            let mut tail = 0u16;
            for x in 0..width {
                if tail > 0 {
                    tail -= 1;
                    continue;
                }
                let symbol = buffer[(x, y)].symbol();
                row.push_str(symbol);
                tail = symbol.cell_width().saturating_sub(1);
            }
            row.trim_end().to_owned()
        })
        .collect()
}

fn render_table(width: u16, height: u16) -> Vec<String> {
    let mut transcript = Transcript::new(width, height);
    transcript.sync(&[Block::assistant("a1", 1, TABLE_TEXT)], 0);
    screen(&mut transcript, width, height)
}

fn row_index(rows: &[String], needle: &str) -> usize {
    rows.iter()
        .position(|r| r.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{}", rows.join("\n")))
}

#[test]
fn assistant_table_draws_a_frame_between_its_paragraphs() {
    let rows = render_table(60, 16);
    let screen = rows.join("\n");

    for glyph in ["┌", "┬", "┐", "├", "┼", "┤", "└", "┴", "┘", "│"] {
        assert!(screen.contains(glyph), "missing {glyph}:\n{screen}");
    }
    for cell in ["名称", "说明", "foo", "bar", "baz", "qux"] {
        assert!(screen.contains(cell), "missing {cell}:\n{screen}");
    }
    assert!(!screen.contains("|---|"), "raw delimiter row:\n{screen}");
    assert!(!screen.contains('`') && !screen.contains("**"), "{screen}");

    let intro = row_index(&rows, "intro");
    let top = row_index(&rows, "┌");
    let bottom = row_index(&rows, "└");
    let after = row_index(&rows, "after");
    assert!(intro < top && top < bottom && bottom < after, "{screen}");
}

#[test]
fn narrow_table_wraps_cells_within_the_width() {
    let width = 24u16;
    let rows = render_table(width, 24);
    let screen = rows.join("\n");

    for row in &rows {
        assert!(row.width() <= usize::from(width), "{row:?} too wide");
    }
    assert!(screen.contains('┌') && screen.contains('┘'), "{screen}");

    // Strip the frame and whitespace: every cell's text must survive intact
    // even when it was split across wrapped lines.
    let body: String = rows
        .iter()
        .filter(|r| r.contains('│'))
        .flat_map(|r| r.chars())
        .filter(|c| !c.is_whitespace() && *c != '│')
        .collect();
    for cell in ["名称", "说明", "foo", "bar", "baz", "qux"] {
        assert!(body.contains(cell), "{cell} broken in {body:?}\n{screen}");
    }
}

#[test]
fn compaction_summary_renders_tables() {
    let view = View {
        status: CompactionStatus::Completed,
        text: "Summary\n\n| key | value |\n|---|---|\n| goal | ship |",
        cancelled: false,
        tokens: None,
        spinner: '⠋',
    };
    let rows: Vec<String> = lines(&view, 40)
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    let all = rows.join("\n");

    for glyph in ["┌", "┬", "┐", "├", "┼", "┤", "└", "┴", "┘"] {
        assert!(all.contains(glyph), "missing {glyph}:\n{all}");
    }
    assert!(all.contains("goal") && all.contains("ship"), "{all}");
    assert!(!all.contains("|---|"), "{all}");
    for row in &rows {
        assert!(row.width() <= 40, "{row:?} too wide");
    }
}
