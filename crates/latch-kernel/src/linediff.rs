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

/// Lines of unchanged context shown around each preview hunk.
const PREVIEW_CONTEXT: usize = 3;

/// Cell budget for exact middle diffing. Localized edits trim to a small
/// middle; larger rewrites fall back to an honest coarse script.
const MAX_EXACT_DIFF_CELLS: usize = 2_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffKind {
    Context,
    Delete,
    Insert,
}

#[derive(Debug)]
struct DiffEntry {
    kind: DiffKind,
    text: String,
}

/// Renders a real unified diff for one path from the actual before/after
/// bytes. This is the source of truth for inline edit previews; callers bound
/// the output and never reconstruct changed lines from counters. `before` is
/// `None` for a created file and `after` is `None` for a deletion.
#[must_use]
pub fn unified_diff(
    path: &str,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
    max_lines: usize,
) -> String {
    if max_lines == 0 || (before.is_none() && after.is_none()) {
        return String::new();
    }
    let before_lines = before.map(split_lines).unwrap_or_default();
    let after_lines = after.map(split_lines).unwrap_or_default();
    let entries = diff_entries(&before_lines, &after_lines);
    if entries.iter().all(|entry| entry.kind == DiffKind::Context) {
        return String::new();
    }

    let has_before = before.is_some();
    let has_after = after.is_some();
    let quoted = quote_path(path);
    let mut out = String::new();
    out.push_str(&format!("diff --git a/{quoted} b/{quoted}\n"));
    out.push_str(&format!(
        "--- {}\n",
        if has_before {
            format!("a/{quoted}")
        } else {
            "/dev/null".to_owned()
        }
    ));
    out.push_str(&format!(
        "+++ {}\n",
        if has_after {
            format!("b/{quoted}")
        } else {
            "/dev/null".to_owned()
        }
    ));

    let numbers = line_numbers(&entries);
    let ranges = change_ranges(&entries);
    let mut emitted = 0usize;
    for (start, end) in ranges {
        let from = start.saturating_sub(PREVIEW_CONTEXT);
        let to = (end + PREVIEW_CONTEXT).min(entries.len() - 1);
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        for entry in &entries[from..=to] {
            match entry.kind {
                DiffKind::Context => {
                    old_count += 1;
                    new_count += 1;
                }
                DiffKind::Delete => old_count += 1,
                DiffKind::Insert => new_count += 1,
            }
        }
        let hunk_lines = to - from + 1;
        let take = hunk_lines.min(max_lines.saturating_sub(emitted + 1));
        if take == 0 {
            break;
        }
        let old_start = if old_count == 0 { 0 } else { numbers[from].0 };
        let new_start = if new_count == 0 { 0 } else { numbers[from].1 };
        out.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));
        // A rewrite too large for the budget is sampled so both directions are
        // visible instead of a wall of one color; the omission note and `/diff`
        // carry the rest.
        let shown: Vec<&DiffEntry> = if hunk_lines <= take {
            entries[from..=to].iter().collect()
        } else {
            balanced_sample(&entries[from..=to], take)
        };
        for entry in shown {
            let prefix = match entry.kind {
                DiffKind::Context => ' ',
                DiffKind::Delete => '-',
                DiffKind::Insert => '+',
            };
            out.push(prefix);
            out.push_str(&entry.text);
            out.push('\n');
        }
        emitted += take + 1;
        if take < hunk_lines {
            break;
        }
    }
    out
}

fn balanced_sample(entries: &[DiffEntry], take: usize) -> Vec<&DiffEntry> {
    let deletions = take / 2;
    let insertions = take - deletions;
    let mut out: Vec<&DiffEntry> = entries
        .iter()
        .filter(|entry| entry.kind == DiffKind::Delete)
        .take(deletions)
        .collect();
    out.extend(
        entries
            .iter()
            .filter(|entry| entry.kind == DiffKind::Insert)
            .take(insertions),
    );
    out
}

fn quote_path(path: &str) -> String {
    if path.chars().any(|ch| ch.is_whitespace() || ch == '"') {
        format!("\"{path}\"")
    } else {
        path.to_owned()
    }
}

fn diff_entries(before: &[String], after: &[String]) -> Vec<DiffEntry> {
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
    let mut entries = Vec::new();
    for text in &before[..prefix] {
        entries.push(DiffEntry {
            kind: DiffKind::Context,
            text: text.clone(),
        });
    }
    if before_mid.len().saturating_mul(after_mid.len()) <= MAX_EXACT_DIFF_CELLS {
        entries.extend(lcs_entries(before_mid, after_mid));
    } else {
        entries.extend(before_mid.iter().map(|text| DiffEntry {
            kind: DiffKind::Delete,
            text: text.clone(),
        }));
        entries.extend(after_mid.iter().map(|text| DiffEntry {
            kind: DiffKind::Insert,
            text: text.clone(),
        }));
    }
    for text in &before[before.len() - suffix..] {
        entries.push(DiffEntry {
            kind: DiffKind::Context,
            text: text.clone(),
        });
    }
    entries
}

