//! The draft in the composer. Tern draws the editor and its caret, but the
//! text is sand's: keys arrive on the pty and are applied here.

use tern_sdk::Key;

#[derive(Debug, Default)]
pub struct Composer {
    pub draft: String,
    /// The caret, in chars.
    cursor: usize,
}

impl Composer {
    fn byte(&self, chars: usize) -> usize {
        self.draft.char_indices().nth(chars).map_or(self.draft.len(), |(i, _)| i)
    }

    fn len(&self) -> usize {
        self.draft.chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.draft.is_empty()
    }

    /// The caret in UTF-16 code units, as the editor node takes it.
    pub fn cursor_utf16(&self) -> usize {
        self.draft.chars().take(self.cursor).map(char::len_utf16).sum()
    }

    pub fn insert(&mut self, text: &str) {
        let at = self.byte(self.cursor);
        self.draft.insert_str(at, text);
        self.cursor += text.chars().count();
    }

    /// Empties the draft and returns it.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.draft)
    }

    /// Applies an editing key; false for a key that edits nothing.
    pub fn key(&mut self, key: &Key) -> bool {
        match key.name.as_str() {
            "enter" if key.shift || key.alt => self.insert("\n"),
            "backspace" if self.cursor > 0 => {
                let at = self.byte(self.cursor - 1);
                self.draft.remove(at);
                self.cursor -= 1;
            }
            "delete" if self.cursor < self.len() => {
                let at = self.byte(self.cursor);
                self.draft.remove(at);
            }
            "left" => self.cursor = self.cursor.saturating_sub(1),
            "right" => self.cursor = (self.cursor + 1).min(self.len()),
            "home" => self.cursor = 0,
            "end" => self.cursor = self.len(),
            "u" if key.ctrl => {
                let at = self.byte(self.cursor);
                self.draft.drain(..at);
                self.cursor = 0;
            }
            "paste" => self.insert(key.text.as_deref().unwrap_or_default()),
            _ => match key.typed() {
                Some(text) => self.insert(text),
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
}
