//! Line diffs for the tools that change files, and how to draw them.
//!
//! `edit_file` and `write_file` know both sides of the change while they are
//! making it, so the diff is computed there and carried out with the tool
//! result. Everything downstream — the activity line, the expanded body,
//! the `/call` report — reads this one structure.
//!
//! The drawing rules follow the theme's rule for diffs: the line number and
//! the `+`/`-` sign carry the only colour, the text stays neutral. A hunk is
//! three lines of context on either side of a change, hunks that touch are
//! merged, and a gap between them is a single `⋯`.

use crate::theme::Theme;
use serde::{Deserialize, Serialize};

/// Unchanged lines kept either side of a change.
const CONTEXT: usize = 3;
/// Above this many LCS cells the middle of a change is described rather than
/// aligned line by line: the table is quadratic, and a whole-file rewrite
/// would otherwise stall the turn to draw something nobody reads.
const LCS_CELL_BUDGET: usize = 2_000_000;
/// Tab width used while rendering, so the gutter stays a fixed column.
const TAB: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Keep,
    Add,
    Remove,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Line {
    pub op: Op,
    /// 1-based line number: in the new file for kept and added lines, in the
    /// old file for removed ones — the numbering a reader wants when asking
    /// "where do I look now?".
    pub number: usize,
    pub text: String,
}

/// A run of lines with changes in it, plus the context around them.
#[derive(Clone, Serialize, Deserialize)]
pub struct Hunk {
    pub lines: Vec<Line>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FileDiff {
    /// Workspace-relative where possible; what the activity line shows.
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub hunks: Vec<Hunk>,
    /// The file did not exist before, so every line is an addition.
    pub created: bool,
    /// The change was too big to align line by line; the hunk is the whole
    /// old block removed and the whole new block added.
    pub coarse: bool,
}

impl FileDiff {
    /// `+8 −3`, or `+40` for a new file. Empty when nothing changed.
    pub fn stat(&self) -> String {
        match (self.added, self.removed) {
            (0, 0) => String::new(),
            (a, 0) => format!("+{a}"),
            (0, r) => format!("−{r}"),
            (a, r) => format!("+{a} −{r}"),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }

    /// Every line across every hunk, ignoring the gaps between them.
    fn total_lines(&self) -> usize {
        self.hunks.iter().map(|h| h.lines.len()).sum()
    }
}

// ------------------------------------------------------------------ compute

/// Diff two texts. `created` marks the case where there was no old file at
/// all, which reads differently even though the line arithmetic is the same.
pub fn compute(path: &str, old: &str, new: &str, created: bool) -> FileDiff {
    let old_lines: Vec<&str> = split(old);
    let new_lines: Vec<&str> = split(new);

    // A change is nearly always a small middle inside two long unchanged
    // ends. Trimming them first is what keeps a 5000-line file affordable.
    let mut head = 0;
    while head < old_lines.len() && head < new_lines.len() && old_lines[head] == new_lines[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < old_lines.len() - head
        && tail < new_lines.len() - head
        && old_lines[old_lines.len() - 1 - tail] == new_lines[new_lines.len() - 1 - tail]
    {
        tail += 1;
    }
    let old_mid = &old_lines[head..old_lines.len() - tail];
    let new_mid = &new_lines[head..new_lines.len() - tail];

    let coarse = old_mid.len().saturating_mul(new_mid.len()) > LCS_CELL_BUDGET;
    let mut ops: Vec<(Op, &str)> = Vec::new();
    for line in &old_lines[..head] {
        ops.push((Op::Keep, line));
    }
    if coarse {
        for line in old_mid {
            ops.push((Op::Remove, line));
        }
        for line in new_mid {
            ops.push((Op::Add, line));
        }
    } else {
        ops.extend(align(old_mid, new_mid));
    }
    for line in &old_lines[old_lines.len() - tail..] {
        ops.push((Op::Keep, line));
    }

    let added = ops.iter().filter(|(op, _)| *op == Op::Add).count();
    let removed = ops.iter().filter(|(op, _)| *op == Op::Remove).count();
    FileDiff {
        path: path.to_string(),
        added,
        removed,
        hunks: hunks(&ops),
        created,
        coarse,
    }
}

/// `str::lines` drops a trailing newline, which is what we want — a file
/// ending in "\n" has no final empty line — but an empty file must come back
/// as no lines rather than one blank one.
fn split(text: &str) -> Vec<&str> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.lines().collect()
    }
}

/// Longest common subsequence, walked back into an edit script. Quadratic in
/// both time and memory, which is why the caller trims the ends first and
/// gives up past `LCS_CELL_BUDGET`.
fn align<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<(Op, &'a str)> {
    let (n, m) = (old.len(), new.len());
    if n == 0 || m == 0 {
        let mut ops = Vec::with_capacity(n + m);
        ops.extend(old.iter().map(|line| (Op::Remove, *line)));
        ops.extend(new.iter().map(|line| (Op::Add, *line)));
        return ops;
    }
    // table[i][j] = LCS length of old[i..] and new[j..]
    let mut table = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if old[i] == new[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut ops = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old[i] == new[j] {
            ops.push((Op::Keep, old[i]));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            ops.push((Op::Remove, old[i]));
            i += 1;
        } else {
            ops.push((Op::Add, new[j]));
            j += 1;
        }
    }
    ops.extend(old[i..].iter().map(|line| (Op::Remove, *line)));
    ops.extend(new[j..].iter().map(|line| (Op::Add, *line)));
    ops
}

/// Group the edit script into hunks: every change with `CONTEXT` lines of
/// company. Runs that would overlap become one hunk.
fn hunks(ops: &[(Op, &str)]) -> Vec<Hunk> {
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (op, _))| *op != Op::Keep)
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return Vec::new();
    }
    // Line numbers are assigned in one pass over the whole script, so a hunk
    // in the middle of a file still reports where it really is.
    let mut numbered: Vec<Line> = Vec::with_capacity(ops.len());
    let (mut old_no, mut new_no) = (1usize, 1usize);
    for (op, text) in ops {
        let number = match op {
            Op::Keep => {
                let n = new_no;
                old_no += 1;
                new_no += 1;
                n
            }
            Op::Add => {
                let n = new_no;
                new_no += 1;
                n
            }
            Op::Remove => {
                let n = old_no;
                old_no += 1;
                n
            }
        };
        numbered.push(Line {
            op: *op,
            number,
            text: (*text).to_string(),
        });
    }

    let mut spans: Vec<(usize, usize)> = Vec::new();
    for &i in &changed {
        let start = i.saturating_sub(CONTEXT);
        let end = (i + CONTEXT).min(ops.len() - 1);
        match spans.last_mut() {
            // `+ 1`: spans that merely touch still read as one hunk.
            Some(last) if start <= last.1 + 1 => last.1 = last.1.max(end),
            _ => spans.push((start, end)),
        }
    }
    spans
        .into_iter()
        .map(|(start, end)| Hunk {
            lines: numbered[start..=end].to_vec(),
        })
        .collect()
}

