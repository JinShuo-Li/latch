//! The Latch composer: a real multiline editor with an independent viewport.
//!
//! The composer keeps the *entire* input buffer. Rendering is a three-stage
//! pipeline:
//!
//! ```text
//! full buffer -> wrapped visual rows -> viewport -> terminal
//! ```
//!
//! The viewport can be scrolled independently of the cursor, so a prompt that
//! was pasted as hundreds of lines can be inspected from any position before
//! submission. Nothing is ever truncated; only the visible window changes.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// One visual (wrapped) row of a logical buffer line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualRow {
    /// Index into [`Composer::lines`].
    pub logical: usize,
    /// First char offset (inclusive) inside the logical line.
    pub start: usize,
    /// Last char offset (exclusive) inside the logical line.
    pub end: usize,
}

/// Multiline input buffer with cursor, history, and an independent viewport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composer {
    pub(crate) lines: Vec<String>,
    pub(crate) row: usize,
    /// Cursor column as a char offset within `lines[row]`.
    pub(crate) col: usize,
    /// Top visible visual row.
    pub(crate) viewport: usize,
    /// True while the user has scrolled the viewport away from the cursor.
    /// The next editing or navigation key re-pins the viewport to the cursor.
    viewport_locked: bool,
    /// Column the cursor should keep while moving vertically through wrapped
    /// rows, so `Up`/`Down` feel stable across short rows.
    desired_col: Option<usize>,
    pub(crate) history: Vec<String>,
    history_index: Option<usize>,
    draft: Option<String>,
}

impl Default for Composer {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            viewport: 0,
            viewport_locked: false,
            desired_col: None,
            history: Vec::new(),
            history_index: None,
            draft: None,
        }
    }
}

