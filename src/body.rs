//! The body editor: one free-form stack of blocks — prose, bullets,
//! numbered items, links, to-dos and pictures. A bullet is `- ` at the
//! head of a line, a number is `1. `, a picture is a pasted or typed
//! path, and the `/` menu turns a line into a to-do, bullet, number or
//! link, or copies the text out. Backspace at the head of a list item
//! turns it back into prose.

use std::path::{Path, PathBuf};

use unicode_segmentation::UnicodeSegmentation;

use crate::model::{
    Block, MAX_BODY_LINES, MAX_CATEGORY_DESC_LINE_LEN, MAX_CATEGORY_DESC_LINES, MAX_NOTES_LINE_LEN,
};
use crate::text_input::TextInput;

/// How many rows a picture takes in the body, its frame included.
pub const IMAGE_ROWS: u16 = 10;
/// `[ ] ` / `[✓] ` before a subtask (same width open or done).
pub const TODO_INDENT: usize = 4;
/// `• ` before a bullet — shorter than a subtask checkbox.
pub const BULLET_INDENT: usize = 2;
/// `↗ ` before a link URL.
pub const LINK_INDENT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Todo,
    Bullet,
    Number,
    Link,
    /// Copy non-image body text to the clipboard.
    Copy,
    /// Copy a picture from the body to the clipboard.
    CopyImage,
    /// Copy the whole body as HTML (text + embedded pictures).
    CopyAll,
}

/// One line of a "copy all" export, in body order.
#[derive(Debug, Clone)]
pub enum CopyLine {
    Text(String),
    Link(String),
    Image(PathBuf),
}

/// What `/copy` / `/image` / `/copyall` hands back for the clipboard.
#[derive(Debug, Clone)]
pub enum CopyPayload {
    Text(String),
    Image(PathBuf),
    /// Mixed content for HTML + plain-text clipboard.
    All(Vec<CopyLine>),
}

impl Command {
    pub const ALL: [Self; 7] = [
        Self::Todo,
        Self::Bullet,
        Self::Number,
        Self::Link,
        Self::Copy,
        Self::CopyImage,
        Self::CopyAll,
    ];
    /// Categories: bullets and text copy (no to-dos or pictures).
    pub const PLAIN: [Self; 2] = [Self::Bullet, Self::Copy];

    pub fn label(self) -> &'static str {
        match self {
            Self::Todo => "To-do list",
            Self::Bullet => "Bullet point",
            Self::Number => "Numbered list",
            Self::Link => "Link",
            Self::Copy => "Copy text",
            Self::CopyImage => "Copy image",
            Self::CopyAll => "Copy all",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Todo => "tick it off with Ctrl+D",
            Self::Bullet => "or type - and a space",
            Self::Number => "or type 1. and a space",
            Self::Link => "click or ⌘↵ to open",
            Self::Copy => "prose, bullets, to-dos",
            Self::CopyImage => "nearest picture in the body",
            Self::CopyAll => "text and pictures together",
        }
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::Todo => &["todo", "to-do", "task", "check", "box", "list"],
            Self::Bullet => &["bullet", "point", "dash", "item", "list"],
            Self::Number => &["number", "numbered", "ordered", "ol", "1"],
            Self::Link => &["link", "url", "href", "http", "https", "www"],
            Self::Copy => &["copy", "clipboard", "text"],
            Self::CopyImage => &["image", "picture", "pic", "img", "photo"],
            Self::CopyAll => &["copyall", "all", "everything", "rich"],
        }
    }

    fn matches(self, query: &str) -> bool {
        let query = query.to_lowercase();
        query.is_empty() || self.keywords().iter().any(|k| k.starts_with(&query))
    }
}

/// The `/` menu, open while a command is being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashMenu {
    /// Char index of the `/` that opened it, within its block.
    start: usize,
    pub query: String,
    pub index: usize,
}

impl SlashMenu {
    pub fn matches_in(&self, allowed: &[Command]) -> Vec<Command> {
        allowed
            .iter()
            .copied()
            .filter(|c| c.matches(&self.query))
            .collect()
    }

    pub fn selected_in(&self, allowed: &[Command]) -> Option<Command> {
        let matches = self.matches_in(allowed);
        matches
            .get(self.index.min(matches.len().saturating_sub(1)))
            .copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    Text(TextInput),
    Todo { text: TextInput, done: bool },
    Bullet(TextInput),
    Number(TextInput),
    Link(TextInput),
    Image { path: String },
}

impl Line {
    fn input(&mut self) -> Option<&mut TextInput> {
        match self {
            Self::Text(text)
            | Self::Todo { text, .. }
            | Self::Bullet(text)
            | Self::Number(text)
            | Self::Link(text) => Some(text),
            Self::Image { .. } => None,
        }
    }

    fn input_ref(&self) -> Option<&TextInput> {
        match self {
            Self::Text(text)
            | Self::Todo { text, .. }
            | Self::Bullet(text)
            | Self::Number(text)
            | Self::Link(text) => Some(text),
            Self::Image { .. } => None,
        }
    }

    /// A numbered line's prefix depends on its position in the run, so
    /// callers pass that width in themselves; here it counts as zero.
    fn indent(&self) -> usize {
        match self {
            Self::Todo { .. } => TODO_INDENT,
            Self::Bullet(_) => BULLET_INDENT,
            Self::Link(_) => LINK_INDENT,
            Self::Text(_) | Self::Number(_) | Self::Image { .. } => 0,
        }
    }

    fn height(&self, width: usize, number: Option<usize>) -> usize {
        match self {
            Self::Image { .. } => usize::from(IMAGE_ROWS),
            line => {
                let indent = number.map(number_indent).unwrap_or_else(|| line.indent());
                let field = width.saturating_sub(indent).max(1);
                line.input_ref()
                    .map(|t| t.wrap_height(field))
                    .unwrap_or(1)
                    .max(1)
            }
        }
    }
}

/// If `text` is an image path once newlines are removed, return the flat
/// path; otherwise `None` (keep multi-line paste as separate lines).
fn flatten_if_image_path(text: &str, images_root: &std::path::Path) -> Option<String> {
    if !text.contains('\n') && !text.contains('\r') {
        return None;
    }
    let flat: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    let flat = flat.trim();
    crate::image::path_if_image_in(flat, images_root).map(|_| flat.to_string())
}

/// Pull a URL out of a markdown `[label](url)` line, or keep the text.
fn link_url_from_line(s: &str) -> String {
    let s = s.trim();
    if let Some(open) = s.find("](")
        && s.starts_with('[')
        && s.ends_with(')')
        && open + 2 < s.len()
    {
        let url = &s[open + 2..s.len() - 1];
        if !url.is_empty() {
            return url.to_string();
        }
    }
    s.to_string()
}

/// Display width of `n. ` (e.g. `1. ` → 3, `10. ` → 4).
fn number_indent(n: usize) -> usize {
    let n = n.max(1);
    let digits = ((n as f64).log10().floor() as usize) + 1;
    digits + 2
}

/// 1-based index within a run of consecutive numbered lines.
fn number_at(lines: &[Line], at: usize) -> usize {
    let mut n = 0;
    for i in (0..=at).rev() {
        if matches!(lines[i], Line::Number(_)) {
            n += 1;
        } else {
            break;
        }
    }
    n
}

/// Per-line 1-based index in a consecutive numbered run (`None` if not a number).
fn number_runs(lines: &[Line]) -> Vec<Option<usize>> {
    let mut out = Vec::with_capacity(lines.len());
    let mut run = 0usize;
    for line in lines {
        if matches!(line, Line::Number(_)) {
            run += 1;
            out.push(Some(run));
        } else {
            run = 0;
            out.push(None);
        }
    }
    out
}

fn line_from_block(block: &Block, line_max_len: usize) -> Line {
    match block {
        Block::Text { text } => Line::Text(TextInput::new(text, line_max_len)),
        Block::Todo { text, done } => Line::Todo {
            text: TextInput::new(text, line_max_len),
            done: *done,
        },
        Block::Bullet { text } => Line::Bullet(TextInput::new(text, line_max_len)),
        Block::Number { text } => Line::Number(TextInput::new(text, line_max_len)),
        Block::Link { url } => Line::Link(TextInput::new(url, line_max_len)),
        Block::Image { attachment_id } => Line::Image {
            path: attachment_id.clone(),
        },
    }
}

fn block_from_input(input: &TextInput, make: impl FnOnce(&str) -> Block) -> Option<Block> {
    let value = input.value();
    let value = value.trim_end();
    (!value.trim().is_empty()).then(|| make(value))
}

fn resolve_image_reference(
    reference: &str,
    image_root: &Path,
    attachments: &crate::image::AttachmentCatalog,
) -> PathBuf {
    attachments.resolve(reference, image_root)
}

/// Visible slice of a block inside a scrolled viewport: `(y, rows, skip_top)`.
/// `skip_top` is how many of the block's own rows sit above the viewport
/// (for trimming wrap lines / shrinking pictures from the top).
fn visible_band(
    start: usize,
    rows: usize,
    scroll: usize,
    height: u16,
) -> Option<(u16, u16, usize)> {
    if rows == 0 || height == 0 {
        return None;
    }
    let height = usize::from(height);
    let end = start.saturating_add(rows);
    let viewport_end = scroll.saturating_add(height);
    if end <= scroll || start >= viewport_end {
        return None;
    }
    let vis_start = start.max(scroll);
    let vis_end = end.min(viewport_end);
    let y = (vis_start - scroll) as u16;
    let vis_rows = (vis_end - vis_start) as u16;
    let skip = vis_start - start;
    (vis_rows > 0).then_some((y, vis_rows, skip))
}

/// One soft-wrapped visual row of a text-like block.
#[derive(Debug, Clone)]
pub struct WrappedRow {
    pub text: String,
    pub sel: Option<(u16, u16)>,
}

/// What one visible block looks like, for the drawing code.
pub enum Painted {
    /// Soft-wrapped prose / list / link content. `prefix` only paints on
    /// the first visual row; continuation rows are indented to match.
    Text {
        rows: Vec<WrappedRow>,
        kind: TextKind,
    },
    Image(PathBuf),
}

/// How the first row of a wrapped text block is marked.
#[derive(Debug, Clone, Copy)]
pub enum TextKind {
    Plain,
    Todo { done: bool },
    Bullet,
    Number(usize),
    Link,
}

impl TextKind {
    pub fn indent(self) -> usize {
        match self {
            Self::Plain => 0,
            Self::Todo { .. } => TODO_INDENT,
            Self::Bullet => BULLET_INDENT,
            Self::Number(n) => number_indent(n),
            Self::Link => LINK_INDENT,
        }
    }
}

pub struct Placed {
    pub block: Painted,
    /// Index into the body line list (for image hit-testing).
    pub line: usize,
    /// Row of the body box this block starts on, and how tall it is.
    pub y: u16,
    pub rows: u16,
    /// Whether the cursor is on this block. A picture cannot hold a text
    /// cursor, so this is how it shows that it is the one selected.
    pub selected: bool,
}

struct LineLayout {
    number: Option<usize>,
    wraps: Vec<(usize, usize)>,
    start: usize,
    rows: usize,
    selection: Option<(usize, usize)>,
    selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyEditor {
    lines: Vec<Line>,
    cursor: usize,
    scroll: usize,
    pub menu: Option<SlashMenu>,
    /// Prose only: no `/` menu, no pictures. Categories use this.
    plain: bool,
    /// Cap on how many blocks may be added (existing oversize files stay).
    max_lines: usize,
    /// Cap passed to each line's [`crate::text_input::TextInput`].
    line_max_len: usize,
    /// Last body width used for layout (for wrap / click / vertical move).
    layout_width: usize,
    /// Total content rows from the last [`Self::layout`] (for the scrollbar).
    content_height: usize,
    /// Preferred display column when moving up/down across wrap rows.
    prefer_col: u16,
    /// Body-level selection anchor `(line, grapheme)`. Cursor is the other end.
    /// Used for Shift(+Option) motions that can span multiple lines.
    sel_anchor: Option<(usize, usize)>,
    image_root: PathBuf,
    attachments: crate::image::AttachmentCatalog,
}

impl BodyEditor {
    pub fn new(blocks: &[Block]) -> Self {
        Self::from_blocks(blocks, MAX_BODY_LINES, MAX_NOTES_LINE_LEN, false)
    }

