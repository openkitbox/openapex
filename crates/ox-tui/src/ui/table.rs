//! Markdown table layout and grid drawing: pure functions, no I/O.
//!
//! `column_widths` decides final column widths; `grid_lines` places cells that
//! the caller has already wrapped and styled into a box-drawing grid. Wrapping
//! and cell styling stay with the caller so this module never refers back to
//! `markdown.rs` (dependency points markdown -> table only).
//!
//! Both follow opentui's `TextTable` (grid style, `cellPaddingX: 1`,
//! `widthMode: full`, `columnFitter: proportional`):
//! - widths below the target are expanded equally, remainder to the earliest
//!   columns (`TextTable.ts` `expandColumnWidths`, ~745-766);
//! - widths above the target are shrunk by a verbatim port of
//!   `allocateProportionalColumnWidths` (`text-table-width.ts`).
//!
//! Deviations from upstream: none in this module. `comparePriority` drops the
//! BigInt fallback and uses u128 cross products, which is exact for any usize
//! widths.

use std::cmp::Ordering;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Minimum column width: 1 cell of padding on each side plus 1 content cell.
const MIN_COL: usize = 3;

/// Final widths for columns whose natural widths (padding included) are
/// `natural`, given `target` cells available for all columns together
/// (borders already subtracted by the caller).
#[must_use]
pub fn column_widths(natural: &[usize], target: usize) -> Vec<usize> {
    if natural.is_empty() {
        return Vec::new();
    }
    let mut widths: Vec<usize> = natural.iter().map(|&w| w.max(MIN_COL)).collect();
    let total: usize = widths.iter().sum();
    match total.cmp(&target) {
        Ordering::Equal => widths,
        Ordering::Less => {
            // TextTable.ts:745-766 expandColumnWidths
            let columns = widths.len();
            let extra = target - total;
            let shared = extra / columns;
            let remainder = extra % columns;
            for (idx, w) in widths.iter_mut().enumerate() {
                *w += shared + usize::from(idx < remainder);
            }
            widths
        }
        Ordering::Greater => allocate_proportional(&widths, target, MIN_COL),
    }
}

/// text-table-width.ts:1-13 `comparePriority`.
/// Squared cross-products give exact ordering. growth^2 * capacity can reach
/// 192 bits, so u128 would overflow only for widths around 2^43 cells; that is
/// far beyond any real terminal, so there is no BigInt fallback.
fn compare_priority(
    left_growth: usize,
    left_capacity: usize,
    right_growth: usize,
    right_capacity: usize,
) -> Ordering {
    let left = left_growth as u128 * left_growth as u128 * right_capacity as u128;
    let right = right_growth as u128 * right_growth as u128 * left_capacity as u128;
    left.cmp(&right)
}

