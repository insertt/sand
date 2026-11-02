//! The draft in the composer. Tern draws the editor and its caret, but the
//! text is sand's: keys arrive on the pty, and with native editing on,
//! selections Tern keeps come back as edit events (in UTF-16 units) and the
//! undo key as an undo event, both applied here.

use tern_sdk::Key;

/// Undo steps kept.
const HISTORY: usize = 200;

#[derive(Debug, Default)]
pub struct Composer {
    pub draft: String,
    /// The caret, in chars.
    cursor: usize,
    /// The completion highlighted, and whether Esc put the completions away.
    pub pick: usize,
    pub dismissed: bool,
    /// Drafts (and carets) before each change, newest last.
    history: Vec<(String, usize)>,
    /// The last change typed a word character, so the next one joins its undo step.
    typing: bool,
}

impl Composer {
    fn byte(&self, chars: usize) -> usize {
        self.draft.char_indices().nth(chars).map_or(self.draft.len(), |(i, _)| i)
    }

    fn len(&self) -> usize {
        self.draft.chars().count()
    }

    fn len_utf16(&self) -> usize {
        self.draft.chars().map(char::len_utf16).sum()
    }

    /// The char index at `units` UTF-16 units in.
    fn char_at(&self, units: usize) -> usize {
        let mut seen = 0;
        for (i, c) in self.draft.chars().enumerate() {
            if seen >= units {
                return i;
            }
            seen += c.len_utf16();
        }
        self.len()
    }

    pub fn is_empty(&self) -> bool {
        self.draft.is_empty()
    }

    /// The caret in UTF-16 code units, as the editor node takes it.
    pub fn cursor_utf16(&self) -> usize {
        self.draft.chars().take(self.cursor).map(char::len_utf16).sum()
    }

    /// Whether the caret sits at the end of the draft.
    pub fn at_end(&self) -> bool {
        self.cursor == self.len()
    }

    /// Remembers the draft before a change; typing a word joins one step.
    fn remember(&mut self, typing: bool) {
        if !(typing && self.typing) {
            self.history.push((self.draft.clone(), self.cursor));
            if self.history.len() > HISTORY {
                self.history.remove(0);
            }
        }
        self.typing = typing;
    }

    /// A new draft brings the completions back, from the top.
    fn edited(&mut self) {
        self.pick = 0;
        self.dismissed = false;
    }

    fn insert_typed(&mut self, text: &str, typing: bool) {
        self.remember(typing);
        let at = self.byte(self.cursor);
        self.draft.insert_str(at, text);
        self.cursor += text.chars().count();
        self.edited();
    }

    pub fn insert(&mut self, text: &str) {
        self.insert_typed(text, false);
    }

    /// Replaces the draft, with the caret at its end.
    pub fn set(&mut self, text: &str) {
        self.remember(false);
        self.draft = text.to_string();
        self.cursor = self.len();
        self.edited();
    }

    /// Empties the draft and returns it (undo brings it back).
    pub fn take(&mut self) -> String {
        if !self.draft.is_empty() {
            self.remember(false);
        }
        self.cursor = 0;
        self.edited();
        std::mem::take(&mut self.draft)
    }

    /// Applies a native edit: UTF-16 `[from, to)` replaced with `text`, the
    /// caret then at `cursor`. Ignored when the draft changed since Tern saw
    /// it (`len`), as keys in flight would have.
    pub fn edit(&mut self, from: usize, to: usize, text: &str, cursor: usize, len: usize) {
        if len != self.len_utf16() || from > to || to > len {
            return;
        }
        if from != to || !text.is_empty() {
            self.remember(false);
            let (start, end) = (self.char_at(from), self.char_at(to));
            let (start, end) = (self.byte(start), self.byte(end));
            self.draft.replace_range(start..end, text);
            self.edited();
        }
        self.cursor = self.char_at(cursor);
    }

    /// Takes the last change back.
    pub fn undo(&mut self) {
        if let Some((draft, cursor)) = self.history.pop() {
            self.draft = draft;
            self.cursor = cursor;
            self.typing = false;
            self.edited();
        }
    }

