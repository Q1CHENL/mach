//! A one-line editor. Every text field in mach is built from it —
//! the title, the due date, category names, the command line, the search
//! box, and each block of a task's description. Cursor and selection indices are
//! Unicode grapheme clusters; display geometry is still measured in cells.
//!
//! Selection follows macOS: Shift extends, Option jumps by word, and
//! Shift+Option+W selects the word under the cursor.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TextInput {
    /// Extended grapheme clusters. A visible user-perceived character is
    /// never split by cursor movement, selection, or deletion.
    chars: Vec<String>,
    cursor: usize,
    /// Other end of the selection; `None` means a caret only.
    sel_anchor: Option<usize>,
    scroll: usize,
    max_len: usize,
    max_bytes: usize,
}

/// What [`TextInput::visible`] hands the UI: text, caret column, and an
/// optional selection span in display columns within that text.
#[derive(Debug, Clone)]
pub struct View {
    pub text: String,
    pub cursor_col: u16,
    /// Inclusive start, exclusive end, in display columns of `text`.
    pub sel_cols: Option<(u16, u16)>,
}

/// One visual row of a soft-wrapped field.
#[derive(Debug, Clone)]
pub struct WrappedLine {
    pub text: String,
    pub sel_cols: Option<(u16, u16)>,
    /// Grapheme range into the full buffer covered by this row.
    pub start: usize,
    pub end: usize,
}

/// Soft-wrapped layout for multi-line description painting.
#[derive(Debug, Clone)]
pub struct WrappedView {
    pub lines: Vec<WrappedLine>,
    /// Cursor row within `lines`, and column within that row.
    pub cursor_row: u16,
    pub cursor_col: u16,
}

/// Soft-wrap graphemes into `(start, end)` ranges of at most `width`
/// display columns. Prefers breaking after a space; otherwise hard-breaks.
pub fn wrap_breaks(chars: &[String], width: usize) -> Vec<(usize, usize)> {
    if chars.is_empty() {
        return vec![(0, 0)];
    }
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut col = 0usize;
    let mut last_ws: Option<usize> = None;
    let mut i = 0usize;

    while i < chars.len() {
        let w = chars[i].width();
        if w > width {
            // Lone character wider than the field — own row.
            if i > start {
                lines.push((start, i));
            }
            lines.push((i, i + 1));
            i += 1;
            start = i;
            col = 0;
            last_ws = None;
            continue;
        }
        if col + w > width && i > start {
            // Prefer the last space so words stay intact when they fit.
            let end = last_ws
                .filter(|&ws| ws >= start)
                .map(|ws| ws + 1)
                .unwrap_or(i);
            let end = end.max(start + 1);
            lines.push((start, end));
            start = end;
            i = start;
            col = 0;
            last_ws = None;
            continue;
        }
        if is_whitespace(&chars[i]) {
            last_ws = Some(i);
        }
        col += w;
        i += 1;
    }
    if start < chars.len() {
        lines.push((start, chars.len()));
    }
    lines
}

fn is_whitespace(grapheme: &str) -> bool {
    !grapheme.is_empty() && grapheme.chars().all(char::is_whitespace)
}

impl TextInput {
    pub fn new(initial: &str, max_len: usize) -> Self {
        let max_bytes = crate::model::text_byte_limit(max_len);
        let chars = bounded_graphemes(initial, max_len, max_bytes);
        let cursor = chars.len();
        Self {
            chars,
            cursor,
            sel_anchor: None,
            scroll: 0,
            max_len,
            max_bytes,
        }
    }

    pub fn value(&self) -> String {
        self.chars.concat()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn slice(&self, start: usize, end: usize) -> String {
        let start = start.min(self.chars.len());
        let end = end.max(start).min(self.chars.len());
        self.chars[start..end].concat()
    }

    pub fn clear_selection(&mut self) {
        self.sel_anchor = None;
    }

    pub fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    /// Inclusive start and exclusive end of the selection, if any.
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let a = self.sel_anchor?;
        let (lo, hi) = if a <= self.cursor {
            (a, self.cursor)
        } else {
            (self.cursor, a)
        };
        (lo < hi).then_some((lo, hi))
    }

    pub fn selected_text(&self) -> Option<String> {
        let (lo, hi) = self.selection_range()?;
        Some(self.chars[lo..hi].concat())
    }

    /// Removes the selected range. Returns true when something was deleted.
    pub fn delete_selection(&mut self) -> bool {
        let Some((lo, hi)) = self.selection_range() else {
            return false;
        };
        self.replace_range(lo, hi, "");
        true
    }