// ------------------------------------------------------------------- render

/// Draw the diff as terminal lines, without any leading indent — the caller
/// owns the margin. `width` is the room available for a whole line including
/// that margin; `max_lines` bounds the body, and what is cut is announced.
///
/// Pass `theme::plain()` for an escape-free rendering (the `/call` report).
pub fn render(
    theme: &Theme,
    diff: &FileDiff,
    indent: usize,
    width: usize,
    max_lines: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    if diff.hunks.is_empty() {
        return out;
    }
    // Widest number decides the gutter, so nothing jitters between hunks.
    let digits = diff
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .map(|l| l.number)
        .max()
        .unwrap_or(1)
        .to_string()
        .len()
        .max(2);
    // gutter = number + space + sign + space
    let text_room = width.saturating_sub(indent + digits + 3).max(8);

    let mut shown = 0usize;
    let mut elided = diff.total_lines();
    'hunks: for (h, hunk) in diff.hunks.iter().enumerate() {
        if h > 0 {
            out.push(format!("{}{:>digits$} ⋯{}", theme.dim, "", theme.reset()));
        }
        for line in &hunk.lines {
            if shown == max_lines {
                out.push(format!(
                    "{}{:>digits$}   … {elided} more line{}{}",
                    theme.dim,
                    "",
                    if elided == 1 { "" } else { "s" },
                    theme.reset()
                ));
                break 'hunks;
            }
            let (sign, sign_style, text_style) = match line.op {
                Op::Add => ('+', theme.diff_added_marker, theme.code_block),
                Op::Remove => ('-', theme.diff_removed_marker, theme.code_block),
                Op::Keep => (' ', theme.dim, theme.dim),
            };
            out.push(format!(
                "{}{:>digits$}{} {sign_style}{sign}{} {text_style}{}{}",
                theme.dim,
                line.number,
                theme.reset(),
                theme.reset(),
                clip(&line.text, text_room),
                theme.reset()
            ));
            shown += 1;
            elided -= 1;
        }
    }
    out
}

/// One line of source, made safe to print: tabs expanded, control characters
/// dropped, cut to the room available rather than wrapped.
fn clip(text: &str, room: usize) -> String {
    let mut flat = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\t' => {
                let pad = TAB - (flat.chars().count() % TAB);
                flat.extend(std::iter::repeat_n(' ', pad));
            }
            c if c.is_control() => {}
            c => flat.push(c),
        }
    }
    let flat = flat.trim_end();
    if flat.chars().count() <= room {
        flat.to_string()
    } else {
        flat.chars()
            .take(room.saturating_sub(1))
            .collect::<String>()
            + "…"
    }
}