/// text-table-width.ts:15-97 `allocateProportionalColumnWidths`.
/// `widths` are already >= `min_width` (the caller clamps), so the
/// `Math.max(minWidth, Math.floor(width))` of line 16 is the identity here.
fn allocate_proportional(widths: &[usize], target_width: usize, min_width: usize) -> Vec<usize> {
    // :16-24
    let base_widths = widths;
    let n = base_widths.len();
    let total_base_width: usize = base_widths.iter().sum();
    let capacity: Vec<usize> = base_widths.iter().map(|&w| w - min_width).collect();
    let mut growth = vec![0usize; n];
    let available = target_width
        .saturating_sub(min_width * n)
        .min(total_base_width - min_width * n);

    // :25-26
    if available == 0 {
        return vec![min_width; n];
    }
    if available == capacity.iter().sum::<usize>() {
        return base_widths.to_vec();
    }

    // :28-32 active columns, stable-sorted by ascending weight
    let weights: Vec<f64> = capacity.iter().map(|&c| (c as f64).sqrt()).collect();
    let mut active: Vec<(usize, usize, f64)> = capacity
        .iter()
        .enumerate()
        .filter(|&(_, &width)| width > 0)
        .map(|(idx, &width)| (idx, width, weights[idx]))
        .collect();
    active.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(Ordering::Equal));

    // :33-37 all columns equal capacity
    if active.len() == n && capacity.iter().all(|&w| w == capacity[0]) {
        let shared_growth = available / n;
        let remainder = available % n;
        return (0..n)
            .map(|idx| min_width + shared_growth + usize::from(idx < remainder))
            .collect();
    }
    let mut remaining = available as f64;
    let mut total_weight: f64 = active.iter().map(|c| c.2).sum();

    // :41-47 water-fill: cap short columns first
    for &(idx, width, weight) in &active {
        if remaining / total_weight <= weight {
            break;
        }
        growth[idx] = width;
        remaining -= width as f64;
        total_weight -= weight;
    }

    // :49-53
    let level = remaining / total_weight;
    for &(idx, width, weight) in &active {
        if growth[idx] == width {
            continue;
        }
        growth[idx] = width.min((level * weight).floor() as usize);
    }

    // :55-73 trim over-allocation, worst priority first (ties: later index)
    let mut allocated_growth: usize = growth.iter().sum();
    while allocated_growth > available {
        let mut worst: Option<usize> = None;
        for idx in 0..n {
            if growth[idx] == 0 {
                continue;
            }
            let comparison = match worst {
                None => Ordering::Greater,
                Some(w) => compare_priority(growth[idx], capacity[idx], growth[w], capacity[w]),
            };
            // idx > worst always holds while scanning ascending
            if comparison != Ordering::Less {
                worst = Some(idx);
            }
        }
        let Some(w) = worst else { break };
        growth[w] -= 1;
        allocated_growth -= 1;
    }

    // :75-94 hand out the flooring leftovers by exact priority
    while allocated_growth < available {
        let mut best: Option<usize> = None;
        for idx in 0..n {
            if growth[idx] >= capacity[idx] {
                continue;
            }
            let comparison = match best {
                None => Ordering::Less,
                Some(b) => {
                    compare_priority(growth[idx] + 1, capacity[idx], growth[b] + 1, capacity[b])
                }
            };
            if comparison == Ordering::Less {
                best = Some(idx);
            }
        }
        let Some(b) = best else { break };
        growth[b] += 1;
        allocated_growth += 1;
    }

    // :96
    growth.into_iter().map(|w| w + min_width).collect()
}

/// Which frame row to draw.
#[derive(Clone, Copy)]
enum Rule {
    Top,
    Mid,
    Bottom,
}

impl Rule {
    fn glyphs(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Rule::Top => ("┌", "┬", "┐"),
            Rule::Mid => ("├", "┼", "┤"),
            Rule::Bottom => ("└", "┴", "┘"),
        }
    }
}

fn rule_line(rule: Rule, widths: &[usize], indent: &str, border: Style) -> Line<'static> {
    let (left, mid, right) = rule.glyphs();
    let mut text = String::from(left);
    for (i, &w) in widths.iter().enumerate() {
        if i > 0 {
            text.push_str(mid);
        }
        text.push_str(&"─".repeat(w));
    }
    text.push_str(right);
    Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled(text, border),
    ])
}

/// Draw `cells` (`cells[row][col]` = already wrapped physical lines of spans)
/// into a grid with column widths `widths` (padding included). The spans are
/// moved into the output, not cloned. Every output line starts with `indent`;
/// `border` styles frame glyphs only.
pub fn grid_lines(
    cells: Vec<Vec<Vec<Vec<Span<'static>>>>>,
    widths: &[usize],
    indent: &str,
    border: Style,
) -> Vec<Line<'static>> {
    if cells.is_empty() || widths.is_empty() {
        return Vec::new();
    }
    let mut out = vec![rule_line(Rule::Top, widths, indent, border)];
    for (r, row) in cells.into_iter().enumerate() {
        if r > 0 {
            out.push(rule_line(Rule::Mid, widths, indent, border));
        }
        let height = row.iter().map(Vec::len).max().unwrap_or(0).max(1);
        let mut columns: Vec<_> = row.into_iter().map(Vec::into_iter).collect();
        for _ in 0..height {
            let mut spans = vec![Span::raw(indent.to_string()), Span::styled("│", border)];
            for (c, &w) in widths.iter().enumerate() {
                let content = w.saturating_sub(2);
                spans.push(Span::raw(" "));
                let mut used = 0;
                if let Some(line) = columns.get_mut(c).and_then(Iterator::next) {
                    for span in line {
                        used += UnicodeWidthStr::width(span.content.as_ref());
                        spans.push(span);
                    }
                }
                spans.push(Span::raw(" ".repeat(content.saturating_sub(used) + 1)));
                spans.push(Span::styled("│", border));
            }
            out.push(Line::from(spans));
        }
    }
    out.push(rule_line(Rule::Bottom, widths, indent, border));
    out
}
