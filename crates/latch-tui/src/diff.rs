//! Terminal rendering for shared parsed diffs.
pub use latch_ui::diff::*;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

fn addition_style() -> Style {
    crate::theme::palette().diff_add()
}
fn deletion_style() -> Style {
    crate::theme::palette().diff_del()
}
fn hunk_style() -> Style {
    crate::theme::palette().diff_hunk()
}
fn meta_style() -> Style {
    crate::theme::palette().diff_meta()
}
fn header_style() -> Style {
    crate::theme::palette().diff_file()
}

/// Full semantic rendering of a parsed document. Unparsed input is rendered as
/// plain, uncolored lines.
#[must_use]
pub fn diff_lines(document: &DiffDocument) -> Vec<Line<'static>> {
    if !document.parsed {
        return raw_diff_lines(document);
    }
    let mut out = Vec::new();
    for (index, file) in document.files.iter().enumerate() {
        if index > 0 {
            out.push(Line::from(""));
        }
        out.push(Line::from(Span::styled(
            format!("{} {}", file.kind(), file.display_path()),
            header_style(),
        )));
        for meta in &file.meta {
            out.push(Line::styled(meta.clone(), meta_style()));
        }
        for hunk in &file.hunks {
            out.push(Line::styled(hunk.header.clone(), hunk_style()));
            for line in &hunk.lines {
                out.push(render_line(line));
            }
        }
    }
    out
}

/// Hunk bodies only, without per-file headers or metadata. Used for compact
/// inline edit previews where the path is already shown by the transcript.
#[must_use]
pub fn diff_body_lines(document: &DiffDocument) -> Vec<Line<'static>> {
    if !document.parsed {
        return raw_diff_lines(document);
    }
    let mut out = Vec::new();
    for file in &document.files {
        for hunk in &file.hunks {
            out.push(Line::styled(hunk.header.clone(), hunk_style()));
            for line in &hunk.lines {
                out.push(render_line(line));
            }
        }
    }
    out
}

/// Raw, uncolored rendering used by the inspector's raw toggle and by parse
/// fallbacks. This is what must remain copyable.
#[must_use]
pub fn raw_diff_lines(document: &DiffDocument) -> Vec<Line<'static>> {
    document
        .raw
        .lines()
        .map(|line| Line::styled(line.to_owned(), Style::default()))
        .collect()
}

/// Bounded rendering for transcript cells. The full raw diff always remains in
/// the document; the overlay and `/raw` show it.
#[must_use]
pub fn diff_lines_bounded(document: &DiffDocument, max_lines: usize) -> Vec<Line<'static>> {
    let full = diff_lines(document);
    if full.len() <= max_lines {
        return full;
    }
    let mut out = full.into_iter().take(max_lines).collect::<Vec<_>>();
    out.push(Line::styled(
        format!(
            "… {} more diff lines · /diff for the full diff",
            document.raw.lines().count().saturating_sub(max_lines)
        ),
        meta_style(),
    ));
    out
}