    /// Select the word under (or beside) the cursor — macOS Select Word.
    pub fn select_word(&mut self) {
        if self.chars.is_empty() {
            self.sel_anchor = None;
            return;
        }
        let n = self.chars.len();
        let i = self.cursor.min(n);
        // Prefer the word containing the caret; if on a boundary, the
        // word to the left; if mid-whitespace, the next word to the right.
        let (mut start, mut end) = if i < n && !is_whitespace(&self.chars[i]) {
            (i, i + 1)
        } else if i > 0 && !is_whitespace(&self.chars[i - 1]) {
            (i - 1, i)
        } else {
            let mut next = i;
            while next < n && is_whitespace(&self.chars[next]) {
                next += 1;
            }
            if next < n {
                (next, next + 1)
            } else {
                let mut previous = i;
                while previous > 0 && is_whitespace(&self.chars[previous - 1]) {
                    previous -= 1;
                }
                if previous == 0 {
                    self.sel_anchor = None;
                    return;
                }
                (previous - 1, previous)
            }
        };
        while start > 0 && !is_whitespace(&self.chars[start - 1]) {
            start -= 1;
        }
        while end < n && !is_whitespace(&self.chars[end]) {
            end += 1;
        }
        self.sel_anchor = Some(start);
        self.cursor = end;
    }

    pub fn set_cursor(&mut self, cursor: usize) {
        self.clear_selection();
        self.cursor = cursor.min(self.chars.len());
    }

    /// Places the cursor at a display column of the visible window, as
    /// produced by [`TextInput::visible`]. Past the end of the text the
    /// cursor lands at the end.
    pub fn set_cursor_from_col(&mut self, col: usize) {
        self.clear_selection();
        let mut used = 0;
        let mut cursor = self.scroll;
        for c in &self.chars[self.scroll.min(self.chars.len())..] {
            let w = c.width();
            if used + w > col {
                break;
            }
            used += w;
            cursor += 1;
        }
        self.cursor = cursor.min(self.chars.len());
    }

    pub fn at_start(&self) -> bool {
        self.cursor == 0
    }

    pub fn at_end(&self) -> bool {
        self.cursor == self.chars.len()
    }

    /// Cuts everything after the cursor into a new input.
    pub fn split_off_at_cursor(&mut self) -> Self {
        self.clear_selection();
        let tail: Vec<String> = self.chars.split_off(self.cursor);
        Self {
            chars: tail,
            cursor: 0,
            sel_anchor: None,
            scroll: 0,
            max_len: self.max_len,
            max_bytes: self.max_bytes,
        }
    }

    /// Appends another input's text, leaving the cursor at the join. Returns
    /// `false` without changing either input when the full join would exceed
    /// this field's grapheme or byte limit.
    pub fn append(&mut self, other: &Self) -> bool {
        let cursor = self.chars.len();
        let left = self.value();
        let right = other.value();
        if left.len().saturating_add(right.len()) > self.max_bytes {
            return false;
        }
        let joined = left + &right;
        let chars: Vec<String> = joined.graphemes(true).map(str::to_string).collect();
        if chars.len() > self.max_len {
            return false;
        }
        self.clear_selection();
        self.chars = chars;
        self.cursor = cursor.min(self.chars.len());
        self.scroll = self.scroll.min(self.cursor);
        true
    }

    fn replace_range(&mut self, lo: usize, hi: usize, replacement: &str) {
        let lo = lo.min(self.chars.len());
        let hi = hi.max(lo).min(self.chars.len());
        let prefix = self.chars[..lo].concat();
        let suffix = self.chars[hi..].concat();
        let before_cursor = format!("{prefix}{replacement}");
        let cursor = before_cursor.graphemes(true).count();
        let joined = before_cursor + &suffix;
        self.chars = joined.graphemes(true).map(str::to_string).collect();
        self.cursor = cursor.min(self.chars.len());
        self.sel_anchor = None;
        self.scroll = self.scroll.min(self.cursor);
    }

    pub fn insert(&mut self, c: char) {
        self.insert_str(&c.to_string());
    }

