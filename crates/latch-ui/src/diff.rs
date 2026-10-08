//! Typed unified-diff parsing and restrained semantic rendering.
//!
//! Diff red/green semantics only ever apply inside a parsed unified diff. A
//! parser failure falls back to raw, uncolored lines so compiler output,
//! Markdown, or arbitrary shell text can never be misclassified as additions
//! or deletions. `--- a/file` / `+++ b/file` are headers, not source lines.

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// `--- a/file` or `+++ b/file`.
    Header,
    /// File metadata such as `new file mode` or `index`.
    Meta,
    /// `@@ -1,2 +1,2 @@`.
    Hunk,
    Addition,
    Deletion,
    Context,
    /// `\ No newline at end of file`.
    NoNewline,
    /// Anything the parser does not understand; rendered with default styling.
    Other,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub meta: Vec<String>,
    pub hunks: Vec<DiffHunk>,
}

impl DiffFile {
    #[must_use]
    pub fn display_path(&self) -> String {
        match (&self.old_path, &self.new_path) {
            (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
            (_, Some(new)) => new.clone(),
            (Some(old), None) => old.clone(),
            (None, None) => "(unknown file)".into(),
        }
    }

    #[must_use]
    pub fn added_lines(&self) -> usize {
        self.hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.kind == DiffLineKind::Addition)
            .count()
    }