    /// A prose editor with bullets: for category descriptions. No to-dos
    /// or pictures — `/` only offers a bullet.
    pub fn plain(text: &str) -> Self {
        let blocks: Vec<Block> = text
            .split('\n')
            .map(|line| match line.strip_prefix("- ") {
                Some(rest) => Block::bullet(rest),
                None => Block::text(line),
            })
            .collect();
        Self::from_blocks(
            &blocks,
            MAX_CATEGORY_DESC_LINES,
            MAX_CATEGORY_DESC_LINE_LEN,
            true,
        )
    }

    fn from_blocks(blocks: &[Block], max_lines: usize, line_max_len: usize, plain: bool) -> Self {
        let mut lines: Vec<Line> = blocks
            .iter()
            .map(|b| line_from_block(b, line_max_len))
            .collect();
        if lines.is_empty() {
            lines.push(Line::Text(TextInput::new("", line_max_len)));
        }
        let mut editor = Self {
            lines,
            cursor: 0,
            scroll: 0,
            menu: None,
            plain,
            max_lines,
            line_max_len,
            layout_width: 40,
            content_height: 0,
            prefer_col: u16::MAX,
            sel_anchor: None,
            image_root: crate::image::default_images_root(),
            attachments: crate::image::AttachmentCatalog::default(),
        };
        // Turn bare image paths in the body into picture blocks.
        if !plain {
            editor.adopt_pasted_paths();
        }
        editor
    }

    fn can_add_lines(&self, n: usize) -> bool {
        self.lines.len().saturating_add(n) <= self.max_lines
    }

    fn line_text_fits(&self, text: &str) -> bool {
        text.len() <= crate::model::text_byte_limit(self.line_max_len)
            && text.graphemes(true).count() <= self.line_max_len
    }

    pub fn set_image_root(&mut self, image_root: PathBuf) {
        self.image_root = image_root;
        if !self.plain {
            self.adopt_pasted_paths();
        }
    }

    pub fn set_attachments(&mut self, attachments: &[crate::store::Attachment]) {
        self.attachments.set(attachments);
    }

    pub fn image_root(&self) -> &std::path::Path {
        &self.image_root
    }

    fn empty_line(&self) -> Line {
        Line::Text(TextInput::new("", self.line_max_len))
    }

    /// Commands the `/` menu may offer in this editor.
    pub fn allowed_commands(&self) -> &'static [Command] {
        if self.plain {
            &Command::PLAIN
        } else {
            &Command::ALL
        }
    }

    /// Filtered slash-menu rows for the open menu, if any.
    pub fn menu_commands(&self) -> Vec<Command> {
        self.menu
            .as_ref()
            .map(|m| m.matches_in(self.allowed_commands()))
            .unwrap_or_default()
    }

