//! Browser-style history over the sidebar's views (the file stack and a
//! repository's git view), so back/forward can step between them.

use std::path::PathBuf;

/// Most entries kept; the oldest is dropped past this.
pub const HISTORY_CAP: usize = 50;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewEntry {
    Stack,
    Git(PathBuf),
}

#[derive(Debug)]
pub struct ViewHistory {
    entries: Vec<ViewEntry>,
    cursor: usize,
}

impl Default for ViewHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewHistory {
    pub fn new() -> Self {
        Self {
            entries: vec![ViewEntry::Stack],
            cursor: 0,
        }
    }

    pub fn current(&self) -> &ViewEntry {
        &self.entries[self.cursor]
    }

    pub fn visit(&mut self, entry: ViewEntry) {
        if entry == *self.current() {
            return;
        }
        self.entries.truncate(self.cursor + 1);
        self.entries.push(entry);
        if self.entries.len() > HISTORY_CAP {
            self.entries.remove(0);
        }
        self.cursor = self.entries.len() - 1;
    }

    pub fn back(&mut self) -> Option<&ViewEntry> {
        self.cursor = self.cursor.checked_sub(1)?;
        Some(&self.entries[self.cursor])
    }

    pub fn forward(&mut self) -> Option<&ViewEntry> {
        if self.cursor + 1 >= self.entries.len() {
            return None;
        }
        self.cursor += 1;
        Some(&self.entries[self.cursor])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_on_the_stack() {
        assert_eq!(ViewHistory::new().current(), &ViewEntry::Stack);
    }

    #[test]
    fn visit_pushes_and_back_forward_step() {
        let mut h = ViewHistory::new();
        h.visit(ViewEntry::Git("/r".into()));
        assert_eq!(h.back(), Some(&ViewEntry::Stack));
        assert_eq!(h.back(), None);
        assert_eq!(h.forward(), Some(&ViewEntry::Git("/r".into())));
        assert_eq!(h.forward(), None);
    }

    #[test]
    fn visit_after_back_truncates_forward() {
        let mut h = ViewHistory::new();
        h.visit(ViewEntry::Git("/a".into()));
        h.back();
        h.visit(ViewEntry::Git("/b".into()));
        assert_eq!(h.forward(), None);
        assert_eq!(h.back(), Some(&ViewEntry::Stack));
    }

    #[test]
    fn visiting_the_current_entry_is_a_noop() {
        let mut h = ViewHistory::new();
        h.visit(ViewEntry::Stack);
        assert_eq!(h.back(), None);
    }

    #[test]
    fn history_is_capped_at_fifty() {
        let mut h = ViewHistory::new();
        for i in 0..120 {
            h.visit(ViewEntry::Git(format!("/r{i}").into()));
            h.visit(ViewEntry::Stack);
        }
        let mut steps = 0;
        while h.back().is_some() {
            steps += 1;
        }
        assert_eq!(steps, HISTORY_CAP - 1);
    }
}