    #[must_use]
    pub fn removed_lines(&self) -> usize {
        self.hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.kind == DiffLineKind::Deletion)
            .count()
    }

    #[must_use]
    pub fn is_new(&self) -> bool {
        self.old_path.is_none() && self.new_path.is_some()
            || self
                .meta
                .iter()
                .any(|line| line.starts_with("new file mode"))
    }

    #[must_use]
    pub fn is_deleted(&self) -> bool {
        self.new_path.is_none() && self.old_path.is_some()
            || self
                .meta
                .iter()
                .any(|line| line.starts_with("deleted file mode"))
    }

    #[must_use]
    pub fn rename(&self) -> Option<(String, String)> {
        let from = self
            .meta
            .iter()
            .find_map(|line| line.strip_prefix("rename from ").map(str::to_owned))?;
        let to = self
            .meta
            .iter()
            .find_map(|line| line.strip_prefix("rename to ").map(str::to_owned))?;
        Some((from, to))
    }

    #[must_use]
    pub fn kind(&self) -> char {
        if self.is_new() {
            'A'
        } else if self.is_deleted() {
            'D'
        } else if self.rename().is_some() {
            'R'
        } else {
            'M'
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DiffDocument {
    pub files: Vec<DiffFile>,
    pub raw: String,
    /// False when the input did not look like a unified diff at all. The raw
    /// text is always retained and rendered without semantic coloring.
    pub parsed: bool,
}

impl DiffDocument {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    #[must_use]
    pub fn added_lines(&self) -> usize {
        self.files.iter().map(DiffFile::added_lines).sum()
    }

    #[must_use]
    pub fn removed_lines(&self) -> usize {
        self.files.iter().map(DiffFile::removed_lines).sum()
    }
}

/// Parses unified `git diff` output conservatively. Unrecognized lines become
/// [`DiffLineKind::Other`]; they are never colored as additions or deletions.
#[must_use]
pub fn parse_unified_diff(raw: &str) -> DiffDocument {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut current: Option<DiffFile> = None;
    let mut hunk: Option<DiffHunk> = None;
    let mut in_hunk = false;
    let mut lines = raw.lines().peekable();

    while let Some(line) = lines.next() {
        if line.starts_with("diff --git ") {
            flush(&mut files, &mut current, &mut hunk, &mut in_hunk);
            let (old_path, new_path) = git_header_paths(line);
            current = Some(DiffFile {
                old_path,
                new_path,
                meta: vec![],
                hunks: vec![],
            });
            continue;
        }
        if !in_hunk
            && let Some(old) = line.strip_prefix("--- ")
            && let Some(new_line) = lines.peek()
            && let Some(new) = new_line.strip_prefix("+++ ")
        {
            let new = new.trim().to_owned();
            lines.next();
            flush_hunk(&mut current, &mut hunk, &mut in_hunk);
            let file = current.get_or_insert_with(|| DiffFile {
                old_path: None,
                new_path: None,
                meta: vec![],
                hunks: vec![],
            });
            file.old_path = normalize_path(old);
            file.new_path = normalize_path(&new);
            continue;
        }
        if line.starts_with("@@") {
            flush_hunk(&mut current, &mut hunk, &mut in_hunk);
            let file = current.get_or_insert_with(|| DiffFile {
                old_path: None,
                new_path: None,
                meta: vec![],
                hunks: vec![],
            });
            file.hunks.push(DiffHunk {
                header: line.to_owned(),
                lines: vec![],
            });
            hunk = file.hunks.pop();
            in_hunk = true;
            continue;
        }
        if in_hunk {
            let kind = match line.as_bytes().first() {
                Some(b'+') => DiffLineKind::Addition,
                Some(b'-') => DiffLineKind::Deletion,
                Some(b'\\') => DiffLineKind::NoNewline,
                Some(b' ') => DiffLineKind::Context,
                None => DiffLineKind::Context,
                _ => DiffLineKind::Other,
            };
            let text = match kind {
                DiffLineKind::Addition
                | DiffLineKind::Deletion
                | DiffLineKind::Context
                | DiffLineKind::NoNewline => line.get(1..).unwrap_or("").to_owned(),
                _ => line.to_owned(),
            };
            if let Some(hunk) = hunk.as_mut() {
                hunk.lines.push(DiffLine { kind, text });
            }
            continue;
        }
        if let Some(file) = current.as_mut() {
            // Everything between file sections is retained verbatim and
            // rendered dim; only hunk bodies receive +/- semantics.
            file.meta.push(line.to_owned());
        }
    }
    flush(&mut files, &mut current, &mut hunk, &mut in_hunk);
    let parsed = !files.is_empty()
        && files
            .iter()
            .any(|file| !file.hunks.is_empty() || !file.meta.is_empty());
    DiffDocument {
        files,
        raw: raw.to_owned(),
        parsed,
    }
}

fn flush(
    files: &mut Vec<DiffFile>,
    current: &mut Option<DiffFile>,
    hunk: &mut Option<DiffHunk>,
    in_hunk: &mut bool,
) {
    flush_hunk(current, hunk, in_hunk);
    if let Some(file) = current.take() {
        files.push(file);
    }
}

fn flush_hunk(current: &mut Option<DiffFile>, hunk: &mut Option<DiffHunk>, in_hunk: &mut bool) {
    if let Some(hunk) = hunk.take()
        && let Some(file) = current.as_mut()
    {
        file.hunks.push(hunk);
    }
    *in_hunk = false;
}

/// Extracts the two paths from a `diff --git a/x b/y` header. Paths with
/// spaces are quoted by Git; the last two whitespace-separated quoted or bare
/// tokens are used conservatively.
fn git_header_paths(line: &str) -> (Option<String>, Option<String>) {
    let rest = line.trim_start_matches("diff --git ").trim();
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in rest.chars() {
        match ch {
            '"' => quoted = !quoted,
            ' ' | '\t' if !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    if tokens.len() >= 2 {
        let new = tokens.pop().unwrap_or_default();
        let old = tokens.pop().unwrap_or_default();
        (normalize_path(&old), normalize_path(&new))
    } else {
        (None, None)
    }
}

fn normalize_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches('"');
    if trimmed == "/dev/null" {
        return None;
    }
    let stripped = trimmed
        .strip_prefix("a/")
        .or_else(|| trimmed.strip_prefix("b/"))
        .unwrap_or(trimmed);
    Some(stripped.to_owned())
}