    /// Applies an editing key; false for a key that edits nothing.
    pub fn key(&mut self, key: &Key) -> bool {
        match key.name.as_str() {
            "enter" if key.shift || key.alt => self.insert("\n"),
            "backspace" if self.cursor > 0 => {
                self.remember(false);
                let at = self.byte(self.cursor - 1);
                self.draft.remove(at);
                self.cursor -= 1;
                self.edited();
            }
            "delete" if self.cursor < self.len() => {
                self.remember(false);
                let at = self.byte(self.cursor);
                self.draft.remove(at);
                self.edited();
            }
            "left" => self.cursor = self.cursor.saturating_sub(1),
            "right" => self.cursor = (self.cursor + 1).min(self.len()),
            "home" => self.cursor = 0,
            "end" => self.cursor = self.len(),
            "u" if key.ctrl => {
                self.remember(false);
                let at = self.byte(self.cursor);
                self.draft.drain(..at);
                self.cursor = 0;
                self.edited();
            }
            "z" if key.ctrl || key.meta => self.undo(),
            "paste" => self.insert(key.text.as_deref().unwrap_or_default()),
            _ => match key.typed() {
                Some(text) => {
                    let word = text.chars().all(char::is_alphanumeric);
                    self.insert_typed(text, word);
                }
                None => return false,
            },
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> Key {
        Key::named(name)
    }

    fn typed(text: &str) -> Key {
        Key { text: Some(text.into()), ..Key::named(text) }
    }

    #[test]
    fn edits_by_codepoint_and_reports_utf16_cursors() {
        let mut c = Composer::default();
        c.insert("a😀é");
        assert_eq!(c.cursor_utf16(), 4);
        assert!(c.key(&key("left")));
        c.insert("界");
        assert_eq!(c.draft, "a😀界é");
        assert_eq!(c.cursor_utf16(), 4);
        assert!(c.key(&key("backspace")));
        assert!(c.key(&key("delete")));
        assert_eq!(c.draft, "a😀");
        assert!(c.key(&key("home")));
        assert!(!c.key(&key("backspace")));
        assert_eq!(c.draft, "a😀");
        assert_eq!(c.take(), "a😀");
        assert!(c.is_empty());
        assert_eq!(c.cursor_utf16(), 0);
    }

    #[test]
    fn shift_enter_breaks_the_line_and_plain_enter_is_left_alone() {
        let mut c = Composer::default();
        c.insert("one");
        assert!(c.key(&Key { shift: true, ..key("enter") }));
        assert!(!c.key(&key("enter")));
        let paste = Key { text: Some("two".into()), ..key("paste") };
        assert!(c.key(&paste));
        assert_eq!(c.draft, "one\ntwo");
        assert!(c.key(&Key { ctrl: true, ..key("u") }));
        assert!(c.is_empty());
    }

    #[test]
    fn native_edits_replace_utf16_ranges() {
        let mut c = Composer::default();
        c.insert("hi 😀 there");
        // Tern's selection over "😀 " (UTF-16 3..6) replaced with "you ".
        c.edit(3, 6, "you ", 7, 11);
        assert_eq!((c.draft.as_str(), c.cursor_utf16()), ("hi you there", 7));
        // A caret move alone changes nothing else and is no undo step.
        c.edit(0, 0, "", 0, 12);
        assert_eq!(c.cursor_utf16(), 0);
        // An edit made against an older draft is dropped.
        c.edit(0, 2, "", 0, 99);
        assert_eq!(c.draft, "hi you there");
        c.undo();
        assert_eq!(c.draft, "hi 😀 there");
    }

    #[test]
    fn undo_takes_back_words_edits_and_a_sent_draft() {
        let mut c = Composer::default();
        for ch in ["f", "i", "x"] {
            c.key(&typed(ch));
        }
        c.key(&typed(" "));
        for ch in ["i", "t"] {
            c.key(&typed(ch));
        }
        assert_eq!(c.draft, "fix it");
        c.undo();
        assert_eq!(c.draft, "fix ");
        c.undo();
        assert_eq!(c.draft, "fix");
        c.undo();
        assert_eq!(c.draft, "");
        c.set("draft");
        assert_eq!(c.take(), "draft");
        c.key(&Key { ctrl: true, ..key("z") });
        assert_eq!((c.draft.as_str(), c.cursor_utf16()), ("draft", 5), "a sent draft comes back");
    }
}