    /// Inserts a run of text at the cursor. Tab/newline separators are
    /// flattened, while terminal control bytes are discarded at the input
    /// boundary so they can never reach the renderer.
    pub fn insert_str(&mut self, text: &str) {
        let (lo, hi) = self.selection_range().unwrap_or((self.cursor, self.cursor));
        let prefix = self.chars[..lo].concat();
        let suffix = self.chars[hi..].concat();
        let available = self
            .max_bytes
            .saturating_sub(prefix.len().saturating_add(suffix.len()));
        let mut flattened = String::with_capacity(text.len().min(available));
        for character in text.chars().filter_map(|character| match character {
            '\t' | '\n' | '\r' => Some(' '),
            character if character.is_control() => None,
            _ => Some(character),
        }) {
            if flattened.len().saturating_add(character.len_utf8()) > available {
                break;
            }
            flattened.push(character);
        }
        if flattened.is_empty() {
            return;
        }
        let joined = format!("{prefix}{flattened}{suffix}");
        let accepted = if joined.graphemes(true).count() <= self.max_len {
            flattened
        } else {
            // A candidate can merge with graphemes on either side (combining
            // marks and ZWJ sequences), so preserve exact boundary behavior
            // while finding the longest valid prefix.
            let mut accepted = String::new();
            for grapheme in flattened.graphemes(true) {
                let candidate = format!("{prefix}{accepted}{grapheme}{suffix}");
                if candidate.graphemes(true).count() > self.max_len {
                    break;
                }
                accepted.push_str(grapheme);
            }
            accepted
        };
        if !accepted.is_empty() {
            self.replace_range(lo, hi, &accepted);
        }
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor > 0 {
            self.replace_range(self.cursor - 1, self.cursor, "");
        }
    }

    pub fn delete(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor < self.chars.len() {
            self.replace_range(self.cursor, self.cursor + 1, "");
        }
    }

    pub fn delete_to_start(&mut self) {
        if self.delete_selection() {
            return;
        }
        self.replace_range(0, self.cursor, "");
    }

    pub fn delete_to_end(&mut self) {
        if self.delete_selection() {
            return;
        }
        self.replace_range(self.cursor, self.chars.len(), "");
    }

    pub fn delete_word_left(&mut self) {
        if self.delete_selection() {
            return;
        }
        let target = self.word_left_index();
        self.replace_range(target, self.cursor, "");
    }

    fn move_to(&mut self, pos: usize, extend: bool) {
        if extend {
            if self.sel_anchor.is_none() {
                self.sel_anchor = Some(self.cursor);
            }
        } else {
            self.sel_anchor = None;
        }
        self.cursor = pos.min(self.chars.len());
    }

    pub fn left(&mut self) {
        self.move_to(self.cursor.saturating_sub(1), false);
    }

    pub fn right(&mut self) {
        self.move_to((self.cursor + 1).min(self.chars.len()), false);
    }

    pub fn select_left(&mut self) {
        self.move_to(self.cursor.saturating_sub(1), true);
    }

    pub fn select_right(&mut self) {
        self.move_to((self.cursor + 1).min(self.chars.len()), true);
    }

    pub fn word_left(&mut self) {
        self.move_to(self.word_left_index(), false);
    }

    pub fn word_right(&mut self) {
        self.move_to(self.word_right_index(), false);
    }

    pub fn select_word_left(&mut self) {
        self.move_to(self.word_left_index(), true);
    }

    pub fn select_word_right(&mut self) {
        self.move_to(self.word_right_index(), true);
    }

    pub fn home(&mut self) {
        self.move_to(0, false);
    }

    pub fn end(&mut self) {
        self.move_to(self.chars.len(), false);
    }

    pub fn select_home(&mut self) {
        self.move_to(0, true);
    }

    pub fn select_end(&mut self) {
        self.move_to(self.chars.len(), true);
    }

