use similar::{Algorithm, DiffOp, capture_diff_slices};

#[derive(Clone, Debug)]
struct Hunk {
    start: usize,
    end: usize,
    lines: Vec<String>,
}

fn hunks(base: &[&str], other: &[&str]) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    for op in capture_diff_slices(Algorithm::Myers, base, other) {
        let (start, end, new_start, new_end) = match op {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => (old_index, old_index + old_len, new_index, new_index),
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => (old_index, old_index, new_index, new_index + new_len),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => (
                old_index,
                old_index + old_len,
                new_index,
                new_index + new_len,
            ),
        };
        let lines: Vec<String> = other[new_start..new_end]
            .iter()
            .map(|line| line.to_string())
            .collect();
        match out.last_mut() {
            Some(last) if last.end == start => {
                last.end = end;
                last.lines.extend(lines);
            }
            _ => out.push(Hunk { start, end, lines }),
        }
    }
    out
}

fn overlaps(a: &Hunk, b: &Hunk) -> bool {
    a.start == b.start || (a.start < b.end && b.start < a.end)
}

fn render(base: &[&str], from: usize, to: usize, hunks: &[Hunk]) -> String {
    let mut out = String::new();
    let mut cursor = from;
    for hunk in hunks {
        for line in &base[cursor..hunk.start] {
            out.push_str(line);
        }
        for line in &hunk.lines {
            out.push_str(line);
        }
        cursor = hunk.end;
    }
    for line in &base[cursor..to] {
        out.push_str(line);
    }
    out
}

fn ensure_newline(text: &mut String) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
}

pub struct Merged {
    pub text: String,
    pub conflicts: usize,
}

pub fn merge_lines(
    base: &str,
    ours: &str,
    theirs: &str,
    ours_name: &str,
    theirs_name: &str,
) -> Merged {
    if ours == theirs {
        return Merged {
            text: ours.to_string(),
            conflicts: 0,
        };
    }
    if ours == base {
        return Merged {
            text: theirs.to_string(),
            conflicts: 0,
        };
    }
    if theirs == base {
        return Merged {
            text: ours.to_string(),
            conflicts: 0,
        };
    }
    let base_lines: Vec<&str> = base.split_inclusive('\n').collect();
    let ours_lines: Vec<&str> = ours.split_inclusive('\n').collect();
    let theirs_lines: Vec<&str> = theirs.split_inclusive('\n').collect();
    let a = hunks(&base_lines, &ours_lines);
    let b = hunks(&base_lines, &theirs_lines);
    let mut out = String::new();
    let mut conflicts = 0;
    let mut cursor = 0;
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        let take_a = match (a.get(i), b.get(j)) {
            (Some(x), Some(y)) => (x.start, x.end) <= (y.start, y.end),
            (Some(_), None) => true,
            _ => false,
        };
        let first = if take_a { &a[i] } else { &b[j] };
        let other = if take_a { b.get(j) } else { a.get(i) };
        let clash = other.is_some_and(|other| overlaps(first, other));
        if !clash {
            for line in &base_lines[cursor..first.start] {
                out.push_str(line);
            }
            for line in &first.lines {
                out.push_str(line);
            }
            cursor = first.end;
            if take_a {
                i += 1;
            } else {
                j += 1;
            }
            continue;
        }
        let mut lo = first.start.min(other.map_or(usize::MAX, |h| h.start));
        let mut hi = first.end.max(other.map_or(0, |h| h.end));
        let (mut ei, mut ej) = (i, j);
        loop {
            let mut grew = false;
            while ei < a.len() && (a[ei].start < hi || a[ei].start == lo) {
                hi = hi.max(a[ei].end);
                lo = lo.min(a[ei].start);
                ei += 1;
                grew = true;
            }
            while ej < b.len() && (b[ej].start < hi || b[ej].start == lo) {
                hi = hi.max(b[ej].end);
                lo = lo.min(b[ej].start);
                ej += 1;
                grew = true;
            }
            if !grew {
                break;
            }
        }
        for line in &base_lines[cursor..lo] {
            out.push_str(line);
        }
        let mine = render(&base_lines, lo, hi, &a[i..ei]);
        let theirs_text = render(&base_lines, lo, hi, &b[j..ej]);
        if mine == theirs_text {
            out.push_str(&mine);
        } else {
            conflicts += 1;
            ensure_newline(&mut out);
            out.push_str(&format!("<<<<<<< {ours_name}\n"));
            let mut mine = mine;
            ensure_newline(&mut mine);
            out.push_str(&mine);
            out.push_str("=======\n");
            let mut theirs_text = theirs_text;
            ensure_newline(&mut theirs_text);
            out.push_str(&theirs_text);
            out.push_str(&format!(">>>>>>> {theirs_name}\n"));
        }
        cursor = hi;
        i = ei;
        j = ej;
    }
    for line in &base_lines[cursor..] {
        out.push_str(line);
    }
    Merged {
        text: out,
        conflicts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disjoint_edits_merge() {
        let base = "a\nb\nc\nd\ne\n";
        let ours = "A\nb\nc\nd\ne\n";
        let theirs = "a\nb\nc\nd\nE\n";
        let merged = merge_lines(base, ours, theirs, "ours", "theirs");
        assert_eq!(merged.text, "A\nb\nc\nd\nE\n");
        assert_eq!(merged.conflicts, 0);
    }

    #[test]
    fn same_line_conflicts() {
        let merged = merge_lines("a\nb\n", "a\nX\n", "a\nY\n", "ours", "theirs");
        assert_eq!(merged.conflicts, 1);
        assert!(
            merged
                .text
                .contains("<<<<<<< ours\nX\n=======\nY\n>>>>>>> theirs\n")
        );
    }

    #[test]
    fn overlapping_edits_conflict_unless_equal() {
        let merged = merge_lines("a\nb\n", "a\nX\n", "a\nX\nc\n", "ours", "theirs");
        assert_eq!(merged.conflicts, 1);
        let merged = merge_lines("a\nb\nc\n", "a\nX\nc\n", "a\nX\nc\n", "ours", "theirs");
        assert_eq!(merged.conflicts, 0);
    }
}
