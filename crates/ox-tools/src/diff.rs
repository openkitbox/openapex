//! Unified-diff rendering for tool results.
//!
//! Upstream produces a `FileDiff.Info` for every file mutation (`edit`,
//! `write`, `patch`) through `createTwoFilesPatch`, so an opencode client can
//! render a diff card from the tool part's `metadata.files` instead of the
//! model-facing text. ox has no formatter or snapshot store, but the client
//! contract only needs the patch itself.
//!
//! This module owns exactly that: given the text before and after a change, it
//! returns the `{ file, patch, additions, deletions, status }` record the
//! client decodes. It holds no tool state and performs no I/O, so `edit`,
//! `write` and `patch` share one implementation and one set of tests.
//!
//! Patches are display artifacts, never applied. Hunks carry
//! [`CONTEXT_LINES`] lines of context, and an edit distance above
//! [`MAX_EDIT_DISTANCE`] degrades to a whole-file replacement instead of
//! allocating an unbounded trace.

use serde::ser::{Serialize, SerializeStruct, Serializer};

/// Lines of unchanged context kept around each hunk.
const CONTEXT_LINES: usize = 3;
/// Above this edit distance the pair is rendered as a full replacement, which
/// bounds the Myers trace to `MAX_EDIT_DISTANCE` rows.
const MAX_EDIT_DISTANCE: usize = 2_000;

/// Upstream `FileDiff.Info.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffStatus {
    Added,
    Deleted,
    Modified,
}

impl DiffStatus {
    fn as_str(self) -> &'static str {
        match self {
            DiffStatus::Added => "added",
            DiffStatus::Deleted => "deleted",
            DiffStatus::Modified => "modified",
        }
    }
}

/// Upstream `FileDiff.Info`: one changed file as the client renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Path as the model addressed it (the tool's `resource`).
    pub file: String,
    /// Unified diff text.
    pub patch: String,
    pub additions: u64,
    pub deletions: u64,
    pub status: DiffStatus,
}

impl Serialize for FileDiff {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("FileDiff", 5)?;
        state.serialize_field("file", &self.file)?;
        state.serialize_field("patch", &self.patch)?;
        state.serialize_field("additions", &self.additions)?;
        state.serialize_field("deletions", &self.deletions)?;
        state.serialize_field("status", self.status.as_str())?;
        state.end()
    }
}

/// The diff between two texts of one file. `before` is empty for a created
/// file and `after` is empty for a deleted one; `status` names which.
pub fn file_diff(file: &str, before: &str, after: &str, status: DiffStatus) -> FileDiff {
    let before_lines = split_lines(before);
    let after_lines = split_lines(after);
    let ops = line_ops(&before_lines, &after_lines);
    let additions = ops
        .iter()
        .filter(|op| matches!(op, Op::Insert { .. }))
        .count() as u64;
    let deletions = ops
        .iter()
        .filter(|op| matches!(op, Op::Delete { .. }))
        .count() as u64;
    FileDiff {
        file: file.to_owned(),
        patch: render_patch(file, &before_lines, &after_lines, &ops),
        additions,
        deletions,
        status,
    }
}

/// A file removed in full: an all-deletions hunk against `/dev/null`.
pub fn deleted_file(file: &str, before: &str) -> FileDiff {
    file_diff(file, before, "", DiffStatus::Deleted)
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    // A trailing newline terminates the last line; it does not add an empty one.
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// One step of the edit script. Indexes address `before` or `after`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Equal { before: usize, after: usize },
    Delete { before: usize },
    Insert { after: usize },
}

/// Myers' O(ND) line diff, or "delete everything then insert everything" when
/// the edit distance exceeds [`MAX_EDIT_DISTANCE`].
fn line_ops(before: &[&str], after: &[&str]) -> Vec<Op> {
    if before.is_empty() {
        return (0..after.len()).map(|after| Op::Insert { after }).collect();
    }
    if after.is_empty() {
        return (0..before.len())
            .map(|before| Op::Delete { before })
            .collect();
    }
    myers(before, after).unwrap_or_else(|| {
        let mut ops: Vec<Op> = (0..before.len())
            .map(|before| Op::Delete { before })
            .collect();
        ops.extend((0..after.len()).map(|after| Op::Insert { after }));
        ops
    })
}

fn myers(before: &[&str], after: &[&str]) -> Option<Vec<Op>> {
    let n = before.len() as isize;
    let m = after.len() as isize;
    let bound = ((n + m) as usize).min(MAX_EDIT_DISTANCE);
    let offset = bound as isize + 1;
    let mut furthest = vec![0isize; 2 * bound + 3];
    // trace[d] is the `furthest` state before distance `d` ran, which is what
    // backtracking needs. Its size is bounded by `MAX_EDIT_DISTANCE`.
    let mut trace: Vec<Vec<isize>> = Vec::with_capacity(bound + 1);
    for distance in 0..=bound as isize {
        trace.push(furthest.clone());
        let mut diagonal = -distance;
        while diagonal <= distance {
            let index = (diagonal + offset) as usize;
            let mut x = if diagonal == -distance
                || (diagonal != distance && furthest[index - 1] < furthest[index + 1])
            {
                furthest[index + 1]
            } else {
                furthest[index - 1] + 1
            };
            let mut y = x - diagonal;
            while x < n && y < m && before[x as usize] == after[y as usize] {
                x += 1;
                y += 1;
            }
            furthest[index] = x;
            if x >= n && y >= m {
                return Some(backtrack(&trace, offset, n, m));
            }
            diagonal += 2;
        }
    }
    None
}

