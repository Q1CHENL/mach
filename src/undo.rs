//! Form-level undo / redo.
//!
//! - Snapshot before each mutating edit.
//! - Coalesce consecutive typing / single-char deletes within a short window.
//! - Atomic edits (paste, word-delete, newline, structure) always split.
//! - New edits clear redo; caret and selection are part of the snapshot.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Max undo steps kept per open dialog.
const MAX_DEPTH: usize = 64;
/// Window for coalescing successive typing keys into one undo step.
const COALESCE_WINDOW: Duration = Duration::from_millis(1_200);

/// How an upcoming edit should group with the previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// Single-character insert, backspace, or forward-delete.
    Typing,
    /// Paste, word ops, newline, list/image changes, importance, due, etc.
    Atomic,
}

/// Undo/redo stacks for one form dialog.
#[derive(Debug, Clone)]
pub struct History<T> {
    undo: VecDeque<T>,
    redo: VecDeque<T>,
    last_edit: Option<(EditKind, Instant)>,
}

impl<T> Default for History<T> {
    fn default() -> Self {
        Self {
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            last_edit: None,
        }
    }
}

impl<T> History<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when the next typing edit should extend the current undo step.
    pub fn will_coalesce(&self, kind: EditKind) -> bool {
        kind == EditKind::Typing
            && self.last_edit.is_some_and(|(last_kind, at)| {
                last_kind == EditKind::Typing && at.elapsed() < COALESCE_WINDOW
            })
            && !self.undo.is_empty()
    }

    /// Refresh the coalesce timer without pushing a snapshot.
    pub fn touch_coalesce(&mut self) {
        if let Some((_, at)) = &mut self.last_edit {
            *at = Instant::now();
        }
    }

    /// Push a pre-edit snapshot. Do not call when [`Self::will_coalesce`] is true.
    pub fn push(&mut self, current: T, kind: EditKind) {
        self.undo.push_back(current);
        while self.undo.len() > MAX_DEPTH {
            self.undo.pop_front();
        }
        self.redo.clear();
        self.last_edit = Some((kind, Instant::now()));
    }

    /// Call before mutating. Builds `current` only when a new step is needed.
    pub fn before_edit_with(&mut self, kind: EditKind, current: impl FnOnce() -> T) {
        if self.will_coalesce(kind) {
            self.touch_coalesce();
            return;
        }
        self.push(current(), kind);
    }

    /// End the current typing run (e.g. after navigation or focus change).
    pub fn break_coalesce(&mut self) {
        self.last_edit = None;
    }

    /// Pop undo; push `current` onto redo. Returns the state to restore.
    pub fn undo(&mut self, current: T) -> Option<T> {
        let prev = self.undo.pop_back()?;
        self.redo.push_back(current);
        self.break_coalesce();
        Some(prev)
    }

    /// Pop redo; push `current` onto undo. Returns the state to restore.
    pub fn redo(&mut self, current: T) -> Option<T> {
        let next = self.redo.pop_back()?;
        self.undo.push_back(current);
        self.break_coalesce();
        Some(next)
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Update snapshots after an external vocabulary change invalidates or
    /// reorders references stored inside them.
    pub fn for_each_mut(&mut self, mut update: impl FnMut(&mut T)) {
        self.undo.iter_mut().for_each(&mut update);
        self.redo.iter_mut().for_each(update);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_coalesces_into_one_step() {
        let mut h: History<String> = History::new();
        h.before_edit_with(EditKind::Typing, || "a".into());
        h.before_edit_with(EditKind::Typing, || "ab".into());
        h.before_edit_with(EditKind::Typing, || "abc".into());
        assert_eq!(h.undo.len(), 1);
        let restored = h.undo("abc".into()).unwrap();
        assert_eq!(restored, "a");
    }

    #[test]
    fn atomic_always_splits() {
        let mut h: History<String> = History::new();
        h.before_edit_with(EditKind::Typing, || "a".into());
        h.before_edit_with(EditKind::Atomic, || "ab".into());
        h.before_edit_with(EditKind::Typing, || "abc".into());
        assert_eq!(h.undo.len(), 3);
    }

    #[test]
    fn redo_clears_on_new_edit() {
        let mut h: History<String> = History::new();
        h.before_edit_with(EditKind::Atomic, || "0".into());
        let _ = h.undo("1".into());
        assert!(h.can_redo());
        h.before_edit_with(EditKind::Atomic, || "2".into());
        assert!(!h.can_redo());
    }

    #[test]
    fn coalesce_skips_snapshot_fn() {
        let mut h: History<String> = History::new();
        h.before_edit_with(EditKind::Typing, || "a".into());
        let mut built = 0;
        h.before_edit_with(EditKind::Typing, || {
            built += 1;
            "ab".into()
        });
        assert_eq!(built, 0);
        assert_eq!(h.undo.len(), 1);
    }
}
