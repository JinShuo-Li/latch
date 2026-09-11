//! Real line-level diff statistics for the change ledger.
//!
//! Earlier versions compared line *sets*, which reported zero changes when a
//! block moved, and under-counted duplicate lines. This module computes the
//! minimal edit script length with the Myers greedy algorithm, so additions
//! and deletions match what a real diff would show.
//!
//! Only the LCS length is needed: for an optimal script,
//! `additions = after - lcs` and `deletions = before - lcs`. Common prefix and
//! suffix lines are trimmed first, which keeps the algorithm linear for the
//! common case of localized edits.

/// Lines above this combined middle size use a conservative fallback
/// (`all middle lines changed`) rather than spending unbounded time on a
/// pathological rewrite. This is a compute guard for line counting only, not a
/// product limit on files.
const MAX_MYERS_LINES: usize = 30_000;

/// Line delta for the kernel change ledger.
#[must_use]
pub fn line_delta(before: &[u8], after: &[u8]) -> (usize, usize) {
    let before_lines = split_lines(before);
    let after_lines = split_lines(after);
    let (additions, deletions) = delta_counts(&before_lines, &after_lines);
    (additions, deletions)
}

fn split_lines(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect()
}

fn delta_counts(before: &[String], after: &[String]) -> (usize, usize) {
    // Trim the common prefix and suffix so localized edits stay cheap.
    let mut prefix = 0usize;
    while prefix < before.len() && prefix < after.len() && before[prefix] == after[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < before.len().saturating_sub(prefix)
        && suffix < after.len().saturating_sub(prefix)
        && before[before.len() - 1 - suffix] == after[after.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let before_mid = &before[prefix..before.len() - suffix];
    let after_mid = &after[prefix..after.len() - suffix];
    if before_mid.is_empty() {
        return (after_mid.len(), 0);
    }
    if after_mid.is_empty() {
        return (0, before_mid.len());
    }
    if before_mid.len() + after_mid.len() > MAX_MYERS_LINES {
        // Conservative but honest: a rewrite this large reports the whole
        // middle as changed instead of silently under-counting.
        return (after_mid.len(), before_mid.len());
    }
    let lcs = lcs_length(before_mid, after_mid);
    (
        after_mid.len().saturating_sub(lcs),
        before_mid.len().saturating_sub(lcs),
    )
}

/// Length of the longest common subsequence via the Myers greedy algorithm.
fn lcs_length(a: &[String], b: &[String]) -> usize {
    let n = a.len();
    let m = b.len();
    let max = n + m;
    let offset = max as isize;
    let mut v = vec![0isize; 2 * max + 1];
    for d in 0..=max as isize {
        let mut k = -d;
        while k <= d {
            let index = (offset + k) as usize;
            let mut x = if k == -d || (k != d && v[index - 1] < v[index + 1]) {
                v[index + 1]
            } else {
                v[index - 1] + 1
            };
            let mut y = x - k;
            while x < n as isize && y < m as isize && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[index] = x;
            if x >= n as isize && y >= m as isize {
                return ((n + m) - d as usize) / 2;
            }
            k += 2;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(before: &str, after: &str) -> (usize, usize) {
        line_delta(before.as_bytes(), after.as_bytes())
    }

    #[test]
    fn simple_line_changes_are_counted_minimally() {
        assert_eq!(delta("a\nb\nc\n", "a\nc\n"), (0, 1));
        assert_eq!(delta("a\nc\n", "a\nb\nc\n"), (1, 0));
        assert_eq!(delta("a\nb\n", "a\nc\n"), (1, 1));
        assert_eq!(delta("x\nx\nx\n", "x\nx\n"), (0, 1));
        assert_eq!(delta("", "a\nb\n"), (2, 0));
        assert_eq!(delta("a\nb\n", ""), (0, 2));
        assert_eq!(delta("", ""), (0, 0));
    }

    #[test]
    fn duplicate_lines_do_not_confuse_the_count() {
        // Set differences reported (0, 0) here because both sets were {"x"}.
        assert_eq!(delta("x\nx\nx\n", "x\nx\nx\nx\n"), (1, 0));
        assert_eq!(delta("a\nb\na\nb\n", "a\nb\n"), (0, 2));
    }

    #[test]
    fn moved_blocks_count_as_delete_plus_add() {
        // Set differences reported (0, 0): every line still exists.
        assert_eq!(delta("a\nb\nc\nd\n", "b\nc\nd\na\n"), (1, 1));
    }

    #[test]
    fn crlf_and_trailing_newline_do_not_fake_changes() {
        assert_eq!(delta("a\r\nb\r\n", "a\nb\n"), (0, 0));
    }

    #[test]
    fn large_rewrites_stay_honest_and_bounded() {
        let before = (0..40_000).map(|i| format!("{i}\n")).collect::<String>();
        let after = (40_000..80_000)
            .map(|i| format!("{i}\n"))
            .collect::<String>();
        let (additions, deletions) = delta(&before, &after);
        assert!(additions > 0 && deletions > 0);
        assert!(additions + deletions <= 80_000);
    }

    #[test]
    fn localized_edit_in_large_file_is_cheap_and_correct() {
        let mut before = String::new();
        for i in 0..50_000 {
            before.push_str(&format!("line {i}\n"));
        }
        let after = before.replace("line 25\n", "line twenty-five\n");
        assert_eq!(delta(&before, &after), (1, 1));
    }
}