impl Composer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|line| line.is_empty())
    }

    #[must_use]
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    #[must_use]
    pub fn first_line(&self) -> &str {
        &self.lines[0]
    }

    #[must_use]
    pub fn viewport(&self) -> usize {
        self.viewport
    }

    #[must_use]
    pub fn viewport_locked(&self) -> bool {
        self.viewport_locked
    }

    fn current_len(&self) -> usize {
        self.lines[self.row].chars().count()
    }

    fn reset_navigation(&mut self) {
        self.viewport_locked = false;
        self.desired_col = None;
    }

    pub fn insert(&mut self, ch: char) {
        let line = &mut self.lines[self.row];
        let byte = char_to_byte(line, self.col);
        line.insert(byte, ch);
        self.col += 1;
        self.reset_navigation();
    }

    /// Inserts one paste payload without interpreting any embedded newline as
    /// an input event. CRLF and bare CR are normalized to durable `\n`. The
    /// whole payload is preserved; the viewport is re-pinned to the cursor.
    pub fn insert_text(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.is_empty() {
            return;
        }
        let parts = normalized.split('\n').collect::<Vec<_>>();
        let cursor_byte = char_to_byte(&self.lines[self.row], self.col);
        let suffix = self.lines[self.row][cursor_byte..].to_owned();
        self.lines[self.row].truncate(cursor_byte);
        self.lines[self.row].push_str(parts[0]);

        if parts.len() == 1 {
            self.lines[self.row].push_str(&suffix);
            self.col += parts[0].chars().count();
            self.reset_navigation();
            return;
        }

        let insert_at = self.row + 1;
        for (offset, part) in parts.iter().skip(1).enumerate() {
            let mut line = (*part).to_owned();
            if offset + 2 == parts.len() {
                line.push_str(&suffix);
            }
            self.lines.insert(insert_at + offset, line);
        }
        self.row += parts.len() - 1;
        self.col = parts.last().map_or(0, |part| part.chars().count());
        self.reset_navigation();
    }

    /// Inserts a newline (Alt+Enter): splits the current line at the cursor.
    pub fn newline(&mut self) {
        let line = self.lines[self.row].clone();
        let byte = char_to_byte(&line, self.col);
        let (head, tail) = line.split_at(byte);
        let tail = tail.to_owned();
        self.lines[self.row] = head.to_owned();
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
        self.reset_navigation();
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let cursor_byte = char_to_byte(line, self.col);
            let previous = line[..cursor_byte]
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(byte, _)| byte);
            let removed_chars = line[previous..cursor_byte].chars().count();
            line.drain(previous..cursor_byte);
            self.col = self.col.saturating_sub(removed_chars);
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&line);
        }
        self.reset_navigation();
    }

    pub fn delete(&mut self) {
        let line = &mut self.lines[self.row];
        if self.col < line.chars().count() {
            let byte = char_to_byte(line, self.col);
            let removed = line[byte..]
                .graphemes(true)
                .next()
                .map(str::len)
                .unwrap_or(0);
            line.drain(byte..byte + removed);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
        self.reset_navigation();
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            let byte = char_to_byte(&self.lines[self.row], self.col);
            let step = self.lines[self.row][..byte]
                .graphemes(true)
                .next_back()
                .map_or(1, |g| g.chars().count());
            self.col = self.col.saturating_sub(step);
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.current_len();
        }
        self.reset_navigation();
    }

    pub fn right(&mut self) {
        if self.col < self.current_len() {
            let byte = char_to_byte(&self.lines[self.row], self.col);
            let step = self.lines[self.row][byte..]
                .graphemes(true)
                .next()
                .map_or(1, |g| g.chars().count());
            self.col = (self.col + step).min(self.current_len());
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
        self.reset_navigation();
    }

    /// Moves the cursor between *visual* rows, preserving the desired display
    /// column across wrapped lines. `Up` from the first visual row recalls
    /// prompt history; `Down` past the last row advances history.
    pub fn up(&mut self, width: usize) {
        let layout = self.layout(width);
        let (current, _) = self.cursor_visual(&layout);
        if current == 0 {
            self.history_previous();
            return;
        }
        self.move_visual_rows(-1, &layout);
    }

    pub fn down(&mut self, width: usize) {
        let layout = self.layout(width);
        let (current, _) = self.cursor_visual(&layout);
        if current + 1 >= layout.len() {
            self.history_next();
            return;
        }
        self.move_visual_rows(1, &layout);
    }

    pub fn page_up(&mut self, width: usize, viewport_height: usize) {
        let layout = self.layout(width);
        let delta = viewport_height.max(1) as isize;
        self.move_visual_rows(-delta, &layout);
    }

    pub fn page_down(&mut self, width: usize, viewport_height: usize) {
        let layout = self.layout(width);
        let delta = viewport_height.max(1) as isize;
        self.move_visual_rows(delta, &layout);
    }

    fn move_visual_rows(&mut self, delta: isize, layout: &[VisualRow]) {
        if layout.is_empty() {
            return;
        }
        let (current, col) = self.cursor_visual(layout);
        let target = (current as isize + delta).clamp(0, layout.len() as isize - 1) as usize;
        let desired = *self.desired_col.get_or_insert(col);
        let row = layout[target];
        self.row = row.logical;
        self.col = self.visual_to_char(&row, desired);
        self.viewport_locked = false;
    }

    pub fn line_home(&mut self) {
        self.col = 0;
        self.reset_navigation();
    }

    pub fn line_end(&mut self) {
        self.col = self.current_len();
        self.reset_navigation();
    }

    /// Ctrl+Home: start of the whole buffer.
    pub fn buffer_home(&mut self) {
        self.row = 0;
        self.col = 0;
        self.reset_navigation();
    }

    /// Ctrl+End: end of the whole buffer.
    pub fn buffer_end(&mut self) {
        self.row = self.lines.len() - 1;
        self.col = self.current_len();
        self.reset_navigation();
    }

    pub fn kill_to_line_start(&mut self) {
        let line = &mut self.lines[self.row];
        let byte = char_to_byte(line, self.col);
        line.drain(..byte);
        self.col = 0;
        self.reset_navigation();
    }

    pub fn kill_to_line_end(&mut self) {
        let line = &mut self.lines[self.row];
        let byte = char_to_byte(line, self.col);
        line.drain(byte..);
        self.reset_navigation();
    }

    /// Ctrl+W: delete the word before the cursor.
    pub fn kill_word(&mut self) {
        let line = &mut self.lines[self.row];
        let chars: Vec<char> = line.chars().collect();
        let end = self.col.min(chars.len());
        let mut start = end;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        if start < end {
            let tail: String = chars[end..].iter().collect();
            let head: String = chars[..start].iter().collect();
            *line = format!("{head}{tail}");
            self.col = start;
        } else if self.row > 0 && end == 0 {
            self.backspace();
            return;
        }
        self.reset_navigation();
    }

    /// Recalls the previous submitted prompt. Editing a recalled prompt never
    /// mutates the stored history entry.
    pub fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.draft = Some(self.text());
                let index = self.history.len() - 1;
                self.history_index = Some(index);
                self.set_text(&self.history[index].clone());
            }
            Some(0) => {}
            Some(index) => {
                self.history_index = Some(index - 1);
                self.set_text(&self.history[index - 1].clone());
            }
        }
        self.viewport = 0;
        self.viewport_locked = false;
        self.desired_col = None;
    }

    /// Returns toward the newest entry; passing it restores the in-progress
    /// draft unchanged.
    pub fn history_next(&mut self) {
        match self.history_index {
            None => {}
            Some(index) if index + 1 < self.history.len() => {
                self.history_index = Some(index + 1);
                self.set_text(&self.history[index + 1].clone());
            }
            Some(_) => {
                self.history_index = None;
                if let Some(draft) = self.draft.take() {
                    self.set_text(&draft);
                }
            }
        }
        self.viewport = 0;
        self.viewport_locked = false;
        self.desired_col = None;
    }

    pub(crate) fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(str::to_owned).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.current_len();
    }

    /// Seeds prompt history from the durable session (resume).
    pub fn seed_history(&mut self, history: Vec<String>) {
        self.history = history;
    }

    /// Takes the composed text for submission, records it in history, and
    /// resets the editor and viewport.
    pub fn take_for_submit(&mut self) -> String {
        let text = self.text();
        if self
            .history
            .last()
            .map(|last| last != &text)
            .unwrap_or(true)
            && !text.trim().is_empty()
        {
            self.history.push(text.clone());
        }
        self.history_index = None;
        self.draft = None;
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
        self.viewport = 0;
        self.viewport_locked = false;
        self.desired_col = None;
        text
    }

    // ---- layout and viewport ----

    /// Wraps the entire buffer into visual rows at `width` columns.
    #[must_use]
    pub fn layout(&self, width: usize) -> Vec<VisualRow> {
        let width = width.max(1);
        let mut rows = Vec::new();
        for (logical, line) in self.lines.iter().enumerate() {
            let points = wrap_points(line, width);
            for (index, &start) in points.iter().enumerate() {
                let end = points
                    .get(index + 1)
                    .copied()
                    .unwrap_or_else(|| line.chars().count());
                rows.push(VisualRow {
                    logical,
                    start,
                    end,
                });
            }
        }
        if rows.is_empty() {
            rows.push(VisualRow {
                logical: 0,
                start: 0,
                end: 0,
            });
        }
        rows
    }

    /// The text of one visual row, taken from the untouched logical line.
    #[must_use]
    pub fn row_text(&self, row: &VisualRow) -> String {
        self.lines[row.logical]
            .chars()
            .skip(row.start)
            .take(row.end.saturating_sub(row.start))
            .collect()
    }

    /// Visual `(row, display column)` of the cursor at `width`. When the
    /// cursor sits at a soft-wrap boundary it belongs to the later row.
    #[must_use]
    pub fn cursor_visual(&self, layout: &[VisualRow]) -> (usize, usize) {
        let mut last = 0usize;
        for (index, row) in layout.iter().enumerate() {
            if row.logical != self.row {
                continue;
            }
            last = index;
            if self.col < row.end || row.start == row.end {
                return (index, self.display_col(row));
            }
        }
        (
            last,
            self.display_col(layout.get(last).expect("layout is never empty")),
        )
    }

    /// Display column of the cursor inside `row`, measured in grapheme widths
    /// so combining marks and emoji never shift the terminal cursor.
    fn display_col(&self, row: &VisualRow) -> usize {
        let line = &self.lines[row.logical];
        let mut char_index = 0usize;
        let mut col = 0usize;
        for grapheme in line.graphemes(true) {
            let count = grapheme.chars().count();
            let start = char_index;
            let end = char_index + count;
            char_index = end;
            if start < row.start {
                continue;
            }
            if start >= self.col {
                break;
            }
            if end > self.col {
                break;
            }
            col += UnicodeWidthStr::width(grapheme).max(1);
        }
        col
    }

    /// Char offset inside `row` closest to a target display column, snapping to
    /// grapheme boundaries.
    fn visual_to_char(&self, row: &VisualRow, target_col: usize) -> usize {
        let line = &self.lines[row.logical];
        let mut char_index = 0usize;
        let mut col = 0usize;
        for grapheme in line.graphemes(true) {
            let count = grapheme.chars().count();
            let start = char_index;
            let end = char_index + count;
            if start < row.start {
                char_index = end;
                continue;
            }
            if start >= row.end {
                break;
            }
            let width = UnicodeWidthStr::width(grapheme).max(1);
            if col + width > target_col {
                break;
            }
            col += width;
            char_index = end;
        }
        char_index
    }

    /// Keeps the cursor visible inside a `viewport_height` window. While the
    /// user has explicitly scrolled with the mouse the viewport is respected.
    pub fn reconcile_viewport(&mut self, width: usize, viewport_height: usize) {
        let layout = self.layout(width);
        let height = viewport_height.max(1);
        let max = layout.len().saturating_sub(height);
        if self.viewport_locked {
            self.viewport = self.viewport.min(max);
            return;
        }
        let (cursor, _) = self.cursor_visual(&layout);
        if cursor < self.viewport {
            self.viewport = cursor;
        } else if cursor >= self.viewport + height {
            self.viewport = cursor + 1 - height;
        }
        self.viewport = self.viewport.min(max);
    }

    /// Scrolls the viewport without touching the buffer or the cursor.
    pub fn scroll_lines(&mut self, delta: isize, width: usize, viewport_height: usize) {
        let layout = self.layout(width);
        let max = layout.len().saturating_sub(viewport_height.max(1));
        let next = (self.viewport as isize + delta).clamp(0, max as isize) as usize;
        if next != self.viewport {
            self.viewport = next;
            self.viewport_locked = true;
        }
    }

    #[must_use]
    pub fn is_scrollable(&self, width: usize, viewport_height: usize) -> bool {
        self.layout(width).len() > viewport_height.max(1)
    }

    #[must_use]
    pub fn total_visual_rows(&self, width: usize) -> usize {
        self.layout(width).len()
    }
}