    /// The prose back out, one block per line.
    pub fn plain_value(&self) -> String {
        let mut n = 0usize;
        self.value()
            .iter()
            .filter_map(|b| match b {
                Block::Text { text } => {
                    n = 0;
                    Some(text.clone())
                }
                Block::Bullet { text } => {
                    n = 0;
                    Some(format!("- {text}"))
                }
                Block::Number { text } => {
                    n += 1;
                    Some(format!("{n}. {text}"))
                }
                Block::Link { url } => {
                    n = 0;
                    Some(url.clone())
                }
                _ => {
                    n = 0;
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The blocks worth saving. Empty prose lines are layout and round-trip
    /// with surrounding content; an entirely empty editor stays an empty body.
    pub fn value(&self) -> Vec<Block> {
        if self.is_empty() {
            return Vec::new();
        }
        self.lines
            .iter()
            .filter_map(|line| match line {
                Line::Text(text) => Some(Block::text(text.value().trim_end())),
                Line::Todo { text, done } => {
                    block_from_input(text, |value| Block::todo(value, *done))
                }
                Line::Bullet(text) => block_from_input(text, Block::bullet),
                Line::Number(text) => block_from_input(text, Block::number),
                Line::Link(text) => block_from_input(text, Block::link),
                Line::Image { path } => Some(Block::image(path)),
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        !self.lines.iter().any(|l| match l {
            Line::Image { .. } => true,
            Line::Text(t)
            | Line::Bullet(t)
            | Line::Number(t)
            | Line::Link(t)
            | Line::Todo { text: t, .. } => !t.value().trim().is_empty(),
        })
    }

    pub fn progress(&self) -> (usize, usize) {
        let mut done = 0usize;
        let mut total = 0usize;
        for l in &self.lines {
            if let Line::Todo { text, done: d } = l
                && !text.value().trim().is_empty()
            {
                total += 1;
                if *d {
                    done += 1;
                }
            }
        }
        (done, total)
    }

    /// Index of the block under the cursor.
    pub fn cursor_line(&self) -> usize {
        self.cursor
    }

    /// Whether any body or in-line selection is active (no string build).
    pub fn has_selection(&self) -> bool {
        if self.ordered_selection().is_some() {
            return true;
        }
        self.lines[self.cursor]
            .input_ref()
            .is_some_and(|t| t.has_selection())
    }

    /// Selected text: multi-line body selection if active, else the
    /// current line's in-line selection. Pictures become `[image: path]`.
    pub fn selected_text(&self) -> Option<String> {
        if let Some(payload) = self.selected_payload() {
            return match payload {
                CopyPayload::Text(s) => Some(s),
                CopyPayload::All(lines) => {
                    let s = lines
                        .into_iter()
                        .map(|l| match l {
                            CopyLine::Text(t) | CopyLine::Link(t) => t,
                            CopyLine::Image(p) => format!("[image: {}]", p.display()),
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (!s.is_empty()).then_some(s)
                }
                CopyPayload::Image(p) => Some(format!("[image: {}]", p.display())),
            };
        }
        None
    }

    /// Clipboard payload for the current selection (text, picture, or both).
    pub fn selected_payload(&self) -> Option<CopyPayload> {
        if let Some(((al, ac), (bl, bc))) = self.ordered_selection() {
            let lines = self.copy_lines_between(al, ac, bl, bc);
            if lines.is_empty() {
                return None;
            }
            if lines.iter().any(|l| matches!(l, CopyLine::Image(_))) {
                return Some(CopyPayload::All(lines));
            }
            let text = lines
                .into_iter()
                .filter_map(|l| match l {
                    CopyLine::Text(t) | CopyLine::Link(t) => Some(t),
                    CopyLine::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            return (!text.is_empty()).then_some(CopyPayload::Text(text));
        }
        match &self.lines[self.cursor] {
            Line::Text(t)
            | Line::Todo { text: t, .. }
            | Line::Bullet(t)
            | Line::Number(t)
            | Line::Link(t) => t.selected_text().map(CopyPayload::Text),
            Line::Image { path } => Some(CopyPayload::Image(resolve_image_reference(
                path,
                &self.image_root,
                &self.attachments,
            ))),
        }
    }

    fn caret(&self) -> (usize, usize) {
        let col = self.lines[self.cursor]
            .input_ref()
            .map(|i| i.cursor())
            .unwrap_or(0);
        (self.cursor, col)
    }

    fn ordered_selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let a = self.sel_anchor?;
        let b = self.caret();
        if a == b {
            return None;
        }
        Some(if (a.0, a.1) <= (b.0, b.1) {
            (a, b)
        } else {
            (b, a)
        })
    }

    fn line_char_len(&self, i: usize) -> usize {
        self.lines[i].input_ref().map(|t| t.len()).unwrap_or(0)
    }

    fn line_text_value(&self, i: usize) -> String {
        self.lines[i]
            .input_ref()
            .map(|t| t.value())
            .unwrap_or_default()
    }

    fn is_image_line(&self, i: usize) -> bool {
        matches!(self.lines.get(i), Some(Line::Image { .. }))
    }

    /// Whether line `i` sits inside the body selection (for frames / paint).
    /// A picture is selected as a whole unit whenever the range covers it
    /// (including when the caret has only just landed on it).
    pub fn line_in_selection(&self, i: usize) -> bool {
        let Some(((al, _), (bl, _))) = self.ordered_selection() else {
            return false;
        };
        if i < al || i > bl {
            return false;
        }
        if self.is_image_line(i) {
            // Same-line image never forms a range (a == b). Multi-line: whole unit.
            return al < bl;
        }
        self.char_sel_on_line(i).is_some()
    }

    fn copy_lines_between(&self, al: usize, ac: usize, bl: usize, bc: usize) -> Vec<CopyLine> {
        let mut out = Vec::new();
        if al == bl {
            let s = self.line_text_value(al);
            let chars: Vec<&str> = s.graphemes(true).collect();
            let lo = ac.min(chars.len());
            let hi = bc.min(chars.len());
            if lo < hi {
                out.push(CopyLine::Text(chars[lo..hi].concat()));
            }
            return out;
        }
        // Start line: from ac through end (whole picture if it is one).
        if let Some(line) = self.copy_line_slice(al, Some(ac), None) {
            out.push(line);
        }
        for i in (al + 1)..bl {
            if let Some(line) = self.copy_line_slice(i, None, None) {
                out.push(line);
            }
        }
        // End line: start through bc. Picture at the caret is included whole.
        if self.is_image_line(bl) {
            if let Line::Image { path } = &self.lines[bl] {
                out.push(CopyLine::Image(resolve_image_reference(
                    path,
                    &self.image_root,
                    &self.attachments,
                )));
            }
        } else if let Some(line) = self.copy_line_slice(bl, None, Some(bc)) {
            out.push(line);
        }
        out
    }

    /// One export line for copy. `from`/`to` are char bounds on text lines;
    /// `None` means start/end of the line. Empty slices are omitted.
    fn copy_line_slice(
        &self,
        i: usize,
        from: Option<usize>,
        to: Option<usize>,
    ) -> Option<CopyLine> {
        match &self.lines[i] {
            Line::Image { path } => Some(CopyLine::Image(resolve_image_reference(
                path,
                &self.image_root,
                &self.attachments,
            ))),
            Line::Link(t) => {
                let s = t.value();
                let chars: Vec<&str> = s.graphemes(true).collect();
                let lo = from.unwrap_or(0).min(chars.len());
                let hi = to.unwrap_or(chars.len()).min(chars.len());
                (lo < hi).then(|| CopyLine::Link(chars[lo..hi].concat()))
            }
            Line::Text(t) | Line::Bullet(t) | Line::Number(t) | Line::Todo { text: t, .. } => {
                let s = t.value();
                let chars: Vec<&str> = s.graphemes(true).collect();
                let lo = from.unwrap_or(0).min(chars.len());
                let hi = to.unwrap_or(chars.len()).min(chars.len());
                (lo < hi).then(|| CopyLine::Text(chars[lo..hi].concat()))
            }
        }
    }

    /// Char range selected on line `i`, if any (for painting text).
    fn char_sel_on_line(&self, i: usize) -> Option<(usize, usize)> {
        let ((al, ac), (bl, bc)) = self.ordered_selection()?;
        if i < al || i > bl || self.is_image_line(i) {
            return None;
        }
        let len = self.line_char_len(i);
        if al == bl {
            let lo = ac.min(bc).min(len);
            let hi = ac.max(bc).min(len);
            return (lo < hi).then_some((lo, hi));
        }
        if i == al {
            let lo = ac.min(len);
            return (lo < len || len == 0).then_some((lo, len));
        }
        if i == bl {
            let hi = bc.min(len);
            return (hi > 0).then_some((0, hi));
        }
        // Middle line: whole content (empty line still "selected").
        Some((0, len))
    }

    fn ensure_sel_anchor(&mut self) {
        if self.sel_anchor.is_none() {
            self.sel_anchor = Some(self.caret());
        }
    }

    fn clear_body_selection(&mut self) {
        self.sel_anchor = None;
        for line in &mut self.lines {
            if let Some(input) = line.input() {
                input.clear_selection();
            }
        }
    }

    /// Delete body-level or in-line selection. Returns true if anything
    /// was removed.
    pub fn delete_body_selection(&mut self) -> bool {
        if let Some(((al, ac), (bl, bc))) = self.ordered_selection() {
            if al == bl {
                // One line: cut the selected range out and rebuild the
                // line, keeping whatever kind it was.
                if let Some(input) = self.lines[al].input() {
                    let value = input.value();
                    let mut chars: Vec<&str> = value.graphemes(true).collect();
                    let lo = ac.min(bc).min(chars.len());
                    let hi = ac.max(bc).min(chars.len());
                    if lo < hi {
                        chars.drain(lo..hi);
                    }
                    let text = chars.concat();
                    let len = self.line_max_len;
                    self.lines[al] = self.line_with_text(al, &text, len);
                    if let Some(input) = self.lines[al].input() {
                        input.place_cursor(lo);
                    }
                }
            } else {
                // Keep prefix of start + suffix of end. Pictures contribute no
                // text — deleting a range that covers one drops the picture.
                let start_s = if self.is_image_line(al) {
                    String::new()
                } else {
                    self.line_text_value(al)
                };
                let end_s = if self.is_image_line(bl) {
                    String::new()
                } else {
                    self.line_text_value(bl)
                };
                let sc: Vec<&str> = start_s.graphemes(true).collect();
                let ec: Vec<&str> = end_s.graphemes(true).collect();
                let ac = if self.is_image_line(al) {
                    0
                } else {
                    ac.min(sc.len())
                };
                let bc = if self.is_image_line(bl) {
                    0
                } else {
                    bc.min(ec.len())
                };
                let merged = format!("{}{}", sc[..ac].concat(), ec[bc..].concat());
                if !self.line_text_fits(&merged) {
                    return false;
                }
                let len = self.line_max_len;
                self.lines[al] = self.line_with_text(al, &merged, len);
                // Remove lines al+1 ..= bl
                for _ in al..bl {
                    if al + 1 < self.lines.len() {
                        self.lines.remove(al + 1);
                    }
                }
                self.cursor = al;
                if let Some(input) = self.lines[al].input() {
                    input.place_cursor(ac.min(input.len()));
                }
            }
            self.sel_anchor = None;
            return true;
        }
        if let Some(input) = self.input() {
            return input.delete_selection();
        }
        false
    }

    fn line_with_text(&self, index: usize, text: &str, len: usize) -> Line {
        match &self.lines[index] {
            Line::Todo { done, .. } => Line::Todo {
                text: TextInput::new(text, len),
                done: *done,
            },
            Line::Bullet(_) => Line::Bullet(TextInput::new(text, len)),
            Line::Number(_) => Line::Number(TextInput::new(text, len)),
            Line::Link(_) => Line::Link(TextInput::new(text, len)),
            Line::Text(_) | Line::Image { .. } => Line::Text(TextInput::new(text, len)),
        }
    }

    /// URL of the link block under the cursor, if any.
    pub fn link_url_at_cursor(&self) -> Option<String> {
        match &self.lines[self.cursor] {
            Line::Link(t) => {
                let u = t.value();
                let u = u.trim();
                (!u.is_empty()).then(|| u.to_string())
            }
            _ => None,
        }
    }

    /// URL under a rendered body cell. Padding to the right of a short link
    /// is deliberately not interactive.
    pub fn link_url_at_position(&self, row: u16, col: usize) -> Option<String> {
        use unicode_width::UnicodeWidthStr;

        let width = self.layout_width.max(1);
        let numbers = number_runs(&self.lines);
        let target = self.scroll.saturating_add(usize::from(row));
        let mut at = 0usize;
        for (index, line) in self.lines.iter().enumerate() {
            let height = line.height(width, numbers[index]);
            if target >= at.saturating_add(height) {
                at = at.saturating_add(height);
                continue;
            }
            let Line::Link(input) = line else {
                return None;
            };
            let row_in = target.saturating_sub(at);
            let indent = line.indent();
            let field = width.saturating_sub(indent).max(1);
            let breaks = input.wrap_breaks(field);
            let &(start, end) = breaks.get(row_in)?;
            let text = input.slice(start, end);
            let text_width = text.width();
            let on_marker = row_in == 0 && col < indent;
            let on_text = col >= indent && col < indent.saturating_add(text_width);
            if !on_marker && !on_text {
                return None;
            }
            let url = input.value();
            let url = url.trim();
            return (!url.is_empty()).then(|| url.to_string());
        }
        None
    }

    pub fn selected_image(&self) -> Option<PathBuf> {
        match &self.lines[self.cursor] {
            Line::Image { path } => Some(resolve_image_reference(
                path,
                &self.image_root,
                &self.attachments,
            )),
            _ => None,
        }
    }

    /// Every image in the body, so the dialog can preview one.
    pub fn images(&self) -> Vec<PathBuf> {
        self.lines
            .iter()
            .filter_map(|l| match l {
                Line::Image { path } => Some(resolve_image_reference(
                    path,
                    &self.image_root,
                    &self.attachments,
                )),
                _ => None,
            })
            .collect()
    }

    /// Move the cursor off a picture onto a neighbouring text line without
    /// inserting blanks. Used when a click lands on the letterbox gutter.
    /// If there is no editable neighbour, the cursor stays on the picture
    /// (←/→ still create a caret).
    pub fn abandon_image_selection(&mut self) {
        if !matches!(self.lines.get(self.cursor), Some(Line::Image { .. })) {
            return;
        }
        if let Some(next) = self.next_editable(self.cursor) {
            self.cursor = next;
            if let Some(input) = self.input() {
                input.home();
            }
            return;
        }
        if let Some(prev) = self.prev_editable(self.cursor) {
            self.cursor = prev;
            if let Some(input) = self.input() {
                input.end();
            }
        }
    }

    fn line(&mut self) -> &mut Line {
        &mut self.lines[self.cursor]
    }

    fn input(&mut self) -> Option<&mut TextInput> {
        self.lines[self.cursor].input()
    }

    // -------------------------------------------------------------- typing

    pub fn insert(&mut self, c: char) {
        // Typing over a selection replaces it.
        if self.has_selection() && !self.delete_body_selection() {
            return;
        }
        if self.input().is_none() {
            // Typing next to a picture starts a line under it.
            self.insert_block(Block::text(""));
        }
        if let Some(input) = self.input() {
            input.insert(c);
        }
        // Leading "- "/"* " → bullet; "N. " → numbered item.
        if c == ' '
            && let Line::Text(text) = &self.lines[self.cursor]
        {
            let v = text.value();
            if text.cursor() == v.graphemes(true).count() {
                if matches!(v.as_str(), "- " | "* ") {
                    self.lines[self.cursor] = Line::Bullet(TextInput::new("", self.line_max_len));
                    return;
                }
                if let Some(rest) = v.strip_suffix(". ")
                    && !rest.is_empty()
                    && rest.chars().all(|ch| ch.is_ascii_digit())
                {
                    self.lines[self.cursor] = Line::Number(TextInput::new("", self.line_max_len));
                    return;
                }
            }
        }
        if c == '/' {
            let start = self.input().map(|i| i.cursor()).unwrap_or(0);
            self.menu = Some(SlashMenu {
                start,
                query: String::new(),
                index: 0,
            });
        } else if self.menu.is_some() {
            if let Some(menu) = &mut self.menu {
                menu.query.push(c);
                menu.index = 0;
            }
            // No matching command → treat `/` as plain text.
            if self.menu_commands().is_empty() {
                self.close_menu();
            }
        }
        // Convert a complete image path on this line (extension gate inside).
        self.try_adopt_line(self.cursor);
    }

    pub fn insert_str(&mut self, text: &str) {
        self.close_menu();
        if self.has_selection() && !self.delete_body_selection() {
            return;
        }
        // Paste may wrap a long path across lines; flatten if it is one image path.
        let text = match flatten_if_image_path(text, &self.image_root) {
            Some(flat) if self.line_text_fits(&flat) => flat,
            _ => text.to_string(),
        };
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 && !self.newline() {
                break;
            }
            if self.input().is_none() {
                self.insert_block(Block::text(""));
            }
            if let Some(input) = self.input() {
                input.insert_str(part.trim_end_matches('\r'));
            }
        }
        // A pasted path shows its picture straight away — unlike one
        // being typed out, it is complete the moment it arrives.
        self.adopt_pasted_paths();
        if matches!(self.lines[self.cursor], Line::Image { .. }) {
            let next_is_text = matches!(self.lines.get(self.cursor + 1), Some(Line::Text(_)));
            if !next_is_text && self.can_add_lines(1) {
                self.lines.insert(self.cursor + 1, self.empty_line());
            }
            if matches!(self.lines.get(self.cursor + 1), Some(Line::Text(_))) {
                self.cursor += 1;
            }
        }
    }

    /// Enter continues to-do, bullet and numbered lines. An empty list item
    /// returns to plain text; prose, links and pictures still start prose.
    /// Returns false only when adding a line would exceed the line cap.
    pub fn newline(&mut self) -> bool {
        self.close_menu();

        let exits_list = match &self.lines[self.cursor] {
            Line::Todo { text, .. } | Line::Bullet(text) | Line::Number(text) => {
                text.value().trim().is_empty()
            }
            Line::Text(_) | Line::Link(_) | Line::Image { .. } => false,
        };
        if exits_list {
            self.lines[self.cursor] = self.empty_line();
            return true;
        }

        if !self.can_add_lines(1) {
            return false;
        }
        let line_max_len = self.line_max_len;
        let next = match self.line() {
            Line::Text(text) | Line::Link(text) => Line::Text(text.split_off_at_cursor()),
            Line::Todo { text, .. } => Line::Todo {
                text: text.split_off_at_cursor(),
                done: false,
            },
            Line::Bullet(text) => Line::Bullet(text.split_off_at_cursor()),
            Line::Number(text) => Line::Number(text.split_off_at_cursor()),
            Line::Image { .. } => Line::Text(TextInput::new("", line_max_len)),
        };
        self.lines.insert(self.cursor + 1, next);
        self.cursor += 1;
        // Leaving a line may complete a typed image path.
        self.adopt_pasted_paths();
        true
    }

    pub fn backspace(&mut self) {
        if let Some(start) = self.menu.as_ref().map(|m| m.start) {
            let at = self.input().map(|i| i.cursor()).unwrap_or(0);
            if at <= start {
                self.close_menu();
            } else if let Some(menu) = &mut self.menu {
                menu.query.pop();
                menu.index = 0;
            }
        }
        let had_selection = self.has_selection();
        if self.delete_body_selection() {
            return;
        }
        if had_selection {
            return;
        }
        match self.line() {
            // A to-do, bullet, number or link turns back into plain
            // text before it disappears.
            Line::Todo { text, .. }
            | Line::Bullet(text)
            | Line::Number(text)
            | Line::Link(text)
                if text.at_start() =>
            {
                let text = text.clone();
                self.lines[self.cursor] = Line::Text(text);
                return;
            }
            Line::Text(text) if text.at_start() => {}
            Line::Image { .. } => {
                self.remove_block();
                return;
            }
            _ => {
                if let Some(input) = self.input() {
                    input.backspace();
                }
                return;
            }
        }
        // At the start of a text line: fold it into the one above.
        if self.cursor == 0 {
            return;
        }
        let current = self.lines.remove(self.cursor);
        self.cursor -= 1;
        match current {
            Line::Text(text) => {
                let merged = match self.lines[self.cursor].input() {
                    Some(previous) => previous.append(&text),
                    // Above is a picture: an empty spacer can disappear.
                    None => text.is_empty(),
                };
                if !merged {
                    self.cursor += 1;
                    self.lines.insert(self.cursor, Line::Text(text));
                }
            }
            line => {
                self.cursor += 1;
                self.lines.insert(self.cursor, line);
            }
        }
    }

    pub fn delete(&mut self) {
        self.close_menu();
        let had_selection = self.has_selection();
        if self.delete_body_selection() {
            return;
        }
        if had_selection {
            return;
        }
        if matches!(self.line(), Line::Image { .. }) {
            self.remove_block();
            return;
        }
        let at_end = self.input().map(|i| i.at_end()).unwrap_or(true);
        if !at_end {
            if let Some(input) = self.input() {
                input.delete();
            }
            return;
        }
        if self.cursor + 1 >= self.lines.len() {
            return;
        }
        let next = self.lines.remove(self.cursor + 1);
        let merged = match next.input_ref() {
            Some(text) => self.lines[self.cursor]
                .input()
                .is_some_and(|current| current.append(text)),
            None => false,
        };
        if !merged {
            self.lines.insert(self.cursor + 1, next);
        }
    }

    /// Drops the block under the cursor, keeping at least one line.
    pub fn remove_block(&mut self) {
        if self.lines.len() == 1 {
            self.lines[0] = self.empty_line();
            return;
        }
        self.lines.remove(self.cursor);
        self.cursor = self.cursor.min(self.lines.len() - 1);
    }

    pub fn toggle(&mut self) {
        if let Line::Todo { done, .. } = self.line() {
            *done = !*done;
        }
    }

    // ------------------------------------------------------------ movement

    fn field_width_for(&self, index: usize) -> usize {
        let width = self.layout_width.max(1);
        let n = match &self.lines[index] {
            Line::Number(_) => Some(number_at(&self.lines, index)),
            _ => None,
        };
        let indent = n
            .map(number_indent)
            .unwrap_or_else(|| self.lines[index].indent());
        width.saturating_sub(indent).max(1)
    }

    pub fn up(&mut self) {
        self.close_menu();
        self.clear_body_selection();
        // Already on a picture: leave it upward (may insert a blank above).
        if matches!(self.lines[self.cursor], Line::Image { .. }) {
            self.leave_image_backward();
            return;
        }
        let width = self.field_width_for(self.cursor);
        let prefer = self.prefer_col;
        let moved = self
            .input()
            .is_some_and(|input| input.wrap_up(width, prefer));
        if moved {
            if self.prefer_col == u16::MAX
                && let Some(input) = self.input()
            {
                self.prefer_col = input.wrap_cursor(width).1;
            }
            return;
        }
        if self.cursor > 0 {
            let left = self.cursor;
            self.cursor -= 1;
            self.try_adopt_line(left);
            // Landing on a picture selects it (do not skip through).
            if matches!(self.lines[self.cursor], Line::Image { .. }) {
                return;
            }
            let width = self.field_width_for(self.cursor);
            let prefer = if self.prefer_col == u16::MAX {
                0
            } else {
                self.prefer_col
            };
            if let Some(input) = self.input() {
                let last = input.wrap_height(width).saturating_sub(1);
                input.set_cursor_from_wrap(width, last, prefer as usize);
            }
        }
    }

    pub fn down(&mut self) {
        self.close_menu();
        self.clear_body_selection();
        if matches!(self.lines[self.cursor], Line::Image { .. }) {
            self.leave_image_forward();
            return;
        }
        let width = self.field_width_for(self.cursor);
        let prefer = self.prefer_col;
        let moved = self
            .input()
            .is_some_and(|input| input.wrap_down(width, prefer));
        if moved {
            if self.prefer_col == u16::MAX
                && let Some(input) = self.input()
            {
                self.prefer_col = input.wrap_cursor(width).1;
            }
            return;
        }
        if self.cursor + 1 < self.lines.len() {
            let left = self.cursor;
            self.cursor += 1;
            self.try_adopt_line(left);
            // Landing on a picture selects it.
            if matches!(self.lines[self.cursor], Line::Image { .. }) {
                return;
            }
            let width = self.field_width_for(self.cursor);
            let prefer = if self.prefer_col == u16::MAX {
                0
            } else {
                self.prefer_col
            };
            if let Some(input) = self.input() {
                input.set_cursor_from_wrap(width, 0, prefer as usize);
            }
        }
    }

    pub fn left(&mut self) {
        self.close_menu();
        self.prefer_col = u16::MAX;
        self.clear_body_selection();
        // On a picture there is no text caret — ← steps into the line above
        // (creating an empty one when the picture is first).
        if matches!(self.lines[self.cursor], Line::Image { .. }) {
            self.leave_image_backward();
            return;
        }
        match self.input() {
            Some(input) if !input.at_start() => input.left(),
            _ => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    // Landing on a picture selects it.
                    if matches!(self.lines[self.cursor], Line::Image { .. }) {
                        return;
                    }
                    if let Some(input) = self.input() {
                        input.end();
                    }
                }
            }
        }
    }

    pub fn right(&mut self) {
        self.close_menu();
        self.prefer_col = u16::MAX;
        self.clear_body_selection();
        // On a picture there is no text caret — → steps into a line below
        // (creating an empty one when needed) so the user can type again.
        if matches!(self.lines[self.cursor], Line::Image { .. }) {
            self.leave_image_forward();
            return;
        }
        match self.input() {
            Some(input) if !input.at_end() => input.right(),
            _ => {
                if self.cursor + 1 < self.lines.len() {
                    self.cursor += 1;
                    // Landing on a picture selects it.
                    if matches!(self.lines[self.cursor], Line::Image { .. }) {
                        return;
                    }
                    if let Some(input) = self.input() {
                        input.home();
                    }
                }
            }
        }
    }

    /// Move off a selected picture onto the line right under it.
    /// Always inserts a blank text line when the next block is missing or
    /// not editable — works when the picture is the first / only line.
    fn leave_image_forward(&mut self) {
        let next = self.cursor + 1;
        if next < self.lines.len() && self.lines[next].input_ref().is_some() {
            self.cursor = next;
            if let Some(input) = self.input() {
                input.home();
            }
            return;
        }
        if !self.can_add_lines(1) {
            return;
        }
        // Insert immediately under this picture (even if another picture
        // follows — user asked for a caret, not to hop to the next image).
        self.lines.insert(next, self.empty_line());
        self.cursor = next;
    }

    /// Move off a picture onto the line right above it. Inserts a blank
    /// line when the picture is first so ← / ↑ always yield a caret.
    fn leave_image_backward(&mut self) {
        if self.cursor > 0 && self.lines[self.cursor - 1].input_ref().is_some() {
            self.cursor -= 1;
            if let Some(input) = self.input() {
                input.end();
            }
            return;
        }
        if !self.can_add_lines(1) {
            return;
        }
        self.lines.insert(self.cursor, self.empty_line());
        // cursor stays on the new blank line at the same index
        if let Some(input) = self.input() {
            input.home();
        }
    }

    pub fn home(&mut self) {
        self.close_menu();
        self.clear_body_selection();
        if matches!(self.lines[self.cursor], Line::Image { .. }) {
            self.leave_image_backward();
            return;
        }
        if let Some(input) = self.input() {
            input.home();
        }
    }

    pub fn end(&mut self) {
        self.close_menu();
        self.clear_body_selection();
        if matches!(self.lines[self.cursor], Line::Image { .. }) {
            self.leave_image_forward();
            return;
        }
        if let Some(input) = self.input() {
            input.end();
        }
    }

    pub fn word_left(&mut self) {
        self.close_menu();
        self.clear_body_selection();
        self.prefer_col = u16::MAX;
        if matches!(self.lines[self.cursor], Line::Image { .. }) {
            self.leave_image_backward();
            return;
        }
        let at_start = self.input().map(|i| i.at_start()).unwrap_or(true);
        if !at_start {
            if let Some(input) = self.input() {
                input.word_left();
            }
            return;
        }
        // Cross into the previous editable line.
        if let Some(prev) = self.prev_editable(self.cursor) {
            self.cursor = prev;
            if let Some(input) = self.input() {
                input.end();
                input.word_left();
            }
        }
    }

    pub fn word_right(&mut self) {
        self.close_menu();
        self.clear_body_selection();
        self.prefer_col = u16::MAX;
        if matches!(self.lines[self.cursor], Line::Image { .. }) {
            self.leave_image_forward();
            return;
        }
        let at_end = self.input().map(|i| i.at_end()).unwrap_or(true);
        if !at_end {
            if let Some(input) = self.input() {
                input.word_right();
            }
            return;
        }
        if let Some(next) = self.next_editable(self.cursor) {
            self.cursor = next;
            if let Some(input) = self.input() {
                input.home();
                input.word_right();
            }
        }
    }

    fn prev_editable(&self, from: usize) -> Option<usize> {
        (0..from)
            .rev()
            .find(|&i| self.lines[i].input_ref().is_some())
    }

    fn next_editable(&self, from: usize) -> Option<usize> {
        ((from + 1)..self.lines.len()).find(|&i| self.lines[i].input_ref().is_some())
    }

    pub fn select_word(&mut self) {
        self.close_menu();
        self.sel_anchor = None;
        if let Some(input) = self.input() {
            input.select_word();
        }
    }

    pub fn select_left(&mut self) {
        self.close_menu();
        self.prefer_col = u16::MAX;
        self.ensure_sel_anchor();
        if let Some(input) = self.input() {
            input.clear_selection();
        }
        // Pictures are one unit: ← leaves them for the previous line.
        if self.is_image_line(self.cursor) {
            self.step_sel_prev_line();
            return;
        }
        let at_start = self.input().map(|i| i.at_start()).unwrap_or(true);
        if !at_start {
            if let Some(input) = self.input() {
                let c = input.cursor().saturating_sub(1);
                input.place_cursor(c);
            }
        } else {
            self.step_sel_prev_line();
        }
    }

    pub fn select_right(&mut self) {
        self.close_menu();
        self.prefer_col = u16::MAX;
        self.ensure_sel_anchor();
        if let Some(input) = self.input() {
            input.clear_selection();
        }
        if self.is_image_line(self.cursor) {
            self.step_sel_next_line();
            return;
        }
        let at_end = self.input().map(|i| i.at_end()).unwrap_or(true);
        if !at_end {
            if let Some(input) = self.input() {
                let c = (input.cursor() + 1).min(input.len());
                input.place_cursor(c);
            }
        } else {
            self.step_sel_next_line();
        }
    }

    /// Shift+Option+← — extend selection by a word, crossing lines and pictures.
    pub fn select_word_left(&mut self) {
        self.close_menu();
        self.prefer_col = u16::MAX;
        self.ensure_sel_anchor();
        if let Some(input) = self.input() {
            input.clear_selection();
        }
        // A picture counts as one word.
        if self.is_image_line(self.cursor) {
            self.step_sel_prev_line();
            if !self.is_image_line(self.cursor)
                && let Some(input) = self.input()
            {
                input.end();
                let target = input.word_left_index();
                input.place_cursor(target);
            }
            return;
        }
        let at_start = self.input().map(|i| i.at_start()).unwrap_or(true);
        if !at_start {
            if let Some(input) = self.input() {
                let target = input.word_left_index();
                input.place_cursor(target);
            }
            return;
        }
        self.step_sel_prev_line();
        if self.is_image_line(self.cursor) {
            return;
        }
        if let Some(input) = self.input() {
            input.end();
            let target = input.word_left_index();
            input.place_cursor(target);
        }
    }

    /// Shift+Option+→ — extend selection by a word, crossing lines and pictures.
    pub fn select_word_right(&mut self) {
        self.close_menu();
        self.prefer_col = u16::MAX;
        self.ensure_sel_anchor();
        if let Some(input) = self.input() {
            input.clear_selection();
        }
        if self.is_image_line(self.cursor) {
            self.step_sel_next_line();
            if !self.is_image_line(self.cursor)
                && let Some(input) = self.input()
            {
                input.home();
                let target = input.word_right_index();
                input.place_cursor(target);
            }
            return;
        }
        let at_end = self.input().map(|i| i.at_end()).unwrap_or(true);
        if !at_end {
            if let Some(input) = self.input() {
                let target = input.word_right_index();
                input.place_cursor(target);
            }
            return;
        }
        self.step_sel_next_line();
        if self.is_image_line(self.cursor) {
            return;
        }
        if let Some(input) = self.input() {
            input.home();
            let target = input.word_right_index();
            input.place_cursor(target);
        }
    }

    /// Move the selection caret onto the previous line (pictures included).
    /// Does not insert blank lines — unlike plain ← on a picture.
    fn step_sel_prev_line(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        if let Some(input) = self.input() {
            input.end();
        }
    }

    /// Move the selection caret onto the next line (pictures included).
    fn step_sel_next_line(&mut self) {
        if self.cursor + 1 >= self.lines.len() {
            return;
        }
        self.cursor += 1;
        if let Some(input) = self.input() {
            input.home();
        }
    }

    pub fn select_home(&mut self) {
        self.close_menu();
        self.ensure_sel_anchor();
        if let Some(input) = self.input() {
            input.clear_selection();
            input.place_cursor(0);
        }
    }

    pub fn select_end(&mut self) {
        self.close_menu();
        self.ensure_sel_anchor();
        if let Some(input) = self.input() {
            input.clear_selection();
            let n = input.len();
            input.place_cursor(n);
        }
    }

    pub fn delete_to_start(&mut self) {
        if let Some(input) = self.input() {
            input.delete_to_start();
        }
    }

    pub fn delete_to_end(&mut self) {
        if let Some(input) = self.input() {
            input.delete_to_end();
        }
    }

    pub fn delete_word_left(&mut self) {
        if let Some(input) = self.input() {
            input.delete_word_left();
        }
    }

    // ---------------------------------------------------------- slash menu

    pub fn menu_next(&mut self) {
        let count = self.menu_commands().len();
        if let Some(menu) = &mut self.menu
            && count > 0
        {
            menu.index = (menu.index + 1) % count;
        }
    }

    pub fn menu_prev(&mut self) {
        let count = self.menu_commands().len();
        if let Some(menu) = &mut self.menu
            && count > 0
        {
            menu.index = (menu.index + count - 1) % count;
        }
    }

    pub fn close_menu(&mut self) {
        self.menu = None;
    }

    /// The command under the cursor in the open menu, if any.
    pub fn menu_selected(&self) -> Option<Command> {
        self.menu
            .as_ref()
            .and_then(|m| m.selected_in(self.allowed_commands()))
    }

    /// Plain-text export of every non-image block, for `/copy`.
    pub fn text_for_copy(&self) -> String {
        self.lines_for_copy_all()
            .into_iter()
            .filter_map(|line| match line {
                CopyLine::Text(s) | CopyLine::Link(s) => Some(s),
                CopyLine::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Full body in order for `/copyall` — text lines and pictures.
    pub fn lines_for_copy_all(&self) -> Vec<CopyLine> {
        self.lines
            .iter()
            .enumerate()
            .filter_map(|(i, line)| match line {
                Line::Text(text) => {
                    let s = text.value();
                    (!s.trim().is_empty()).then_some(CopyLine::Text(s))
                }
                Line::Bullet(text) => Some(CopyLine::Text(format!("- {}", text.value()))),
                Line::Number(text) => {
                    let n = number_at(&self.lines, i);
                    Some(CopyLine::Text(format!("{n}. {}", text.value())))
                }
                Line::Todo { text, done } => {
                    let mark = if *done { "[✓]" } else { "[ ]" };
                    Some(CopyLine::Text(format!("{mark} {}", text.value())))
                }
                Line::Link(text) => {
                    let s = text.value();
                    (!s.trim().is_empty()).then_some(CopyLine::Link(s))
                }
                Line::Image { path } => Some(CopyLine::Image(resolve_image_reference(
                    path,
                    &self.image_root,
                    &self.attachments,
                ))),
            })
            .collect()
    }

    /// Picture nearest the cursor: search upward first, then downward.
    pub fn image_for_copy(&self) -> Option<PathBuf> {
        for i in (0..=self.cursor).rev() {
            if let Line::Image { path } = &self.lines[i] {
                return Some(resolve_image_reference(
                    path,
                    &self.image_root,
                    &self.attachments,
                ));
            }
        }
        for i in (self.cursor + 1)..self.lines.len() {
            if let Line::Image { path } = &self.lines[i] {
                return Some(resolve_image_reference(
                    path,
                    &self.image_root,
                    &self.attachments,
                ));
            }
        }
        None
    }

    /// Removes the typed `/command` and applies it. Returns clipboard
    /// payload for copy commands.
    pub fn apply(&mut self, command: Command) -> Option<CopyPayload> {
        if !self.allowed_commands().contains(&command) {
            self.close_menu();
            return None;
        }
        let menu = self.menu.take()?;
        if let Some(input) = self.input() {
            // Cut away the `/query` that was typed.
            let end = input.cursor();
            input.set_cursor(end);
            for _ in menu.start.saturating_sub(1)..end {
                input.backspace();
            }
        }
        let payload = match command {
            Command::Copy => Some(CopyPayload::Text(self.text_for_copy())),
            Command::CopyImage => self.image_for_copy().map(CopyPayload::Image),
            Command::CopyAll => Some(CopyPayload::All(self.lines_for_copy_all())),
            Command::Todo | Command::Bullet | Command::Number | Command::Link => {
                let text = match self.line() {
                    Line::Text(text)
                    | Line::Todo { text, .. }
                    | Line::Bullet(text)
                    | Line::Number(text)
                    | Line::Link(text) => text.clone(),
                    Line::Image { .. } => return None,
                };
                self.lines[self.cursor] = match command {
                    Command::Todo => Line::Todo { text, done: false },
                    Command::Bullet => Line::Bullet(text),
                    Command::Number => Line::Number(text),
                    Command::Link => {
                        let url = link_url_from_line(&text.value());
                        Line::Link(TextInput::new(&url, self.line_max_len))
                    }
                    Command::Copy | Command::CopyImage | Command::CopyAll => return None,
                };
                None
            }
        };
        if matches!(
            command,
            Command::Copy | Command::CopyImage | Command::CopyAll
        ) && self.input().is_some_and(|input| input.is_empty())
        {
            self.remove_block();
        }
        payload
    }

    /// Puts a block in at the cursor, replacing the line when it is an
    /// empty one and pushing it down otherwise.
    pub fn insert_block(&mut self, block: Block) {
        self.close_menu();
        let is_image = matches!(block, Block::Image { .. });
        let replace = match &self.lines[self.cursor] {
            Line::Text(text) => text.is_empty(),
            _ => false,
        };
        let extra = match (replace, is_image) {
            (true, true) => 1, // image replaces empty, then a blank line under it
            (true, false) => 0,
            (false, true) => 2, // image + blank line
            (false, false) => 1,
        };
        if extra > 0 && !self.can_add_lines(extra) {
            return;
        }
        let line = line_from_block(&block, self.line_max_len);
        if replace {
            self.lines[self.cursor] = line;
        } else {
            self.lines.insert(self.cursor + 1, line);
            self.cursor += 1;
        }
        // A picture is not editable, so leave a line under it to type on.
        if is_image {
            self.lines.insert(self.cursor + 1, self.empty_line());
            self.cursor += 1;
        }
    }

    /// Turn bare image-file paths into image blocks (paste / leave-line).
    fn adopt_pasted_paths(&mut self) {
        if self.plain {
            return;
        }
        self.merge_broken_image_paths();
        for i in 0..self.lines.len() {
            self.try_adopt_line(i);
        }
    }

    fn try_adopt_line(&mut self, i: usize) {
        let Line::Text(text) = &self.lines[i] else {
            return;
        };
        let value = text.value();
        if !crate::image::looks_like_image(&value) {
            return;
        }
        if let Some(path) = crate::image::path_if_image_in(&value, &self.image_root) {
            self.lines[i] = Line::Image {
                path: crate::image::short_in(&path, &self.image_root),
            };
        }
    }

    /// If line *i* + line *i+1* form an existing image path when joined,
    /// fold them into one text line (for the next convert pass).
    fn merge_broken_image_paths(&mut self) {
        let mut i = 0;
        while i + 1 < self.lines.len() {
            let joined = match (&self.lines[i], &self.lines[i + 1]) {
                (Line::Text(a), Line::Text(b)) => {
                    let left = a.value();
                    let right = b.value();
                    let joined = format!("{left}{right}");
                    // Only glue when the first piece looks like a path
                    // fragment (no image ext yet) and the second finishes it.
                    if left.contains('/')
                        && !crate::image::looks_like_image(&left)
                        && self.line_text_fits(&joined)
                        && crate::image::path_if_image_in(&joined, &self.image_root).is_some()
                    {
                        Some(joined)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(path) = joined {
                let len = self.line_max_len;
                self.lines[i] = Line::Text(TextInput::new(&path, len));
                self.lines.remove(i + 1);
                if self.cursor > i {
                    self.cursor -= 1;
                }
                // Don't advance — the merged line may convert next.
            } else {
                i += 1;
            }
        }
    }

    // ------------------------------------------------------------ painting

    /// Lays the blocks out in a `width` x `height` box and scrolls so the
    /// cursor stays in view. Returns the visible blocks and where the
    /// text cursor sits, if it is on an editable line.
    pub fn layout(&mut self, width: usize, height: u16) -> (Vec<Placed>, Option<(u16, u16)>) {
        if height == 0 || width == 0 {
            return (Vec::new(), None);
        }
        self.layout_width = width;

        let numbers = number_runs(&self.lines);
        let mut total = 0usize;
        let layouts: Vec<LineLayout> = self
            .lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let number = numbers[i];
                let indent = number.map(number_indent).unwrap_or_else(|| line.indent());
                let field = width.saturating_sub(indent).max(1);
                let wraps = line
                    .input_ref()
                    .map(|text| text.wrap_breaks(field))
                    .unwrap_or_default();
                let rows = if matches!(line, Line::Image { .. }) {
                    usize::from(IMAGE_ROWS)
                } else {
                    wraps.len().max(1)
                };
                let layout = LineLayout {
                    number,
                    wraps,
                    start: total,
                    rows,
                    selection: self
                        .char_sel_on_line(i)
                        .or_else(|| line.input_ref().and_then(TextInput::selection_range)),
                    selected: i == self.cursor || self.line_in_selection(i),
                };
                total = total.saturating_add(rows);
                layout
            })
            .collect();
        self.content_height = total;

        // Keep the caret's visual row on screen (not just the block).
        let cursor_visual = {
            let layout = &layouts[self.cursor];
            let row_in_block = self.lines[self.cursor]
                .input_ref()
                .map(|text| text.wrap_cursor_from_breaks(&layout.wraps).0)
                .unwrap_or(0);
            layout.start.saturating_add(row_in_block)
        };
        if cursor_visual < self.scroll {
            self.scroll = cursor_visual;
        } else if cursor_visual >= self.scroll.saturating_add(usize::from(height)) {
            self.scroll = cursor_visual + 1 - usize::from(height);
        }
        self.scroll = self.scroll.min(total.saturating_sub(usize::from(height)));

        let mut placed = Vec::new();
        let mut cursor_at = None;
        for (i, (line, layout)) in self.lines.iter_mut().zip(&layouts).enumerate() {
            // Intersection with the viewport — clip top and bottom the same
            // way so a tall block (picture) shrinks until it disappears when
            // scrolled off either edge, instead of painting full-height at y=0
            // and overlapping the next block.
            let Some((y, vis_rows, skip)) =
                visible_band(layout.start, layout.rows, self.scroll, height)
            else {
                continue;
            };
            let (text, kind) = match line {
                Line::Todo { text, done } => (text, TextKind::Todo { done: *done }),
                Line::Bullet(text) => (text, TextKind::Bullet),
                Line::Number(text) => (text, TextKind::Number(layout.number.unwrap_or(1))),
                Line::Link(text) => (text, TextKind::Link),
                Line::Text(text) => (text, TextKind::Plain),
                Line::Image { path } => {
                    placed.push(Placed {
                        block: Painted::Image(resolve_image_reference(
                            path,
                            &self.image_root,
                            &self.attachments,
                        )),
                        line: i,
                        y,
                        rows: vis_rows,
                        selected: layout.selected,
                    });
                    continue;
                }
            };
            let indent = kind.indent();
            let view = text.wrapped_from_breaks(&layout.wraps, layout.selection);
            if i == self.cursor {
                let row = usize::from(view.cursor_row).saturating_sub(skip) as u16;
                cursor_at = Some((y.saturating_add(row), view.cursor_col + indent as u16));
            }
            let wrap_rows: Vec<WrappedRow> = view
                .lines
                .into_iter()
                .skip(skip)
                .take(vis_rows as usize)
                .map(|l| WrappedRow {
                    text: l.text,
                    sel: l.sel_cols,
                })
                .collect();
            placed.push(Placed {
                block: Painted::Text {
                    rows: wrap_rows,
                    kind,
                },
                line: i,
                y,
                rows: vis_rows,
                selected: layout.selected,
            });
        }
        (placed, cursor_at)
    }

    /// Body scroll offset after the last [`Self::layout`] call.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Total laid-out rows after the last [`Self::layout`] call (scrollbar).
    pub fn content_height(&self) -> usize {
        self.content_height
    }

    /// Moves the cursor to a clicked cell of the body box.
    /// Returns `true` when the click landed on a real block (not empty
    /// padding below the content).
    pub fn click(&mut self, row: u16, col: usize) -> bool {
        let width = self.layout_width.max(1);
        let numbers = number_runs(&self.lines);
        let target = self.scroll.saturating_add(usize::from(row));
        let mut at = 0usize;
        let mut hit = false;
        for (i, line) in self.lines.iter().enumerate() {
            let h = line.height(width, numbers[i]);
            if target < at.saturating_add(h) {
                self.cursor = i;
                let row_in = target.saturating_sub(at);
                let indent = numbers[i]
                    .map(number_indent)
                    .unwrap_or_else(|| line.indent());
                let field = width.saturating_sub(indent).max(1);
                if let Some(input) = self.lines[i].input() {
                    input.set_cursor_from_wrap(field, row_in, col.saturating_sub(indent));
                }
                hit = true;
                break;
            }
            at = at.saturating_add(h);
        }
        self.close_menu();
        self.prefer_col = u16::MAX;
        self.clear_body_selection();
        hit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(blocks: &[Block]) -> BodyEditor {
        BodyEditor::new(blocks)
    }

    fn type_in(editor: &mut BodyEditor, text: &str) {
        for c in text.chars() {
            editor.insert(c);
        }
    }

    #[test]
    fn types_prose_and_splits_lines() {
        let mut e = editor(&[]);
        type_in(&mut e, "first");
        e.newline();
        type_in(&mut e, "second");
        assert_eq!(e.value(), vec![Block::text("first"), Block::text("second")]);
    }

    #[test]
    fn body_value_round_trips_blank_rows_around_content() {
        let mut e = editor(&[]);
        assert!(e.newline());
        type_in(&mut e, "first");
        assert!(e.newline());
        assert!(e.newline());
        type_in(&mut e, "second");
        assert!(e.newline());

        let expected = vec![
            Block::text(""),
            Block::text("first"),
            Block::text(""),
            Block::text("second"),
            Block::text(""),
        ];
        assert_eq!(e.value(), expected);
        assert_eq!(editor(&e.value()).value(), expected);
        assert!(editor(&[]).value().is_empty());
    }

    #[test]
    fn refuses_more_lines_than_the_cap() {
        let mut e = BodyEditor::from_blocks(&[], 2, 32, false);
        type_in(&mut e, "a");
        assert!(e.newline());
        type_in(&mut e, "b");
        assert!(!e.newline(), "third line blocked");
        assert_eq!(e.value().len(), 2);
    }

    #[test]
    fn plain_description_uses_shorter_line_cap() {
        let mut e = BodyEditor::plain("");
        type_in(&mut e, &"x".repeat(MAX_CATEGORY_DESC_LINE_LEN + 10));
        assert_eq!(
            e.value()[0],
            Block::text(&"x".repeat(MAX_CATEGORY_DESC_LINE_LEN))
        );
    }

    #[test]
    fn slash_bullet_turns_the_line_into_a_point() {
        let mut e = editor(&[]);
        type_in(&mut e, "/bul");
        e.apply(Command::Bullet);
        type_in(&mut e, "a point");
        assert_eq!(e.value(), vec![Block::bullet("a point")]);
    }

    #[test]
    fn slash_number_turns_the_line_into_a_list_item() {
        let mut e = editor(&[]);
        type_in(&mut e, "/num");
        assert_eq!(e.menu_selected(), Some(Command::Number));
        e.apply(Command::Number);
        type_in(&mut e, "first");
        e.newline();
        type_in(&mut e, "second");
        assert_eq!(
            e.value(),
            vec![Block::number("first"), Block::number("second")]
        );
        assert_eq!(e.text_for_copy(), "1. first\n2. second");
    }

    #[test]
    fn typing_1_dot_space_makes_a_numbered_item() {
        let mut e = editor(&[]);
        type_in(&mut e, "1. ");
        type_in(&mut e, "alpha");
        assert_eq!(e.value(), vec![Block::number("alpha")]);
    }

    #[test]
    fn slash_link_turns_the_line_into_a_url() {
        let mut e = editor(&[Block::text("https://example.com")]);
        e.end();
        type_in(&mut e, "/link");
        assert_eq!(e.menu_selected(), Some(Command::Link));
        e.apply(Command::Link);
        assert_eq!(e.value(), vec![Block::link("https://example.com")]);
    }

    #[test]
    fn slash_link_unwraps_markdown() {
        let mut e = editor(&[Block::text("[docs](https://example.com/docs)")]);
        e.end();
        type_in(&mut e, "/link");
        e.apply(Command::Link);
        assert_eq!(e.value(), vec![Block::link("https://example.com/docs")]);
    }

    #[test]
    fn slash_link_on_empty_line_is_ready_for_a_url() {
        let mut e = editor(&[]);
        type_in(&mut e, "/link");
        e.apply(Command::Link);
        type_in(&mut e, "https://x.ai");
        assert_eq!(e.value(), vec![Block::link("https://x.ai")]);
    }

    #[test]
    fn slash_todo_turns_the_line_into_a_task() {
        let mut e = editor(&[]);
        type_in(&mut e, "/todo");
        let menu = e.menu.as_ref().expect("menu is open");
        assert_eq!(menu.query, "todo");
        assert_eq!(e.menu_selected(), Some(Command::Todo));
        e.apply(Command::Todo);
        type_in(&mut e, "buy milk");
        assert_eq!(e.value(), vec![Block::todo("buy milk", false)]);
        assert!(e.menu.is_none());
    }

    #[test]
    fn enter_in_a_todo_starts_an_unchecked_todo() {
        let mut e = editor(&[Block::todo("one", true)]);
        e.end();
        e.newline();
        type_in(&mut e, "two");
        assert_eq!(
            e.value(),
            vec![Block::todo("one", true), Block::todo("two", false)]
        );
    }

    #[test]
    fn enter_in_a_bullet_continues_the_list() {
        let mut e = editor(&[Block::bullet("first")]);
        e.end();
        e.newline();
        type_in(&mut e, "second");

        assert_eq!(
            e.value(),
            vec![Block::bullet("first"), Block::bullet("second")]
        );
    }

    #[test]
    fn enter_splits_a_numbered_item_into_the_continued_list() {
        let mut e = editor(&[Block::number("firstsecond")]);
        e.home();
        for _ in 0..5 {
            e.right();
        }
        e.newline();

        assert_eq!(
            e.value(),
            vec![Block::number("first"), Block::number("second")]
        );
        assert_eq!(e.text_for_copy(), "1. first\n2. second");
    }

    #[test]
    fn enter_on_an_empty_list_item_returns_to_plain_text() {
        let mut e = editor(&[]);
        type_in(&mut e, "- first");
        e.newline();
        e.newline();
        type_in(&mut e, "plain");

        assert_eq!(
            e.value(),
            vec![Block::bullet("first"), Block::text("plain")]
        );
    }

    #[test]
    fn backspace_at_the_start_unmakes_a_todo() {
        let mut e = editor(&[Block::todo("one", false)]);
        e.home();
        e.backspace();
        assert_eq!(e.value(), vec![Block::text("one")]);
    }

    #[test]
    fn moving_the_cursor_closes_the_body_command_menu() {
        let mut editor = BodyEditor::new(&[]);
        for c in "/todo".chars() {
            editor.insert(c);
        }
        assert!(editor.menu.is_some());

        editor.left();

        assert!(
            editor.menu.is_none(),
            "the cached query must never outlive its caret range"
        );
        assert_eq!(editor.value(), vec![Block::text("/todo")]);
    }

    #[test]
    fn link_hit_testing_excludes_blank_row_padding() {
        let mut editor = BodyEditor::new(&[Block::link("https://example.com")]);
        let _ = editor.layout(40, 4);

        assert_eq!(
            editor.link_url_at_position(0, 3).as_deref(),
            Some("https://example.com")
        );
        assert_eq!(editor.link_url_at_position(0, 39), None);
    }

    #[test]
    fn toggling_counts_towards_progress() {
        let mut e = editor(&[Block::todo("a", false), Block::todo("b", false)]);
        assert_eq!(e.progress(), (0, 2));
        e.toggle();
        assert_eq!(e.progress(), (1, 2));
    }

    #[test]
    fn an_image_lands_between_the_lines_with_room_to_type() {
        let mut e = editor(&[]);
        type_in(&mut e, "before");
        e.newline();
        e.insert_block(Block::image("/tmp/a.png"));
        type_in(&mut e, "after");
        assert_eq!(
            e.value(),
            vec![
                Block::text("before"),
                Block::image("/tmp/a.png"),
                Block::text("after")
            ]
        );
        assert_eq!(e.images().len(), 1);
    }

    #[test]
    fn a_path_in_the_body_becomes_a_picture() {
        // A real file, so the check that it is readable passes.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/screenshot.png");
        let e = editor(&[Block::text(path), Block::text("below")]);
        assert!(matches!(e.value()[0], Block::Image { .. }));
        assert_eq!(e.value()[1], Block::text("below"));
    }

    #[test]
    fn a_path_to_nothing_stays_text() {
        let e = editor(&[Block::text("/tmp/not-here-at-all.png")]);
        assert!(matches!(e.value()[0], Block::Text { .. }));
    }

    #[test]
    fn a_pasted_path_is_a_picture_at_once() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/screenshot.png");
        let mut e = editor(&[]);
        e.insert_str(path);
        assert!(
            matches!(e.value()[0], Block::Image { .. }),
            "no need to move the cursor off it first"
        );
        type_in(&mut e, "and on we go");
        assert_eq!(e.value()[1], Block::text("and on we go"));
    }

    #[test]
    fn a_complete_path_becomes_a_picture_even_under_the_cursor() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/screenshot.png");
        let mut e = editor(&[]);
        type_in(&mut e, path);
        e.layout(40, 20);
        assert!(
            matches!(e.value()[0], Block::Image { .. }),
            "complete existing path converts without leaving the line"
        );
    }

    #[test]
    fn shift_option_left_selects_across_lines() {
        let mut e = editor(&[Block::text("one two"), Block::text("three four")]);
        e.down();
        e.end(); // on "four"
        e.select_word_left(); // select "four"
        assert_eq!(e.selected_text().as_deref(), Some("four"));
        e.select_word_left(); // "three "
        e.select_word_left(); // cross into previous line
        let sel = e.selected_text().expect("cross-line selection");
        assert!(
            sel.contains("two") && sel.contains("three"),
            "expected multi-line sel, got {sel:?}"
        );
    }

    #[test]
    fn shift_selection_includes_pictures_with_text() {
        let path = "/tmp/shot.png";
        let mut e = editor(&[
            Block::text("above"),
            Block::image(path),
            Block::text("below here"),
        ]);
        // Caret at start of "below here".
        e.down();
        e.down();
        e.home();
        e.select_word_left(); // onto the picture
        assert!(e.line_in_selection(1), "picture covered by selection");
        assert!(
            matches!(e.selected_payload(), Some(CopyPayload::All(_))),
            "mixed selection is rich copy"
        );
        e.select_word_left(); // into "above"
        let sel = e.selected_text().expect("text+image selection");
        assert!(
            sel.contains("above") && sel.contains("[image:") && !sel.contains("below"),
            "got {sel:?}"
        );
        // Layout marks the picture selected for its frame.
        let (placed, _) = e.layout(40, 40);
        let img = placed
            .iter()
            .find(|p| matches!(p.block, Painted::Image(_)))
            .expect("image placed");
        assert!(img.selected, "outer frame while selection covers image");
    }

    #[test]
    fn a_pasted_path_broken_by_newlines_still_becomes_a_picture() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/screenshot.png");
        // Simulate a clipboard soft-break in the middle of the path.
        let mid = path.len() / 2;
        let broken = format!("{}\n{}", &path[..mid], &path[mid..]);
        let mut e = editor(&[]);
        e.insert_str(&broken);
        assert!(
            matches!(e.value()[0], Block::Image { .. }),
            "flattened paste: {broken:?} → {:?}",
            e.value()
        );
    }

    #[test]
    fn two_text_lines_that_form_a_path_are_merged() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/screenshot.png");
        let mid = path.len() / 2;
        let e = editor(&[Block::text(&path[..mid]), Block::text(&path[mid..])]);
        assert!(
            matches!(e.value()[0], Block::Image { .. }),
            "split lines rejoin: {:?}",
            e.value()
        );
    }

    #[test]
    fn dash_and_a_space_make_a_bullet() {
        let mut e = editor(&[]);
        type_in(&mut e, "- buy milk");
        assert_eq!(e.value(), vec![Block::bullet("buy milk")]);
    }

    #[test]
    fn a_dash_mid_line_is_just_a_dash() {
        let mut e = editor(&[]);
        type_in(&mut e, "a - b");
        assert_eq!(e.value(), vec![Block::text("a - b")]);
    }

    #[test]
    fn backspace_at_the_start_unmakes_a_bullet() {
        let mut e = editor(&[Block::bullet("one")]);
        e.home();
        e.backspace();
        assert_eq!(e.value(), vec![Block::text("one")]);
    }

    #[test]
    fn plain_text_round_trips_its_bullets() {
        let e = BodyEditor::plain("intro\n- first\n- second");
        assert_eq!(e.value().len(), 3);
        assert_eq!(e.plain_value(), "intro\n- first\n- second");
    }

    #[test]
    fn plain_description_round_trips_blank_rows() {
        let description = "\nfirst\n\nsecond\n";
        let e = BodyEditor::plain(description);

        assert_eq!(e.plain_value(), description);
        assert_eq!(
            BodyEditor::plain(&e.plain_value()).plain_value(),
            description
        );
    }

    #[test]
    fn plain_mode_rejects_todo_slash_command() {
        let mut e = BodyEditor::plain("");
        type_in(&mut e, "/todo");
        assert!(e.menu.is_none(), "no to-do command in plain mode");
        assert_eq!(e.value(), vec![Block::text("/todo")]);
    }

    #[test]
    fn plain_mode_slash_makes_a_bullet() {
        let mut e = BodyEditor::plain("");
        type_in(&mut e, "/bul");
        assert_eq!(e.menu_selected(), Some(Command::Bullet));
        e.apply(Command::Bullet);
        type_in(&mut e, "a point");
        assert_eq!(e.plain_value(), "- a point");
    }

    #[test]
    fn copy_exports_text_and_skips_images() {
        let e = editor(&[
            Block::text("hello"),
            Block::todo("one", true),
            Block::image("/tmp/a.png"),
            Block::bullet("point"),
        ]);
        assert_eq!(e.text_for_copy(), "hello\n[✓] one\n- point");
    }

    #[test]
    fn copy_all_keeps_text_and_images_in_order() {
        let e = editor(&[
            Block::text("hello"),
            Block::image("/tmp/a.png"),
            Block::bullet("point"),
        ]);
        let lines = e.lines_for_copy_all();
        assert_eq!(lines.len(), 3);
        match &lines[0] {
            CopyLine::Text(t) => assert_eq!(t, "hello"),
            _ => panic!("text first"),
        }
        match &lines[1] {
            CopyLine::Image(p) => assert!(p.ends_with("a.png")),
            _ => panic!("image second"),
        }
        match &lines[2] {
            CopyLine::Text(t) => assert_eq!(t, "- point"),
            _ => panic!("bullet third"),
        }
    }

    #[test]
    fn slash_copy_strips_the_query_and_returns_text() {
        let mut e = editor(&[Block::text("keep me")]);
        e.end();
        e.newline();
        type_in(&mut e, "/copy");
        assert_eq!(e.menu_selected(), Some(Command::Copy));
        match e.apply(Command::Copy).expect("copy yields payload") {
            CopyPayload::Text(text) => assert_eq!(text, "keep me"),
            CopyPayload::Image(_) | CopyPayload::All(_) => panic!("expected text"),
        }
        assert!(e.menu.is_none());
        // The `/copy` line is gone (it was empty after stripping).
        assert_eq!(e.value(), vec![Block::text("keep me")]);
    }

    #[test]
    fn image_for_copy_picks_nearest_above_the_cursor() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/screenshot.png");
        let mut e = editor(&[
            Block::text("above"),
            Block::image(path),
            Block::text("below"),
        ]);
        // Cursor on the last line (under the image).
        e.down();
        e.down();
        let got = e.image_for_copy().expect("finds the image above");
        assert!(got.ends_with("screenshot.png"));
    }

    #[test]
    fn slash_image_returns_the_picture_path() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/screenshot.png");
        let mut e = editor(&[Block::image(path), Block::text("")]);
        e.down();
        type_in(&mut e, "/img");
        assert_eq!(e.menu_selected(), Some(Command::CopyImage));
        match e.apply(Command::CopyImage).expect("image payload") {
            CopyPayload::Image(p) => assert!(p.ends_with("screenshot.png")),
            CopyPayload::Text(_) | CopyPayload::All(_) => panic!("expected image"),
        }
    }

    #[test]
    fn a_picture_is_deleted_by_backspace() {
        let mut e = editor(&[Block::text("a"), Block::image("/tmp/a.png")]);
        e.down();
        e.backspace();
        assert_eq!(e.value(), vec![Block::text("a")]);
    }

    #[test]
    fn the_menu_filters_by_abbreviation() {
        let mut e = editor(&[]);
        type_in(&mut e, "/che");
        assert_eq!(e.menu_selected(), Some(Command::Todo));
    }

    #[test]
    fn the_menu_closes_when_nothing_matches() {
        let mut e = editor(&[]);
        type_in(&mut e, "/zz");
        assert!(e.menu.is_none());
        assert_eq!(e.value(), vec![Block::text("/zz")], "the text is kept");
    }

    #[test]
    fn the_menu_closes_when_the_slash_is_removed() {
        let mut e = editor(&[]);
        type_in(&mut e, "/to");
        assert!(e.menu.is_some());
        e.backspace();
        e.backspace();
        assert!(e.menu.is_some());
        e.backspace();
        assert!(e.menu.is_none(), "removing the slash closes the menu");
    }

    #[test]
    fn right_on_sole_image_inserts_text_line() {
        let blocks = [Block::image("/tmp/x.png")];
        let mut e = editor(&blocks);
        assert!(matches!(e.lines[0], Line::Image { .. }));
        assert_eq!(e.cursor, 0);
        e.right();
        assert_eq!(e.lines.len(), 2, "should insert a text line");
        assert_eq!(e.cursor, 1);
        assert!(matches!(e.lines[1], Line::Text(_)));
        assert!(e.input().is_some());
    }

    #[test]
    fn left_on_first_image_inserts_text_line_above() {
        let blocks = [Block::image("/tmp/x.png")];
        let mut e = editor(&blocks);
        e.left();
        assert_eq!(e.lines.len(), 2);
        assert_eq!(e.cursor, 0);
        assert!(matches!(e.lines[0], Line::Text(_)));
        assert!(matches!(e.lines[1], Line::Image { .. }));
    }

    #[test]
    fn visible_band_clips_top_like_bottom() {
        // Block at rows 5..15 inside viewport scroll=8 height=10 → show 8..15
        // at y=0 with 7 rows, 3 skipped off the top.
        assert_eq!(visible_band(5, 10, 8, 10), Some((0, 7, 3)));
        // Fully above / below.
        assert_eq!(visible_band(0, 5, 8, 10), None);
        assert_eq!(visible_band(20, 5, 8, 10), None);
        // Bottom clip only (scroll=0): y=start, shrink from bottom.
        assert_eq!(visible_band(8, 10, 0, 12), Some((8, 4, 0)));
    }

    #[test]
    fn consecutive_images_shrink_when_scrolled_off_the_top() {
        let path = "/tmp/a.png";
        let mut e = editor(&[Block::image(path), Block::image(path), Block::text("tail")]);
        // Full view first so click can land on the trailing text (row 20).
        e.layout(40, 30);
        assert!(e.click(20, 0));
        assert_eq!(e.cursor_line(), 2);

        let (placed, _) = e.layout(40, 12);
        // cursor at visual 20 → scroll = 20 + 1 - 12 = 9
        assert_eq!(e.scroll(), 9);
        let imgs: Vec<_> = placed
            .iter()
            .filter(|p| matches!(p.block, Painted::Image(_)))
            .collect();
        assert_eq!(imgs.len(), 2);
        // Image 0 occupied 0..10; with scroll 9 only 1 row remains at y=0.
        assert_eq!((imgs[0].y, imgs[0].rows), (0, 1));
        // Image 1 at 10..20 → y=1, full 10 rows (fits in remaining 11).
        assert_eq!((imgs[1].y, imgs[1].rows), (1, 10));
        // No overlap: first ends at y+rows = 1, second starts at 1.
        assert_eq!(imgs[0].y + imgs[0].rows, imgs[1].y);
    }

    #[test]
    fn narrow_maximum_body_keeps_the_last_row_addressable() {
        let line = "x".repeat(MAX_NOTES_LINE_LEN);
        let blocks = vec![Block::text(&line); MAX_BODY_LINES];
        let mut e = editor(&blocks);
        e.cursor = e.lines.len() - 1;
        e.input().unwrap().end();

        let (_, cursor) = e.layout(1, 10);

        let expected_height = MAX_BODY_LINES * MAX_NOTES_LINE_LEN;
        assert_eq!(e.content_height() as usize, expected_height);
        assert_eq!(e.scroll() as usize, expected_height - 10);
        assert_eq!(cursor, Some((9, 1)));
        assert!(e.click(0, 0));
        assert_eq!(e.cursor_line(), MAX_BODY_LINES - 1);
    }

    #[test]
    fn line_join_at_the_length_limit_never_discards_the_next_line() {
        let full = "a".repeat(MAX_NOTES_LINE_LEN);

        let mut backward = editor(&[Block::text(&full), Block::text("tail")]);
        backward.cursor = 1;
        backward.input().unwrap().home();
        backward.backspace();
        assert_eq!(
            backward.value(),
            vec![Block::text(&full), Block::text("tail")]
        );

        let mut forward = editor(&[Block::text(&full), Block::text("tail")]);
        forward.input().unwrap().end();
        forward.delete();
        assert_eq!(
            forward.value(),
            vec![Block::text(&full), Block::text("tail")]
        );
    }

    #[test]
    fn oversized_cross_line_selection_replacement_is_rejected_without_data_loss() {
        let full = "a".repeat(MAX_NOTES_LINE_LEN);
        let mut editor = editor(&[Block::text(&full), Block::text("tail")]);
        editor.sel_anchor = Some((0, MAX_NOTES_LINE_LEN));
        editor.cursor = 1;
        editor.input().unwrap().home();

        editor.insert('x');

        assert_eq!(
            editor.value(),
            vec![Block::text(&full), Block::text("tail")]
        );
    }

    #[test]
    fn oversized_image_path_detection_never_discards_a_source_line() {
        let root = std::env::temp_dir().join(format!(
            "mach-long-image-path-test-{}",
            uuid::Uuid::new_v4()
        ));
        let first_dir = "a".repeat(240);
        let second_dir = "b".repeat(240);
        let file = format!("{}.png", "c".repeat(40));
        let relative = format!("{first_dir}/{second_dir}/{file}");
        assert!(relative.graphemes(true).count() > MAX_NOTES_LINE_LEN);
        std::fs::create_dir_all(root.join(&first_dir).join(&second_dir)).unwrap();
        std::fs::write(root.join(&relative), []).unwrap();

        let split = relative.len() / 2;
        let (left, right) = relative.split_at(split);
        let expected = vec![Block::text(left), Block::text(right)];
        let mut loaded = editor(&expected);
        loaded.set_image_root(root.clone());
        assert_eq!(loaded.value(), expected);

        let mut pasted = editor(&[]);
        pasted.set_image_root(root.clone());
        pasted.insert_str(&format!("{left}\n{right}"));
        assert_eq!(pasted.value(), expected);

        std::fs::remove_dir_all(root).unwrap();
    }
}