    /// Where the caret lands moving one word left; the description editor uses
    /// this to extend a selection across block boundaries.
    pub fn word_left_index(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && is_whitespace(&self.chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && !is_whitespace(&self.chars[i - 1]) {
            i -= 1;
        }
        i
    }

    /// Where the caret lands moving one word right.
    pub fn word_right_index(&self) -> usize {
        let mut i = self.cursor;
        let n = self.chars.len();
        while i < n && is_whitespace(&self.chars[i]) {
            i += 1;
        }
        while i < n && !is_whitespace(&self.chars[i]) {
            i += 1;
        }
        i
    }

    /// Soft-wrap into visual rows of at most `width` columns. Prefers
    /// breaking after whitespace; hard-breaks when a single word is wider
    /// than the field. Empty text yields one empty row.
    pub fn wrap_breaks(&self, width: usize) -> Vec<(usize, usize)> {
        wrap_breaks(&self.chars, width)
    }

    /// How many visual rows the text needs at `width` (at least 1).
    pub fn wrap_height(&self, width: usize) -> usize {
        self.wrap_breaks(width).len()
    }

    /// Cursor as `(visual_row, column)` from precomputed wrap breaks.
    pub fn wrap_cursor_from_breaks(&self, breaks: &[(usize, usize)]) -> (usize, u16) {
        for (row, &(start, end)) in breaks.iter().enumerate() {
            if self.cursor < end || (self.cursor == end && row + 1 == breaks.len()) {
                let col: usize = self.chars[start..self.cursor.min(end)]
                    .iter()
                    .map(|c| c.width())
                    .sum();
                // When the caret sits exactly at a soft-break and another
                // row follows, show it at the start of the next row.
                if self.cursor == end && row + 1 < breaks.len() && end < self.chars.len() {
                    return (row + 1, 0);
                }
                return (row, col as u16);
            }
        }
        let last = breaks.len().saturating_sub(1);
        (last, 0)
    }

    /// Cursor as `(visual_row, column)` for a wrapped layout.
    pub fn wrap_cursor(&self, width: usize) -> (usize, u16) {
        self.wrap_cursor_from_breaks(&self.wrap_breaks(width))
    }

    /// Place the caret from a click on a wrapped row.
    pub fn set_cursor_from_wrap(&mut self, width: usize, row: usize, col: usize) {
        self.clear_selection();
        let breaks = self.wrap_breaks(width);
        let row = row.min(breaks.len() - 1);
        let (start, end) = breaks[row];
        let mut used = 0usize;
        let mut cursor = start;
        for c in &self.chars[start..end] {
            let w = c.width();
            if used + w > col {
                break;
            }
            used += w;
            cursor += 1;
        }
        self.cursor = cursor.min(self.chars.len());
    }

    /// Move the caret up one visual row. Returns false when already on
    /// the first row (caller may leave the block).
    pub fn wrap_up(&mut self, width: usize, prefer_col: u16) -> bool {
        let (row, col) = self.wrap_cursor(width);
        let prefer = if prefer_col == u16::MAX {
            col
        } else {
            prefer_col
        };
        if row == 0 {
            return false;
        }
        self.set_cursor_from_wrap(width, row - 1, prefer as usize);
        true
    }

    /// Move the caret down one visual row. Returns false when already on
    /// the last row (caller may leave the block).
    pub fn wrap_down(&mut self, width: usize, prefer_col: u16) -> bool {
        let (row, col) = self.wrap_cursor(width);
        let prefer = if prefer_col == u16::MAX {
            col
        } else {
            prefer_col
        };
        let height = self.wrap_height(width);
        if row + 1 >= height {
            return false;
        }
        self.set_cursor_from_wrap(width, row + 1, prefer as usize);
        true
    }

    /// Full soft-wrapped view for painting a multi-line field.
    pub fn wrapped(&self, width: usize) -> WrappedView {
        self.wrapped_with_sel(width, self.selection_range())
    }

    /// Like [`Self::wrapped`], but uses an explicit grapheme-range selection
    /// (for description-level multi-line selections).
    pub fn wrapped_with_sel(&self, width: usize, sel: Option<(usize, usize)>) -> WrappedView {
        self.wrapped_from_breaks(&self.wrap_breaks(width), sel)
    }

    /// Soft-wrapped view from precomputed breaks (one wrap pass per paint).
    pub fn wrapped_from_breaks(
        &self,
        breaks: &[(usize, usize)],
        sel: Option<(usize, usize)>,
    ) -> WrappedView {
        let (cursor_row, cursor_col) = self.wrap_cursor_from_breaks(breaks);
        let lines = breaks
            .iter()
            .map(|&(start, end)| {
                let text = self.chars[start..end].concat();
                let sel_cols = sel.and_then(|(lo, hi)| {
                    let vis_lo = lo.max(start);
                    let vis_hi = hi.min(end);
                    if vis_lo >= vis_hi {
                        return None;
                    }
                    let col = |idx: usize| -> u16 {
                        self.chars[start..idx]
                            .iter()
                            .map(|c| c.width())
                            .sum::<usize>() as u16
                    };
                    Some((col(vis_lo), col(vis_hi)))
                });
                WrappedLine {
                    text,
                    sel_cols,
                    start,
                    end,
                }
            })
            .collect();
        WrappedView {
            lines,
            cursor_row: cursor_row as u16,
            cursor_col,
        }
    }

    /// Move the caret and drop this line's own selection: when the description
    /// editor drives the caret, the multi-line selection is its to own.
    pub fn place_cursor(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.chars.len());
        self.sel_anchor = None;
    }