/// Char offset of a char index within `text`.
fn char_to_byte(text: &str, offset: usize) -> usize {
    text.char_indices()
        .nth(offset)
        .map_or(text.len(), |(byte, _)| byte)
}

/// One grapheme cluster with its char range and display width.
#[derive(Debug, Clone, Copy)]
struct Grapheme {
    start: usize,
    end: usize,
    width: usize,
    is_space: bool,
}

fn graphemes(line: &str) -> Vec<Grapheme> {
    let mut out = Vec::new();
    let mut char_index = 0usize;
    for grapheme in line.graphemes(true) {
        let count = grapheme.chars().count();
        out.push(Grapheme {
            start: char_index,
            end: char_index + count,
            width: UnicodeWidthStr::width(grapheme).max(1),
            is_space: grapheme == " ",
        });
        char_index += count;
    }
    out
}

/// Word-aware soft wrap of one logical line. Returns the char offset at which
/// each visual row starts; row `i` spans `points[i]..points[i+1]` (the last row
/// spans to the end of the line). Breaks always land on grapheme boundaries,
/// the buffer is never modified, and no character is ever dropped: a row
/// boundary only changes where the line is displayed.
#[must_use]
pub fn wrap_points(line: &str, width: usize) -> Vec<usize> {
    let mut points = vec![0usize];
    if width == 0 {
        return points;
    }
    let graphemes = graphemes(line);
    if graphemes.is_empty() {
        return points;
    }
    let mut row_start = 0usize;
    let mut col = 0usize;
    let mut last_space: Option<usize> = None;
    let mut index = 0usize;
    while index < graphemes.len() {
        let grapheme = graphemes[index];
        if col + grapheme.width > width && grapheme.start > row_start {
            // Prefer breaking after the last space in this row; otherwise hard
            // break before the overflowing grapheme.
            let break_at = match last_space.filter(|space| graphemes[*space].start >= row_start) {
                Some(space) => graphemes[space].end,
                None => grapheme.start,
            };
            points.push(break_at);
            row_start = break_at;
            col = graphemes
                .iter()
                .filter(|candidate| {
                    candidate.start >= row_start && candidate.start < grapheme.start
                })
                .map(|candidate| candidate.width)
                .sum();
            last_space = None;
            continue;
        }
        if grapheme.is_space {
            last_space = Some(index);
        }
        col += grapheme.width;
        index += 1;
    }
    points
}

