//! A unified diff of two texts, by line.
//!
//! What a file tool changed is reported to programs as a diff
//! ([`crate::events::file_change`]); this is where it is computed, once, for
//! whichever interface asks. Lines are compared whole, terminator included,
//! so a change that only adds the final newline shows up as one (with the
//! usual `\ No newline at end of file` marker).
//!
//! The common head and tail are set aside first — an edit usually touches a
//! few lines of a long file — and the middle goes through Myers' algorithm,
//! which is quick when the two sides are close. When they are not (more than
//! [`MAX_EDITS`] lines apart) the middle is reported as replaced outright:
//! still a correct diff, just not the shortest.

/// Lines of unchanged text shown around each change.
const CONTEXT: usize = 3;

/// The longest edit script searched for. Myers' algorithm keeps a row per
/// step, so its memory grows with the square of this.
const MAX_EDITS: usize = 1000;

/// One line of an edit script.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Keep,
    Delete,
    Insert,
}

/// A diff, rendered.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Unified {
    /// The hunks, each opened by its `@@` line. Empty when the texts are
    /// the same. No file header: the caller knows the names.
    pub hunks: String,
    /// Lines added and removed.
    pub added: usize,
    pub removed: usize,
}

/// The unified diff that turns `old` into `new`.
pub fn unified(old: &str, new: &str) -> Unified {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let ops = script(&a, &b);
    render(&a, &b, &ops)
}

/// The edit script from `a` to `b`: one [`Op`] per line of either.
fn script(a: &[&str], b: &[&str]) -> Vec<Op> {
    let head = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let (a_rest, b_rest) = (&a[head..], &b[head..]);
    let tail = a_rest
        .iter()
        .rev()
        .zip(b_rest.iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (a_mid, b_mid) = (
        &a_rest[..a_rest.len() - tail],
        &b_rest[..b_rest.len() - tail],
    );
    let mut ops = vec![Op::Keep; head];
    match myers(a_mid, b_mid) {
        Some(mid) => ops.extend(mid),
        None => {
            ops.extend(std::iter::repeat_n(Op::Delete, a_mid.len()));
            ops.extend(std::iter::repeat_n(Op::Insert, b_mid.len()));
        }
    }
    ops.extend(std::iter::repeat_n(Op::Keep, tail));
    ops
}

/// Myers' shortest edit script, or `None` when it is longer than
/// [`MAX_EDITS`].
fn myers(a: &[&str], b: &[&str]) -> Option<Vec<Op>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (a.len() + b.len()).min(MAX_EDITS) as isize;
    // `v[k + offset]`: the furthest x reached on diagonal k (x - y = k).
    let offset = max + 1;
    let mut v = vec![0isize; (2 * max + 3) as usize];
    // The part of `v` each round started from (diagonals -d-1..=d+1), to
    // walk the path back.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    for d in 0..=max {
        let window = (offset - d - 1) as usize..=(offset + d + 1) as usize;
        trace.push(v[window].to_vec());
        let mut k = -d;
        while k <= d {
            let down =
                k == -d || (k != d && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize]);
            let mut x = if down {
                v[(k + 1 + offset) as usize]
            } else {
                v[(k - 1 + offset) as usize] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[(k + offset) as usize] = x;
            if x >= n && y >= m {
                return Some(backtrack(&trace, n, m));
            }
            k += 2;
        }
    }
    None
}

/// Walk the rounds of [`myers`] back from the end to the start.
fn backtrack(trace: &[Vec<isize>], n: isize, m: isize) -> Vec<Op> {
    let mut ops = Vec::new();
    let (mut x, mut y) = (n, m);
    for (d, row) in trace.iter().enumerate().rev() {
        let d = d as isize;
        // `row` holds diagonals -d-1..=d+1.
        let at = |k: isize| row[(k + d + 1) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Keep);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            ops.push(if x == prev_x { Op::Insert } else { Op::Delete });
        }
        (x, y) = (prev_x, prev_y);
    }
    ops.reverse();
    ops
}

/// The hunks of an edit script, with [`CONTEXT`] lines around each change.
fn render(a: &[&str], b: &[&str], ops: &[Op]) -> Unified {
    let mut out = Unified::default();
    // Where each op stands in the two texts.
    let mut at = Vec::with_capacity(ops.len());
    let (mut i, mut j) = (0, 0);
    for op in ops {
        at.push((i, j));
        match op {
            Op::Keep => (i, j) = (i + 1, j + 1),
            Op::Delete => i += 1,
            Op::Insert => j += 1,
        }
    }
    let mut next = 0;
    while let Some(first) = (next..ops.len()).find(|&p| ops[p] != Op::Keep) {
        // The hunk runs to the last change that is followed by more
        // unchanged lines than two contexts hold (or by the end).
        let mut last = first;
        let mut p = first + 1;
        while p < ops.len() && p - last <= 2 * CONTEXT + 1 {
            if ops[p] != Op::Keep {
                last = p;
            }
            p += 1;
        }
        let start = first.saturating_sub(CONTEXT);
        let end = (last + 1 + CONTEXT).min(ops.len());
        let (old_start, new_start) = at[start];
        let old_len = ops[start..end].iter().filter(|o| **o != Op::Insert).count();
        let new_len = ops[start..end].iter().filter(|o| **o != Op::Delete).count();
        out.hunks.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(old_start, old_len),
            range(new_start, new_len)
        ));
        for p in start..end {
            let (i, j) = at[p];
            let (mark, line) = match ops[p] {
                Op::Keep => (' ', a[i]),
                Op::Delete => {
                    out.removed += 1;
                    ('-', a[i])
                }
                Op::Insert => {
                    out.added += 1;
                    ('+', b[j])
                }
            };
            out.hunks.push(mark);
            out.hunks.push_str(line);
            if !line.ends_with('\n') {
                out.hunks.push_str("\n\\ No newline at end of file\n");
            }
        }
        next = end;
    }
    out
}