/// The diff as plain text for the `/call` report: a header and the body, no
/// escapes, no width games beyond the report's own measure.
pub fn plain_block(diff: &FileDiff, width: usize) -> String {
    let mut out = String::new();
    for line in render(crate::theme::plain(), diff, 0, width, usize::MAX) {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops_of(diff: &FileDiff) -> Vec<(Op, usize, String)> {
        diff.hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .map(|l| (l.op, l.number, l.text.clone()))
            .collect()
    }

    #[test]
    fn a_one_line_change_is_one_hunk_with_context() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nb\nc\nD\ne\nf\ng\nh\n";
        let diff = compute("x.txt", old, new, false);
        assert_eq!((diff.added, diff.removed), (1, 1));
        assert_eq!(diff.stat(), "+1 −1");
        assert_eq!(diff.hunks.len(), 1);
        let lines = ops_of(&diff);
        // 3 context, the change, 3 context.
        assert_eq!(lines.len(), 8);
        assert_eq!(lines[3], (Op::Remove, 4, "d".into()));
        assert_eq!(lines[4], (Op::Add, 4, "D".into()));
        // Context after the change keeps counting in the new file.
        assert_eq!(lines[5], (Op::Keep, 5, "e".into()));
    }

    #[test]
    fn distant_changes_become_separate_hunks() {
        let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let new = old
            .replace("line 3\n", "line three\n")
            .replace("line 30\n", "line thirty\n");
        let diff = compute("x.txt", &old, &new, false);
        assert_eq!(diff.hunks.len(), 2, "far apart changes must not merge");
        assert_eq!((diff.added, diff.removed), (2, 2));
        // Nearby changes do merge.
        let new = old
            .replace("line 3\n", "line three\n")
            .replace("line 5\n", "line five\n");
        assert_eq!(compute("x.txt", &old, &new, false).hunks.len(), 1);
    }

    #[test]
    fn insertions_and_deletions_number_from_the_right_side() {
        let diff = compute("x.txt", "a\nb\n", "a\nb\nc\n", false);
        assert_eq!((diff.added, diff.removed), (1, 0));
        assert_eq!(diff.stat(), "+1");
        assert_eq!(ops_of(&diff).last().unwrap(), &(Op::Add, 3, "c".into()));

        let diff = compute("x.txt", "a\nb\nc\n", "a\nc\n", false);
        assert_eq!(diff.stat(), "−1");
        // The removed line is numbered where it was, in the old file.
        assert!(ops_of(&diff).contains(&(Op::Remove, 2, "b".into())));
    }

    #[test]
    fn a_new_file_is_all_additions() {
        let diff = compute("new.txt", "", "one\ntwo\n", true);
        assert!(diff.created);
        assert_eq!((diff.added, diff.removed), (2, 0));
        assert_eq!(ops_of(&diff).len(), 2);

        // And an unchanged write reports nothing at all.
        let diff = compute("x.txt", "same\n", "same\n", false);
        assert!(diff.is_empty());
        assert!(diff.hunks.is_empty());
        assert_eq!(diff.stat(), "");
    }

    #[test]
    fn a_rewrite_too_large_to_align_falls_back_to_a_coarse_block() {
        // Both sides differ on every line, so trimming saves nothing and the
        // cell budget decides.
        let old: String = (0..3000).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..3000).map(|i| format!("new {i}\n")).collect();
        let diff = compute("big.txt", &old, &new, false);
        assert!(diff.coarse, "3000×3000 cells must not be aligned");
        assert_eq!((diff.added, diff.removed), (3000, 3000));
        // Still one contiguous hunk: removals then additions.
        assert_eq!(diff.hunks.len(), 1);
    }

    #[test]
    fn rendering_is_gutter_then_sign_then_neutral_text() {
        let diff = compute("x.txt", "a\nb\n", "a\nB\n", false);
        let lines = render(crate::theme::plain(), &diff, 2, 80, 100);
        assert_eq!(lines, vec![" 1   a", " 2 - b", " 2 + B"]);
    }

    #[test]
    fn rendering_bounds_the_body_and_says_what_it_cut() {
        let old = String::new();
        let new: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let diff = compute("x.txt", &old, &new, true);
        let lines = render(crate::theme::plain(), &diff, 2, 80, 10);
        assert_eq!(lines.len(), 11);
        assert!(
            lines.last().unwrap().contains("… 20 more lines"),
            "{:?}",
            lines.last()
        );
    }

    #[test]
    fn long_lines_are_clipped_and_tabs_expanded() {
        assert_eq!(clip("\tx", 20), "    x");
        assert_eq!(clip("abcdefghij", 5), "abcd…");
        assert_eq!(clip("trailing   ", 20), "trailing");
        assert_eq!(clip("a\u{7}b", 20), "ab");
    }

    #[test]
    fn a_gap_between_hunks_is_one_marker() {
        let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let new = old
            .replace("line 3\n", "line three\n")
            .replace("line 30\n", "line thirty\n");
        let diff = compute("x.txt", &old, &new, false);
        let body = plain_block(&diff, 80);
        assert_eq!(body.matches('⋯').count(), 1, "{body}");
    }
}