/// Display width of a whole string (used by layout tests and labels).
#[must_use]
pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composer_with(text: &str) -> Composer {
        let mut composer = Composer::new();
        composer.insert_text(text);
        composer
    }

    fn visible_rows(composer: &Composer, width: usize, height: usize) -> Vec<String> {
        let layout = composer.layout(width);
        layout
            .iter()
            .skip(composer.viewport())
            .take(height)
            .map(|row| composer.row_text(row))
            .collect()
    }

    #[test]
    fn full_multiline_paste_is_preserved_and_reachable() {
        let text = (0..200)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut composer = composer_with(&text);
        assert_eq!(composer.line_count(), 200);
        assert_eq!(composer.text(), text, "the entire buffer survives paste");
        // Cursor is at the end; the viewport follows it.
        composer.reconcile_viewport(40, 8);
        let layout = composer.layout(40);
        assert_eq!(layout.len(), 200);
        let (cursor, _) = composer.cursor_visual(&layout);
        assert!(composer.viewport() <= cursor);
        assert!(cursor < composer.viewport() + 8);
        // Scroll to the top and inspect the first pasted line.
        composer.scroll_lines(-1000, 40, 8);
        assert_eq!(composer.viewport(), 0);
        assert_eq!(visible_rows(&composer, 40, 8)[0], "line 0");
        // Scroll to the middle and inspect an arbitrary earlier line.
        composer.scroll_lines(97, 40, 8);
        assert_eq!(visible_rows(&composer, 40, 8)[0], "line 97");
        // Scroll to the bottom: the last line is reachable.
        composer.scroll_lines(1000, 40, 8);
        assert_eq!(visible_rows(&composer, 40, 8).last().unwrap(), "line 199");
        // Buffer unchanged after all scrolling.
        assert_eq!(composer.text(), text);
    }

    #[test]
    fn cursor_movement_crosses_viewport_boundaries() {
        let mut composer = composer_with(
            &(0..30)
                .map(|index| format!("row {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        composer.row = 20;
        composer.col = 0;
        composer.reconcile_viewport(40, 6);
        assert!(composer.viewport() >= 15, "viewport follows a far cursor");
        // Move up beyond the top of the viewport; the viewport follows.
        for _ in 0..10 {
            composer.up(40);
            composer.reconcile_viewport(40, 6);
        }
        let layout = composer.layout(40);
        let (cursor, _) = composer.cursor_visual(&layout);
        assert!(cursor >= composer.viewport());
        assert!(cursor < composer.viewport() + 6);
        assert_eq!(composer.cursor(), (10, 0));
        // Move back down across the boundary.
        for _ in 0..15 {
            composer.down(40);
            composer.reconcile_viewport(40, 6);
        }
        let layout = composer.layout(40);
        let (cursor, _) = composer.cursor_visual(&layout);
        assert!(cursor < composer.viewport() + 6);
        assert_eq!(composer.cursor().0, 25);
    }

    #[test]
    fn wrapped_lines_map_the_cursor_correctly() {
        let mut composer = Composer::new();
        composer.insert_text(&"word ".repeat(40));
        let width = 24;
        let layout = composer.layout(width);
        assert!(layout.len() > 1, "long line wraps");
        // The cursor sits on the last visual row at the end of the last word.
        let (cursor_row, cursor_col) = composer.cursor_visual(&layout);
        assert_eq!(cursor_row, layout.len() - 1);
        // Every row is within the width and rows are contiguous.
        for (index, row) in layout.iter().enumerate() {
            assert!(display_width(&composer.row_text(row)) <= width);
            if let Some(next) = layout.get(index + 1) {
                assert_eq!(row.end, next.start, "rows must be contiguous");
            }
        }
        let _ = cursor_col;
    }

    #[test]
    fn vertical_movement_preserves_desired_column_across_short_rows() {
        let mut composer = composer_with("hello world\nx\nhello world");
        composer.row = 0;
        composer.col = 8; // inside "hello world"
        composer.desired_col = None;
        composer.down(80);
        assert_eq!(composer.cursor().0, 1);
        composer.down(80);
        assert_eq!(composer.cursor().0, 2);
        // The short middle line clamps, but the desired column is remembered.
        assert_eq!(composer.cursor().1, 8);
        composer.up(80);
        composer.up(80);
        assert_eq!(composer.cursor(), (0, 8));
    }

    #[test]
    fn unicode_and_cjk_cursor_and_scroll_are_consistent() {
        let mut composer = Composer::new();
        composer.insert_text("你好世界\n第二行 with mixed text 🌍\nthird");
        assert!(composer.text().contains("🌍"));
        let layout = composer.layout(6);
        // Wide characters wrap; cursor column is a display width, never a byte
        // or char count that would misplace the terminal cursor.
        let (_, col) = composer.cursor_visual(&layout);
        assert!(col <= 6 * 2);
        composer.buffer_home();
        composer.page_down(6, 2);
        composer.reconcile_viewport(6, 2);
        assert_eq!(
            composer.text(),
            "你好世界\n第二行 with mixed text 🌍\nthird"
        );
    }

    #[test]
    fn page_navigation_moves_within_the_viewport() {
        let mut composer = composer_with(
            &(0..40)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        composer.row = 0;
        composer.col = 0;
        composer.page_down(30, 5);
        assert_eq!(composer.cursor().0, 5);
        composer.page_down(30, 5);
        assert_eq!(composer.cursor().0, 10);
        composer.page_up(30, 5);
        assert_eq!(composer.cursor().0, 5);
        composer.page_up(30, 5);
        assert_eq!(composer.cursor().0, 0);
        // Page up at the top clamps instead of recalling history.
        composer.page_up(30, 5);
        assert_eq!(composer.cursor(), (0, 0));
    }

    #[test]
    fn editing_outside_the_initially_visible_region_works() {
        let mut composer = composer_with(
            &(0..50)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        // Inspect the top without moving the cursor, then edit there.
        composer.scroll_lines(-100, 40, 6);
        assert_eq!(composer.viewport(), 0);
        composer.buffer_home();
        composer.line_end();
        composer.insert('!');
        assert!(composer.text().starts_with("line 0!"));
        // Cursor is pinned to row 0 and the viewport followed it.
        assert_eq!(composer.cursor(), (0, 7));
        composer.reconcile_viewport(40, 6);
        let layout = composer.layout(40);
        let (cursor, _) = composer.cursor_visual(&layout);
        assert!(cursor >= composer.viewport() && cursor < composer.viewport() + 6);
        // The rest of the buffer is intact.
        assert!(composer.text().ends_with("line 49"));
    }

    #[test]
    fn resize_keeps_the_buffer_and_cursor_visible() {
        let mut composer = composer_with(&"long word ".repeat(80));
        composer.reconcile_viewport(30, 5);
        let before = composer.text();
        let wide_layout = composer.layout(80);
        let wide_cursor = composer.cursor_visual(&wide_layout);
        assert!(wide_cursor.0 < composer.viewport() + 5);
        composer.reconcile_viewport(20, 5);
        let narrow_layout = composer.layout(20);
        assert!(narrow_layout.len() > wide_layout.len());
        let (cursor, _) = composer.cursor_visual(&narrow_layout);
        assert!(cursor < composer.viewport() + 5);
        assert_eq!(composer.text(), before, "resize never changes the buffer");
    }

    #[test]
    fn home_end_and_ctrl_variants() {
        let mut composer = composer_with("first\nsecond\nthird");
        composer.line_home();
        assert_eq!(composer.cursor(), (2, 0));
        composer.line_end();
        assert_eq!(composer.cursor(), (2, 5));
        composer.buffer_home();
        assert_eq!(composer.cursor(), (0, 0));
        composer.buffer_end();
        assert_eq!(composer.cursor(), (2, 5));
    }

    #[test]
    fn history_interacts_with_visual_navigation() {
        let mut composer = Composer::new();
        composer.seed_history(vec!["earlier".into()]);
        composer.insert('x');
        composer.up(40);
        assert_eq!(composer.text(), "earlier");
        composer.down(40);
        assert_eq!(composer.text(), "x");
    }

    #[test]
    fn wrapped_history_recall_does_not_lose_text() {
        let long = "a ".repeat(200);
        let mut composer = Composer::new();
        composer.seed_history(vec![long.clone()]);
        composer.up(20);
        assert_eq!(composer.text(), long, "recall preserves the exact buffer");
        composer.down(20);
        assert_eq!(composer.text(), "");
    }

    #[test]
    fn scroll_lines_does_not_modify_the_buffer() {
        let text = (0..100)
            .map(|index| format!("item {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut composer = composer_with(&text);
        composer.scroll_lines(-10, 20, 4);
        composer.scroll_lines(3, 20, 4);
        assert_eq!(composer.text(), text);
        assert!(composer.viewport_locked());
        // Any editing key re-pins the viewport to the cursor.
        composer.insert('z');
        assert!(!composer.viewport_locked());
    }
}