/// Walk the stored traces backwards from `(n, m)`, emitting ops in forward
/// order. `trace[d]` is the state before distance `d`, so the loop starts at
/// the last recorded distance.
fn backtrack(trace: &[Vec<isize>], offset: isize, n: isize, m: isize) -> Vec<Op> {
    let mut ops: Vec<Op> = Vec::new();
    let mut x = n;
    let mut y = m;
    for (distance, furthest) in trace.iter().enumerate().rev() {
        let distance = distance as isize;
        let diagonal = x - y;
        let index = (diagonal + offset) as usize;
        let previous_diagonal = if diagonal == -distance
            || (diagonal != distance && furthest[index - 1] < furthest[index + 1])
        {
            diagonal + 1
        } else {
            diagonal - 1
        };
        let previous_x = furthest[(previous_diagonal + offset) as usize];
        let previous_y = previous_x - previous_diagonal;
        while x > previous_x && y > previous_y {
            ops.push(Op::Equal {
                before: (x - 1) as usize,
                after: (y - 1) as usize,
            });
            x -= 1;
            y -= 1;
        }
        if distance > 0 {
            if x == previous_x {
                ops.push(Op::Insert {
                    after: (y - 1) as usize,
                });
            } else {
                ops.push(Op::Delete {
                    before: (x - 1) as usize,
                });
            }
        }
        x = previous_x;
        y = previous_y;
    }
    ops.reverse();
    ops
}

fn render_patch(file: &str, before: &[&str], after: &[&str], ops: &[Op]) -> String {
    // Header: a created file has no before side, so it names /dev/null.
    let mut patch = String::new();
    patch.push_str(if before.is_empty() {
        "--- /dev/null\n"
    } else {
        "--- "
    });
    if !before.is_empty() {
        patch.push_str(file);
        patch.push('\n');
    }
    patch.push_str("+++ ");
    patch.push_str(file);
    patch.push('\n');

    // Line numbers per op, so a hunk header never has to re-walk the script.
    let mut positions: Vec<(usize, usize)> = Vec::with_capacity(ops.len() + 1);
    let mut before_line = 1usize;
    let mut after_line = 1usize;
    for op in ops {
        positions.push((before_line, after_line));
        match op {
            Op::Equal { .. } => {
                before_line += 1;
                after_line += 1;
            }
            Op::Delete { .. } => before_line += 1,
            Op::Insert { .. } => after_line += 1,
        }
    }
    positions.push((before_line, after_line));

    for (start, end) in hunks(ops) {
        let (before_start, after_start) = positions[start];
        let mut before_count = 0usize;
        let mut after_count = 0usize;
        let mut lines = String::with_capacity((end - start) * 32);
        for op in &ops[start..end] {
            match *op {
                Op::Equal { before: index, .. } => {
                    before_count += 1;
                    after_count += 1;
                    lines.push(' ');
                    lines.push_str(before[index]);
                    lines.push('\n');
                }
                Op::Delete { before: index } => {
                    before_count += 1;
                    lines.push('-');
                    lines.push_str(before[index]);
                    lines.push('\n');
                }
                Op::Insert { after: index } => {
                    after_count += 1;
                    lines.push('+');
                    lines.push_str(after[index]);
                    lines.push('\n');
                }
            }
        }
        patch.push_str(&format!(
            "@@ -{}{} +{}{} @@\n",
            if before_count == 0 {
                before_start.saturating_sub(1)
            } else {
                before_start
            },
            range_suffix(before_count),
            if after_count == 0 {
                after_start.saturating_sub(1)
            } else {
                after_start
            },
            range_suffix(after_count),
        ));
        patch.push_str(&lines);
    }
    patch
}

/// Hunks as `[start, end)` ranges over the op list, with context applied and
/// overlapping context merged.
fn hunks(ops: &[Op]) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut index = 0;
    while index < ops.len() {
        if matches!(ops[index], Op::Equal { .. }) {
            index += 1;
            continue;
        }
        let start = index.saturating_sub(CONTEXT_LINES);
        let mut end = index + 1;
        let mut cursor = index + 1;
        while cursor < ops.len() {
            if matches!(ops[cursor], Op::Equal { .. }) {
                let run_start = cursor;
                while cursor < ops.len() && matches!(ops[cursor], Op::Equal { .. }) {
                    cursor += 1;
                }
                let run = cursor - run_start;
                if cursor == ops.len() {
                    // Trailing context: keep at most CONTEXT_LINES lines.
                    end = run_start + run.min(CONTEXT_LINES);
                } else if run > CONTEXT_LINES * 2 {
                    // A long unchanged run closes this hunk.
                    end = run_start + CONTEXT_LINES;
                } else {
                    // Short run: absorb it and keep going.
                    end = cursor;
                    continue;
                }
                break;
            }
            end = cursor + 1;
            cursor += 1;
        }
        index = cursor.max(index + 1);
        ranges.push((start, end));
    }
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

fn range_suffix(count: usize) -> String {
    if count == 1 {
        String::new()
    } else {
        format!(",{count}")
    }
}