/// One side of a hunk header: `start,len`, 1-based, the way `diff -u`
/// writes it (an empty range names the line before it).
fn range(start: usize, len: usize) -> String {
    match len {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{len}", start + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apply a rendered diff to `old`, checking its context as it goes.
    fn apply(old: &str, hunks: &str) -> String {
        let a: Vec<&str> = old.split_inclusive('\n').collect();
        let mut out = String::new();
        let mut i = 0;
        let mut lines = hunks.split_inclusive('\n').peekable();
        while let Some(line) = lines.next() {
            if let Some(header) = line.strip_prefix("@@ -") {
                let start: usize = header.split([',', ' ']).next().unwrap().parse().unwrap();
                let len_zero = header.split(' ').next().unwrap().ends_with(",0");
                let start = if len_zero { start } else { start - 1 };
                while i < start {
                    out.push_str(a[i]);
                    i += 1;
                }
                continue;
            }
            let (mark, mut text) = (line.as_bytes()[0], line[1..].to_string());
            if lines.peek().is_some_and(|l| l.starts_with('\\')) {
                lines.next();
                text.pop();
            }
            match mark {
                b' ' => {
                    assert_eq!(a[i], text);
                    out.push_str(a[i]);
                    i += 1;
                }
                b'-' => {
                    assert_eq!(a[i], text);
                    i += 1;
                }
                b'+' => out.push_str(&text),
                _ => panic!("unexpected line {line:?}"),
            }
        }
        out.extend(a[i..].iter().copied());
        out
    }

    #[test]
    fn a_one_line_change_is_one_hunk_with_context() {
        let old = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n";
        let new = old.replace("five", "5");
        let diff = unified(old, &new);
        assert_eq!(
            diff.hunks,
            "@@ -2,7 +2,7 @@\n two\n three\n four\n-five\n+5\n six\n seven\n eight\n"
        );
        assert_eq!((diff.added, diff.removed), (1, 1));
    }

    #[test]
    fn identical_texts_have_no_hunks() {
        assert_eq!(unified("a\nb\n", "a\nb\n"), Unified::default());
        assert_eq!(unified("", ""), Unified::default());
    }

    #[test]
    fn a_new_file_and_a_removed_one() {
        let diff = unified("", "a\nb\n");
        assert_eq!(diff.hunks, "@@ -0,0 +1,2 @@\n+a\n+b\n");
        assert_eq!((diff.added, diff.removed), (2, 0));
        let diff = unified("a\nb\n", "");
        assert_eq!(diff.hunks, "@@ -1,2 +0,0 @@\n-a\n-b\n");
        assert_eq!((diff.added, diff.removed), (0, 2));
    }

    #[test]
    fn a_missing_final_newline_is_marked() {
        let diff = unified("a\nb", "a\nb\n");
        assert_eq!(
            diff.hunks,
            "@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b\n"
        );
    }

    #[test]
    fn distant_changes_are_separate_hunks() {
        let old: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let new = old.replace("line 3\n", "LINE 3\n").replace("line 25\n", "");
        let diff = unified(&old, &new);
        assert_eq!(diff.hunks.matches("@@ -").count(), 2);
        assert_eq!((diff.added, diff.removed), (1, 2));
        assert_eq!(apply(&old, &diff.hunks), new);
    }

    #[test]
    fn diffs_apply_back_to_the_new_text() {
        let cases = [
            ("a\nb\nc\n", "a\nc\n"),
            ("a\nb\nc\n", "x\na\nb\nc\ny\n"),
            ("a\nb\nc\nd\ne\nf\ng\nh\n", "a\nB\nc\nd\ne\nf\nG\nh\ni\n"),
            ("same\n", "same\nmore"),
            ("x\ny\nz", "z\ny\nx"),
            ("", "only"),
            ("a\na\na\nb\na\na\n", "a\na\nb\na\na\na\n"),
        ];
        for (old, new) in cases {
            let diff = unified(old, new);
            assert_eq!(apply(old, &diff.hunks), new, "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn texts_too_far_apart_are_replaced_outright() {
        let old: String = (0..MAX_EDITS + 10).map(|n| format!("old {n}\n")).collect();
        let new: String = (0..MAX_EDITS + 10).map(|n| format!("new {n}\n")).collect();
        let diff = unified(&format!("head\n{old}tail\n"), &format!("head\n{new}tail\n"));
        assert_eq!(diff.added, MAX_EDITS + 10);
        assert_eq!(diff.removed, MAX_EDITS + 10);
        assert_eq!(diff.hunks.matches("@@ -").count(), 1);
    }
}
