//! The draft in the composer. Tern draws the editor and its caret, but the
//! text is sand's: keys arrive on the pty, and with native editing on,
//! selections Tern keeps come back as edit events (in UTF-16 units) and the
//! undo key as an undo event, both applied here.

use std::time::{Duration, Instant};

use tern_sdk::Key;

use super::model::Image;

/// Undo steps kept.
const HISTORY: usize = 200;

/// A paste longer than this many chars, or with more line breaks than
/// `PASTE_BREAKS`, shows as a placeholder (as in Claude Code).
const PASTE_CHARS: usize = 800;
const PASTE_BREAKS: usize = 2;
/// A paste up to this many chars can be expanded in place by pasting it
/// again, which a hint says for `EXPAND_HINT`.
const EXPAND_CHARS: usize = 100_000;
const EXPAND_HINT: Duration = Duration::from_secs(8);

/// The placeholder for paste `id` of `breaks` line breaks, as Claude Code writes it.
fn placeholder(id: usize, breaks: usize) -> String {
    match breaks {
        0 => format!("[Pasted text #{id}]"),
        _ => format!("[Pasted text #{id} +{breaks} lines]"),
    }
}

/// Where `[Pasted text #N]` or `[Pasted text #N +M lines]` placeholders sit
/// in `text`: their byte ranges and `N`.
fn placeholders(text: &str) -> Vec<(std::ops::Range<usize>, usize)> {
    const OPEN: &str = "[Pasted text #";
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(OPEN).map(|i| from + i) {
        from = at + OPEN.len();
        let rest = &text[from..];
        let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let Ok(id) = rest[..digits].parse() else { continue };
        let mut end = digits;
        if let Some(lines) = rest[end..].strip_prefix(" +") {
            let n = lines.find(|c: char| !c.is_ascii_digit()).unwrap_or(lines.len());
            if n > 0 && lines[n..].starts_with(" lines") {
                end += 2 + n + " lines".len();
            }
        }
        if rest[end..].starts_with(']') {
            from += end + 1;
            found.push((at..from, id));
        }
    }
    found
}

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
    /// Images pasted for the next message.
    pub images: Vec<Image>,
    /// Texts pasted as placeholders, `[Pasted text #N]` being `pastes[N - 1]`;
    /// kept so an undone or restored draft still expands.
    pastes: Vec<String>,
    /// The paste last made a placeholder, which pasting again expands.
    last_paste: Option<usize>,
    /// When that paste was made, for the hint that says so.
    hint: Option<(usize, Instant)>,
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

    /// Pastes `text`: a long one as a placeholder, which `expand` turns back.
    /// Pasting the same text again expands its placeholder in place.
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ");
        if self.last_paste.is_some_and(|id| self.pastes[id - 1] == text) && self.expand_last() {
            return;
        }
        let breaks = text.matches('\n').count();
        let chars = text.chars().count();
        if chars <= PASTE_CHARS && breaks <= PASTE_BREAKS {
            return self.insert(&text);
        }
        self.pastes.push(text);
        let id = self.pastes.len();
        self.insert(&placeholder(id, breaks));
        self.last_paste = Some(id);
        self.hint = (chars <= EXPAND_CHARS).then(|| (id, Instant::now()));
    }

    /// Puts the newest placeholder back as its text, the caret after it, if
    /// it is the last paste's; false if not.
    fn expand_last(&mut self) -> bool {
        let newest = placeholders(&self.draft)
            .into_iter()
            .filter(|(_, id)| (1..=self.pastes.len()).contains(id))
            .max_by_key(|(_, id)| *id);
        let Some((range, id)) = newest.filter(|(_, id)| Some(*id) == self.last_paste) else { return false };
        let text = self.pastes[id - 1].clone();
        if text.chars().count() > EXPAND_CHARS {
            return false;
        }
        self.remember(false);
        self.draft.replace_range(range.clone(), &text);
        self.cursor = self.draft[..range.start + text.len()].chars().count();
        self.last_paste = None;
        self.hint = None;
        self.edited();
        true
    }

    /// The paste whose "paste again to expand" hint is showing, for a while
    /// after it was made and while there is a draft.
    pub fn expand_hint(&self) -> Option<usize> {
        let (id, at) = self.hint?;
        (at.elapsed() < EXPAND_HINT && !self.draft.is_empty()).then_some(id)
    }

    /// `text` with the placeholders of pastes put back as what was pasted;
    /// one edited out of shape stays as it is.
    pub fn expand(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut from = 0;
        for (range, id) in placeholders(text) {
            if let Some(pasted) = id.checked_sub(1).and_then(|i| self.pastes.get(i)) {
                out.push_str(&text[from..range.start]);
                out.push_str(pasted);
                from = range.end;
            }
        }
        out.push_str(&text[from..]);
        out
    }

    /// Where the draft's placeholders of pastes are, in UTF-16 units, for the
    /// editor to mark them.
    pub fn placeholders_utf16(&self) -> Vec<(usize, usize)> {
        let utf16 = |at: usize| self.draft[..at].encode_utf16().count();
        placeholders(&self.draft)
            .into_iter()
            .filter(|(_, id)| (1..=self.pastes.len()).contains(id))
            .map(|(range, _)| (utf16(range.start), utf16(range.end)))
            .collect()
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

    /// The `@` mention being typed at the caret: where its `@` is and what follows it.
    pub fn mention(&self) -> Option<(usize, String)> {
        super::files::at_caret(&self.draft, self.cursor)
    }

    /// Replaces the draft from char `start` to the caret with `text`; a space
    /// `text` ends with takes the place of one after the caret.
    pub fn replace_to_caret(&mut self, start: usize, text: &str) {
        if start > self.cursor {
            return;
        }
        self.remember(false);
        let spaced = text.ends_with(' ') && self.draft.chars().nth(self.cursor) == Some(' ');
        let (from, to) = (self.byte(start), self.byte(self.cursor + usize::from(spaced)));
        self.draft.replace_range(from..to, text);
        self.cursor = start + text.chars().count();
        self.edited();
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
                let end = self.byte(self.cursor);
                // A paste's placeholder goes whole.
                let start = placeholders(&self.draft[..end])
                    .last()
                    .filter(|(range, id)| range.end == end && (1..=self.pastes.len()).contains(id))
                    .map_or_else(|| self.byte(self.cursor - 1), |(range, _)| range.start);
                self.cursor -= self.draft[start..end].chars().count();
                self.draft.replace_range(start..end, "");
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
            "paste" => self.paste(key.text.as_deref().unwrap_or_default()),
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
    fn long_pastes_show_as_placeholders_and_expand_to_what_was_pasted() {
        let mut c = Composer::default();
        let paste = |text: &str| Key { text: Some(text.into()), ..key("paste") };
        c.insert("see ");
        c.key(&paste("a\nb\nc"));
        assert_eq!(c.draft, "see a\nb\nc", "two line breaks stay inline");
        c.set("see ");
        c.key(&paste("a\r\nb\nc\nd"));
        c.insert(" and ");
        let long = "x".repeat(801);
        c.key(&paste(&long));
        assert_eq!(c.draft, "see [Pasted text #1 +3 lines] and [Pasted text #2]");
        assert_eq!(c.expand(&c.draft), format!("see a\nb\nc\nd and {long}"));
        // A placeholder edited out of shape, or of no paste, is sent as typed.
        assert_eq!(c.expand("[Pasted text #1 +3 line] [Pasted text #9]"), "[Pasted text #1 +3 line] [Pasted text #9]");

        // Backspace takes a placeholder whole; undo brings it back, still expanding.
        assert!(c.key(&key("backspace")));
        assert_eq!(c.draft, "see [Pasted text #1 +3 lines] and ");
        assert!(c.key(&key("backspace")));
        assert_eq!(c.draft, "see [Pasted text #1 +3 lines] and");
        assert_eq!(c.take(), "see [Pasted text #1 +3 lines] and");
        c.undo();
        assert_eq!(c.expand(&c.draft), "see a\nb\nc\nd and");
        // The paste itself is one undo step.
        c.set("");
        c.key(&paste(&long));
        c.undo();
        assert_eq!(c.draft, "");
    }

    #[test]
    fn pasting_the_same_text_again_expands_its_placeholder() {
        let mut c = Composer::default();
        let paste = |text: &str| Key { text: Some(text.into()), ..key("paste") };
        c.key(&paste("a\tb\nc\nd\ne"));
        c.insert(" tail");
        assert_eq!(c.expand_hint(), Some(1));
        c.key(&Key::named("home"));
        c.key(&paste("a\tb\nc\nd\ne"));
        assert_eq!(c.draft, "a    b\nc\nd\ne tail", "tabs go as four spaces");
        assert_eq!(c.cursor_utf16(), 12, "the caret ends after the expanded text");
        assert_eq!(c.expand_hint(), None);
        c.undo();
        assert_eq!(c.draft, "[Pasted text #1 +3 lines] tail");

        // Another text, or a paste that is not the newest, collapses anew.
        c.set("");
        c.key(&paste("1\n2\n3\n4"));
        c.key(&paste("5\n6\n7\n8"));
        c.key(&paste("1\n2\n3\n4"));
        assert_eq!(c.draft, "[Pasted text #2 +3 lines][Pasted text #3 +3 lines][Pasted text #4 +3 lines]");
        c.take();
        assert_eq!(c.expand_hint(), None, "no hint without a draft");
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
    fn a_mention_is_completed_in_place() {
        let mut c = Composer::default();
        c.insert("see @ma please");
        c.edit(0, 0, "", 7, 14);
        assert_eq!(c.mention(), Some((4, "ma".into())));
        c.replace_to_caret(4, "@src/main.rs ");
        assert_eq!((c.draft.as_str(), c.cursor_utf16()), ("see @src/main.rs please", 17));
        assert_eq!(c.mention(), None);
        c.undo();
        assert_eq!(c.draft, "see @ma please");
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