/// Backtracking LCS over a bounded middle.
fn lcs_entries(a: &[String], b: &[String]) -> Vec<DiffEntry> {
    let n = a.len();
    let m = b.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut entries = Vec::with_capacity(n.max(m));
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            entries.push(DiffEntry {
                kind: DiffKind::Context,
                text: a[i].clone(),
            });
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            entries.push(DiffEntry {
                kind: DiffKind::Delete,
                text: a[i].clone(),
            });
            i += 1;
        } else {
            entries.push(DiffEntry {
                kind: DiffKind::Insert,
                text: b[j].clone(),
            });
            j += 1;
        }
    }
    while i < n {
        entries.push(DiffEntry {
            kind: DiffKind::Delete,
            text: a[i].clone(),
        });
        i += 1;
    }
    while j < m {
        entries.push(DiffEntry {
            kind: DiffKind::Insert,
            text: b[j].clone(),
        });
        j += 1;
    }
    entries
}

fn line_numbers(entries: &[DiffEntry]) -> Vec<(usize, usize)> {
    let mut numbers = Vec::with_capacity(entries.len());
    let mut old_no = 1usize;
    let mut new_no = 1usize;
    for entry in entries {
        numbers.push((old_no, new_no));
        match entry.kind {
            DiffKind::Context => {
                old_no += 1;
                new_no += 1;
            }
            DiffKind::Delete => old_no += 1,
            DiffKind::Insert => new_no += 1,
        }
    }
    numbers
}

/// Groups changed entries into hunks, merging runs separated by at most twice
/// the context so nearby edits share one header.
fn change_ranges(entries: &[DiffEntry]) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.kind == DiffKind::Context {
            continue;
        }
        match ranges.last_mut() {
            Some((_, end)) if index <= *end + 2 * PREVIEW_CONTEXT + 1 => *end = index,
            _ => ranges.push((index, index)),
        }
    }
    ranges
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
    fn unified_diff_shows_real_added_and_removed_lines() {
        let diff = unified_diff(
            "src/foo.rs",
            Some(b"keep\nold_value = calc();\ntail\n"),
            Some(b"keep\nnew_value = calc();\ntail\n"),
            100,
        );
        assert!(
            diff.contains("diff --git a/src/foo.rs b/src/foo.rs"),
            "{diff}"
        );
        assert!(diff.contains("--- a/src/foo.rs"), "{diff}");
        assert!(diff.contains("+++ b/src/foo.rs"), "{diff}");
        assert!(diff.contains("-old_value = calc();"), "{diff}");
        assert!(diff.contains("+new_value = calc();"), "{diff}");
        assert!(diff.contains(" keep"), "context survives: {diff}");
        assert!(diff.contains("@@ -1,3 +1,3 @@"), "{diff}");
    }

    #[test]
    fn unified_diff_covers_creation_deletion_and_repeated_lines() {
        let created = unified_diff("src/new.rs", None, Some(b"one\ntwo\n"), 100);
        assert!(created.contains("--- /dev/null"), "{created}");
        assert!(created.contains("+++ b/src/new.rs"), "{created}");
        assert!(created.contains("+one"), "{created}");
        assert!(!created.contains("\n-one"), "{created}");

        let deleted = unified_diff("src/gone.rs", Some(b"one\n"), None, 100);
        assert!(deleted.contains("+++ /dev/null"), "{deleted}");
        assert!(deleted.contains("-one"), "{deleted}");

        // Repeated identical lines delete exactly one line, not a set collapse.
        let repeated = unified_diff("r.txt", Some(b"x\nx\nx\n"), Some(b"x\nx\n"), 100);
        assert_eq!(repeated.matches("\n-x").count(), 1, "{repeated}");
    }

    #[test]
    fn unified_diff_preserves_unicode_and_bounds_large_rewrites() {
        let diff = unified_diff(
            "cjk.txt",
            Some("老\n值\n".as_bytes()),
            Some("新\n值\n".as_bytes()),
            100,
        );
        assert!(diff.contains("-老"), "{diff}");
        assert!(diff.contains("+新"), "{diff}");

        let before = (0..500).map(|i| format!("old {i}\n")).collect::<String>();
        let after = (0..500).map(|i| format!("new {i}\n")).collect::<String>();
        let bounded = unified_diff(
            "big.txt",
            Some(before.as_bytes()),
            Some(after.as_bytes()),
            12,
        );
        assert!(bounded.contains("-old 0"), "{bounded}");
        assert!(bounded.contains("+new 0"), "{bounded}");
        assert!(
            bounded.lines().count() <= 15,
            "bounded body plus headers: {bounded}"
        );
    }

    #[test]
    fn unified_diff_is_empty_when_nothing_changed() {
        assert_eq!(
            unified_diff("same.txt", Some(b"a\n"), Some(b"a\n"), 100),
            ""
        );
        assert_eq!(unified_diff("none.txt", None, None, 100), "");
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