    /// Text to draw in a field `width` columns wide, the cursor's column
    /// within that field, and the selection's column span when it overlaps.
    /// Single-row fields (title, status) use this; description uses [`Self::wrapped`].
    pub fn visible(&mut self, width: usize) -> View {
        if width == 0 {
            return View {
                text: String::new(),
                cursor_col: 0,
                sel_cols: None,
            };
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
        // Scroll forward until the cursor fits; it may rest on the column
        // just past the field, right after the last character typed.
        let mut cursor_width: usize = self.chars[self.scroll..self.cursor]
            .iter()
            .map(|c| c.width())
            .sum();
        while cursor_width > width && self.scroll < self.cursor {
            cursor_width = cursor_width.saturating_sub(self.chars[self.scroll].width());
            self.scroll += 1;
        }
        let mut text = String::new();
        let mut used = 0usize;
        let mut end_idx = self.scroll;
        for c in &self.chars[self.scroll..] {
            let w = c.width();
            if used + w > width {
                break;
            }
            used += w;
            text.push_str(c);
            end_idx += 1;
        }
        let sel_cols = self.selection_range().and_then(|(lo, hi)| {
            let vis_lo = lo.max(self.scroll);
            let vis_hi = hi.min(end_idx);
            if vis_lo >= vis_hi {
                return None;
            }
            let col = |idx: usize| -> u16 {
                self.chars[self.scroll..idx]
                    .iter()
                    .map(|c| c.width())
                    .sum::<usize>() as u16
            };
            Some((col(vis_lo), col(vis_hi)))
        });

        View {
            text,
            cursor_col: cursor_width.min(width) as u16,
            sel_cols,
        }
    }
}

fn bounded_graphemes(value: &str, max_len: usize, max_bytes: usize) -> Vec<String> {
    let mut result = Vec::with_capacity(value.len().min(max_len));
    let mut bytes = 0usize;
    for grapheme in value.graphemes(true).take(max_len) {
        if bytes.saturating_add(grapheme.len()) > max_bytes {
            break;
        }
        bytes += grapheme.len();
        result.push(grapheme.to_string());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(text: &str) -> TextInput {
        TextInput::new(text, 256)
    }

    #[test]
    fn edits_at_the_cursor() {
        let mut i = input("hello");
        i.left();
        i.insert('!');
        assert_eq!(i.value(), "hell!o");
        i.backspace();
        assert_eq!(i.value(), "hello");
        i.delete();
        assert_eq!(i.value(), "hell");
    }

    #[test]
    fn moves_and_deletes_by_word() {
        let mut i = input("one two three");
        i.word_left();
        assert_eq!(i.cursor, 8);
        i.word_left();
        assert_eq!(i.cursor, 4);
        i.delete_word_left();
        assert_eq!(i.value(), "two three");
        i.end();
        i.delete_to_start();
        assert!(i.is_empty());
    }

    #[test]
    fn honours_the_length_limit() {
        let mut i = TextInput::new("ab", 2);
        i.insert('c');
        assert_eq!(i.value(), "ab");
    }

    #[test]
    fn combining_sequences_cannot_bypass_the_byte_limit() {
        let mut typed = TextInput::new("", 1);
        typed.insert('e');
        for _ in 0..1_000 {
            typed.insert('\u{301}');
        }
        assert_eq!(typed.len(), 1);
        assert!(typed.value().len() <= crate::model::text_byte_limit(1));

        let pasted = format!("e{}", "\u{301}".repeat(1_000));
        let pasted = TextInput::new(&pasted, 1);
        assert!(pasted.value().len() <= crate::model::text_byte_limit(1));
    }

    #[test]
    fn appending_inputs_preserves_the_same_limits_as_typing() {
        let mut input = TextInput::new("a", 1);
        assert!(!input.append(&TextInput::new("b", 1)));
        assert_eq!(input.value(), "a");
    }

    #[test]
    fn scrolls_to_keep_the_cursor_in_view() {
        let mut i = input("abcdefghij");
        let view = i.visible(4);
        assert_eq!(view.text, "ghij");
        assert_eq!(view.cursor_col, 4);
        i.home();
        let view = i.visible(4);
        assert_eq!(view.text, "abcd");
        assert_eq!(view.cursor_col, 0);
    }

    #[test]
    fn measures_wide_characters_in_columns() {
        let mut i = input("买菜");
        let view = i.visible(4);
        assert_eq!(view.text, "买菜");
        assert_eq!(view.cursor_col, 4);
    }

    #[test]
    fn cursor_and_deletion_treat_combining_sequences_as_one_grapheme() {
        let mut i = input("e\u{301}x");
        assert_eq!(i.len(), 2);
        i.left();
        i.backspace();
        assert_eq!(i.value(), "x");
        assert_eq!(i.cursor(), 0);
    }

    #[test]
    fn cursor_and_selection_do_not_split_zwj_emoji() {
        let family = "👨‍👩‍👧‍👦";
        let mut i = input(&format!("{family}!"));
        assert_eq!(i.len(), 2);
        i.home();
        i.select_right();
        assert_eq!(i.selected_text().as_deref(), Some(family));
        i.delete_selection();
        assert_eq!(i.value(), "!");
    }

    #[test]
    fn paste_keeps_graphemes_but_strips_terminal_controls() {
        let family = "👨‍👩‍👧‍👦";
        let mut i = TextInput::new("", 64);
        i.insert_str(&format!("e\u{301}\t{family}\u{1b}\u{7f}\u{85}!"));

        assert_eq!(i.value(), format!("e\u{301} {family}!"));
        assert_eq!(i.len(), 4);
        assert!(!i.value().chars().any(char::is_control));
    }

    #[test]
    fn separately_typed_combining_marks_merge_with_the_previous_grapheme() {
        let mut i = TextInput::new("", 1);
        i.insert('e');
        i.insert('\u{301}');

        assert_eq!(i.value(), "e\u{301}");
        assert_eq!(i.len(), 1);
    }

    #[test]
    fn select_word_picks_the_word_under_the_cursor() {
        let mut i = input("one two three");
        i.home();
        i.word_right(); // after "one "
        i.right(); // on 't' of two
        i.select_word();
        assert_eq!(i.selected_text().as_deref(), Some("two"));
        assert_eq!(i.selection_range(), Some((4, 7)));
    }

    #[test]
    fn shift_arrows_extend_the_selection() {
        let mut i = input("hello");
        i.home();
        i.select_right();
        i.select_right();
        assert_eq!(i.selected_text().as_deref(), Some("he"));
        i.delete_selection();
        assert_eq!(i.value(), "llo");
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut i = input("hello");
        i.home();
        i.select_word();
        i.insert('x');
        assert_eq!(i.value(), "x");
    }

    #[test]
    fn wraps_on_spaces_then_hard_breaks() {
        let s: Vec<String> = "one two three"
            .graphemes(true)
            .map(str::to_string)
            .collect();
        let breaks = wrap_breaks(&s, 8);
        let parts: Vec<String> = breaks.iter().map(|&(a, b)| s[a..b].concat()).collect();
        // width 8 fits "one two " then "three"
        assert!(parts.len() >= 2);
        assert!(parts[0].starts_with("one"));
        assert!(parts.iter().any(|p| p.contains("three")));
    }

    #[test]
    fn wrap_ranges_preserve_every_space_and_accept_every_caret_position() {
        let mut input = input("word   next");
        let breaks = input.wrap_breaks(5);
        assert_eq!(breaks.first().map(|range| range.0), Some(0));
        assert_eq!(breaks.last().map(|range| range.1), Some(input.len()));
        assert!(breaks.windows(2).all(|pair| pair[0].1 == pair[1].0));
        assert_eq!(
            breaks
                .iter()
                .map(|&(start, end)| input.slice(start, end))
                .collect::<String>(),
            input.value()
        );

        for cursor in 0..=input.len() {
            input.set_cursor(cursor);
            let _ = input.wrap_cursor_from_breaks(&breaks);
        }
    }

    #[test]
    fn wrap_height_is_at_least_one() {
        let i = input("");
        assert_eq!(i.wrap_height(10), 1);
        let i = input("hello world again");
        assert!(i.wrap_height(6) >= 2);
    }

    #[test]
    fn wrap_cursor_tracks_rows() {
        let mut i = input("aaaa bbbb cccc");
        i.home();
        // width 5 → "aaaa " then "bbbb " then "cccc"
        let (row, col) = i.wrap_cursor(5);
        assert_eq!((row, col), (0, 0));
        i.end();
        let (row, _) = i.wrap_cursor(5);
        assert!(row >= 1);
    }
}