fn render_line(line: &DiffLine) -> Line<'static> {
    let (prefix, style) = match line.kind {
        DiffLineKind::Addition => ("+", addition_style()),
        DiffLineKind::Deletion => ("-", deletion_style()),
        DiffLineKind::Context => (" ", crate::theme::palette().diff_context()),
        DiffLineKind::Hunk => ("", hunk_style()),
        DiffLineKind::Header => ("", header_style()),
        DiffLineKind::Meta => ("", meta_style()),
        DiffLineKind::NoNewline => ("", meta_style().add_modifier(Modifier::ITALIC)),
        DiffLineKind::Other => ("", Style::default()),
    };
    Line::from(vec![
        Span::styled(prefix.to_owned(), style),
        Span::styled(line.text.clone(), style),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    const SAMPLE: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -18,7 +18,7 @@ fn x() {
 fn average(values: &[i32]) -> Option<f64> {
-    Some((sum / values.len() as i32) as f64)
+    Some(sum as f64 / values.len() as f64)
 }
";

    fn style_fg(line: &Line<'_>, needle: &str) -> Option<Color> {
        line.spans
            .iter()
            .find(|span| span.content.contains(needle))
            .and_then(|span| span.style.fg)
            .or(line.style.fg)
    }

    #[test]
    fn parses_hunks_and_classifies_lines() {
        let document = parse_unified_diff(SAMPLE);
        assert!(document.parsed);
        assert_eq!(document.files.len(), 1);
        let file = &document.files[0];
        assert_eq!(file.display_path(), "src/lib.rs");
        assert_eq!(file.kind(), 'M');
        assert_eq!(file.added_lines(), 1);
        assert_eq!(file.removed_lines(), 1);
        let kinds: Vec<DiffLineKind> = file.hunks[0].lines.iter().map(|line| line.kind).collect();
        assert!(kinds.contains(&DiffLineKind::Addition));
        assert!(kinds.contains(&DiffLineKind::Deletion));
        assert!(kinds.contains(&DiffLineKind::Context));
    }

    #[test]
    fn additions_are_green_deletions_red_context_neutral() {
        let lines = diff_lines(&parse_unified_diff(SAMPLE));
        let addition = lines
            .iter()
            .find(|line| line.spans.iter().any(|span| span.content == "+"))
            .expect("addition prefix");
        assert_eq!(style_fg(addition, "+"), Some(Color::Green));
        let deletion = lines
            .iter()
            .find(|line| line.spans.iter().any(|span| span.content == "-"))
            .expect("deletion prefix");
        assert_eq!(style_fg(deletion, "-"), Some(Color::Red));
        let context = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains("fn average"))
            })
            .expect("context line");
        assert_eq!(style_fg(context, "fn average"), None, "context is default");
        let hunk = lines
            .iter()
            .find(|line| line.spans.iter().any(|span| span.content.starts_with("@@")))
            .expect("hunk header");
        assert_eq!(style_fg(hunk, "@@"), Some(Color::Cyan));
    }

    #[test]
    fn file_headers_are_never_source_additions_or_deletions() {
        let document = parse_unified_diff(SAMPLE);
        for file in &document.files {
            assert!(file.hunks[0].lines.iter().all(|line| {
                !line.text.starts_with("-- a/") && !line.text.starts_with("++ b/")
            }));
        }
        assert_eq!(document.added_lines(), 1, "only the real addition counts");
        assert_eq!(document.removed_lines(), 1);
    }

    #[test]
    fn metadata_is_dim_and_hunk_headers_are_cyan() {
        let lines = diff_lines(&parse_unified_diff(SAMPLE));
        let meta = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.starts_with("index "))
            })
            .expect("meta line");
        assert_eq!(style_fg(meta, "index"), None, "meta keeps the default fg");
        assert!(
            meta.style.add_modifier.contains(Modifier::DIM),
            "meta is dim"
        );
    }

    #[test]
    fn changed_lines_carry_restrained_background_tints() {
        use crate::theme::{ColorLevel, Palette, ThemeKind};
        let palette = Palette::new(ThemeKind::Dark, ColorLevel::TrueColor);
        assert_eq!(palette.diff_add().bg, Some(Color::Rgb(22, 46, 30)));
        assert_eq!(palette.diff_del().bg, Some(Color::Rgb(54, 28, 26)));
        // ANSI-16 terminals drop the tint instead of guessing.
        let basic = Palette::new(ThemeKind::Dark, ColorLevel::Ansi16);
        assert_eq!(basic.diff_add().bg, None);
        assert_eq!(basic.diff_add().fg, Some(Color::Green));
    }

    #[test]
    fn multi_file_and_creation_and_deletion_kinds() {
        let raw = "\
diff --git a/new.rs b/new.rs
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/new.rs
@@ -0,0 +1,2 @@
+one
+two
diff --git a/old.rs b/old.rs
deleted file mode 100644
index 1111111..0000000
--- a/old.rs
+++ /dev/null
@@ -1,1 +0,0 @@
-gone
";
        let document = parse_unified_diff(raw);
        assert_eq!(document.files.len(), 2);
        assert_eq!(document.files[0].kind(), 'A');
        assert!(document.files[0].is_new());
        assert_eq!(document.files[0].added_lines(), 2);
        assert_eq!(document.files[1].kind(), 'D');
        assert!(document.files[1].is_deleted());
        assert_eq!(document.files[1].removed_lines(), 1);
    }

    #[test]
    fn rename_is_recognized() {
        let raw = "\
diff --git a/old/name.rs b/new/name.rs
similarity index 90%
rename from old/name.rs
rename to new/name.rs
";
        let document = parse_unified_diff(raw);
        assert_eq!(document.files.len(), 1);
        assert_eq!(document.files[0].kind(), 'R');
        assert_eq!(
            document.files[0].rename(),
            Some(("old/name.rs".into(), "new/name.rs".into()))
        );
    }

    #[test]
    fn no_newline_marker_is_dim() {
        let raw = "\
diff --git a/a b/a
--- a/a
+++ b/a
@@ -1 +1 @@
-old
\\ No newline at end of file
+new
\\ No newline at end of file
";
        let document = parse_unified_diff(raw);
        let marker = document.files[0].hunks[0]
            .lines
            .iter()
            .find(|line| line.kind == DiffLineKind::NoNewline)
            .expect("no-newline marker");
        assert_eq!(marker.text, " No newline at end of file");
    }

    #[test]
    fn malformed_input_falls_back_to_uncolored_raw() {
        let raw = "error[E0502]: cannot borrow `cache` as mutable\n  --> src/lib.rs:238:20\n+not a diff addition";
        let document = parse_unified_diff(raw);
        assert!(!document.parsed);
        let lines = diff_lines(&document);
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|line| {
            line.spans
                .iter()
                .all(|span| span.style.fg != Some(Color::Green))
        }));
        assert!(document.raw.contains("E0502"));
    }

    #[test]
    fn cjk_and_long_lines_render_without_panicking() {
        let raw = "\
diff --git a/文档.txt b/文档.txt
--- a/文档.txt
+++ b/文档.txt
@@ -1 +1 @@
-旧内容
+新内容 with a very long tail 你好世界你好世界你好世界你好世界
";
        let document = parse_unified_diff(raw);
        let rendered = diff_lines(&document);
        assert!(rendered.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("新内容") && span.style.fg == Some(Color::Green))
        }));
        assert!(document.files[0].display_path().contains("文档.txt"));
    }

    #[test]
    fn semantic_diff_snapshot() {
        let document = parse_unified_diff(SAMPLE);
        let plain = diff_lines(&document)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            plain.trim_end(),
            include_str!("../tests/snapshots/v31_diff.txt")
                .replace("\r\n", "\n")
                .trim_end()
        );
    }

    #[test]
    fn bounded_rendering_keeps_raw_and_reports_truncation() {
        let mut raw = String::from("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -0,0 +1,500 @@\n");
        for index in 0..500 {
            raw.push_str(&format!("+line {index}\n"));
        }
        let document = parse_unified_diff(&raw);
        let bounded = diff_lines_bounded(&document, 50);
        assert_eq!(bounded.len(), 51);
        assert!(
            bounded.last().unwrap().spans[0]
                .content
                .contains("more diff lines")
        );
        assert_eq!(document.raw.lines().count(), 504);
    }
}
