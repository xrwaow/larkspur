//! A minimal, view-owned text field.
//!
//! GPUI 0.2 has no drop-in text input, so the pieces every text box in this app
//! needs — a string, a caret, a selection, and key handling — live here and are
//! reused by the search box, the settings path box, the inline playlist rename,
//! and the playlist's add-songs search.
//!
//! It owns no focus: the surrounding view tracks focus and routes keys here.
//! Editing keys it understands (typing, backspace/delete, caret movement,
//! `ctrl+a`, and the word/selection chords) are consumed; everything else
//! (enter, escape, `ctrl+tab`, …) is left for the surrounding view.

use gpui::{div, prelude::*, px, AnyElement, KeyDownEvent};

use crate::ui::theme::Theme;

#[derive(Default, Clone)]
pub struct TextField {
    pub value: String,
    /// Caret position, as a byte index into `value`.
    pub caret: usize,
    /// Where the current selection was anchored. Equal to `caret` when there's
    /// no selection.
    pub anchor: usize,
}

impl TextField {
    /// A field seeded with `value`, caret at the end.
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let caret = value.len();
        Self { value, caret, anchor: caret }
    }

    pub fn is_empty(&self) -> bool {
        self.value.trim().is_empty()
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.caret = 0;
        self.anchor = 0;
    }

    /// The selected byte range, `None` when the caret is collapsed.
    pub fn selection(&self) -> Option<(usize, usize)> {
        (self.anchor != self.caret).then(|| {
            if self.anchor < self.caret {
                (self.anchor, self.caret)
            } else {
                (self.caret, self.anchor)
            }
        })
    }

    /// Handle an editing key, with clipboard text available for paste.
    ///
    /// Only the surrounding view has `App` access to read the clipboard, so it
    /// passes the text in; `ctrl+v` inserts it at the caret. Everything else
    /// falls through to [`handle_key`](Self::handle_key).
    pub fn handle_key_with_clipboard(
        &mut self,
        event: &KeyDownEvent,
        clipboard: Option<&str>,
    ) -> bool {
        let modifiers = event.keystroke.modifiers;
        if modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && event.keystroke.key == "v"
        {
            if let Some(text) = clipboard {
                self.insert(text);
                return true;
            }
            return false;
        }
        self.handle_key(event)
    }

    /// Handle an editing key, returning whether it was consumed.
    pub fn handle_key(&mut self, event: &KeyDownEvent) -> bool {
        let modifiers = event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        let shift = modifiers.shift;

        // Control chords we own. Anything else with a command modifier bubbles,
        // so `ctrl+tab`/`ctrl+w`/`ctrl+shift+f` keep working.
        if modifiers.control && !modifiers.alt && !modifiers.platform {
            return match key {
                "a" => {
                    self.anchor = 0;
                    self.caret = self.value.len();
                    true
                }
                "left" => {
                    let to = prev_word(&self.value, self.caret);
                    self.move_caret(to, shift);
                    true
                }
                "right" => {
                    let to = next_word(&self.value, self.caret);
                    self.move_caret(to, shift);
                    true
                }
                "home" => {
                    self.move_caret(0, shift);
                    true
                }
                "end" => {
                    self.move_caret(self.value.len(), shift);
                    true
                }
                _ => false,
            };
        }
        if modifiers.control || modifiers.alt || modifiers.platform {
            return false;
        }

        match key {
            "backspace" => {
                if !self.delete_selection() && self.caret > 0 {
                    let previous = prev_boundary(&self.value, self.caret);
                    self.value.replace_range(previous..self.caret, "");
                    self.caret = previous;
                    self.anchor = previous;
                }
                true
            }
            "delete" => {
                if !self.delete_selection() && self.caret < self.value.len() {
                    let next = next_boundary(&self.value, self.caret);
                    self.value.replace_range(self.caret..next, "");
                }
                true
            }
            "left" => {
                let to = match (self.selection(), shift) {
                    // Shift extends; a plain press collapses to the selection's
                    // near end before moving on the next press.
                    (Some((start, _)), false) => start,
                    _ => prev_boundary(&self.value, self.caret),
                };
                self.move_caret(to, shift);
                true
            }
            "right" => {
                let to = match (self.selection(), shift) {
                    (Some((_, end)), false) => end,
                    _ => next_boundary(&self.value, self.caret),
                };
                self.move_caret(to, shift);
                true
            }
            "home" => {
                self.move_caret(0, shift);
                true
            }
            "end" => {
                self.move_caret(self.value.len(), shift);
                true
            }
            _ => match &event.keystroke.key_char {
                // `key_char` already accounts for shift, so capitals and
                // punctuation just work.
                Some(text) => {
                    self.insert(text);
                    true
                }
                None => false,
            },
        }
    }

    /// Move the caret, collapsing the selection unless `extend` (shift) keeps
    /// the anchor where it was.
    fn move_caret(&mut self, to: usize, extend: bool) {
        self.caret = to.min(self.value.len());
        if !extend {
            self.anchor = self.caret;
        }
    }

    /// Delete the selection, if any. Returns whether anything was removed.
    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else { return false };
        self.value.replace_range(start..end, "");
        self.caret = start;
        self.anchor = start;
        true
    }

    /// Insert `text` at the caret, replacing any selection.
    fn insert(&mut self, text: &str) {
        self.delete_selection();
        self.value.insert_str(self.caret, text);
        self.caret += text.len();
        self.anchor = self.caret;
    }

    /// Render the box: the text with a caret and selection, or a placeholder.
    pub fn render(&self, theme: Theme, placeholder: &str) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(theme.row_odd)
            .border_1()
            .border_color(theme.border)
            .child(self.render_text(theme, Some(placeholder)))
            .into_any_element()
    }

    /// Render just the editable text — the caret, selection, and placeholder —
    /// so a caller can wrap it in its own chrome (e.g. the search box).
    pub fn render_text(&self, theme: Theme, placeholder: Option<&str>) -> AnyElement {
        let caret_px = (theme.cell_px() + 2.0).max(10.0);
        let mut row = div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .items_center()
            .text_size(px(theme.cell_px()))
            .text_color(theme.text);

        match self.selection() {
            Some((start, end)) => {
                row = row
                    .child(self.value[..start].to_string())
                    .child(
                        div()
                            .flex_none()
                            .rounded_sm()
                            .bg(theme.selection)
                            .text_color(theme.text)
                            .child(self.value[start..end].to_string()),
                    )
                    .child(self.value[end..].to_string());
            }
            None => {
                let (before, after) = self.value.split_at(self.caret.min(self.value.len()));
                row = row
                    .child(before.to_string())
                    // The caret is drawn *over* the text rather than between it:
                    // a zero-width, relatively-positioned wrapper holds an
                    // absolutely-positioned bar, so moving the caret never nudges
                    // the characters around it.
                    .child(
                        div().relative().flex_none().w(px(0.0)).h(px(caret_px)).child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .w(px(2.0))
                                .h(px(caret_px))
                                .bg(theme.accent),
                        ),
                    )
                    .child(after.to_string());
                if self.value.is_empty() {
                    if let Some(placeholder) = placeholder {
                        row = row.child(
                            div()
                                .flex_none()
                                .pl_2()
                                .text_color(theme.text_faint)
                                .child(placeholder.to_string()),
                        );
                    }
                }
            }
        }
        row.into_any_element()
    }
}

/// The previous char boundary before `at`.
pub fn prev_boundary(text: &str, at: usize) -> usize {
    text[..at.min(text.len())].char_indices().last().map(|(i, _)| i).unwrap_or(0)
}

/// The next char boundary after `at`.
pub fn next_boundary(text: &str, at: usize) -> usize {
    let at = at.min(text.len());
    text[at..].chars().next().map(|c| at + c.len_utf8()).unwrap_or(at)
}

/// The start of the word before `at` (whitespace-delimited): skip spaces, then
/// the word itself.
pub fn prev_word(text: &str, at: usize) -> usize {
    let mut index = at.min(text.len());
    while index > 0 {
        let previous = prev_boundary(text, index);
        if !text[previous..index].chars().next().is_some_and(char::is_whitespace) {
            break;
        }
        index = previous;
    }
    while index > 0 {
        let previous = prev_boundary(text, index);
        if text[previous..index].chars().next().is_some_and(char::is_whitespace) {
            break;
        }
        index = previous;
    }
    index
}

/// The end of the word at or after `at`: skip the word, then trailing spaces.
pub fn next_word(text: &str, at: usize) -> usize {
    let mut index = at.min(text.len());
    while index < text.len() {
        let next = next_boundary(text, index);
        if text[index..next].chars().next().is_some_and(char::is_whitespace) {
            break;
        }
        index = next;
    }
    while index < text.len() {
        let next = next_boundary(text, index);
        if !text[index..next].chars().next().is_some_and(char::is_whitespace) {
            break;
        }
        index = next;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Keystroke;

    fn key(name: &str, ch: Option<&str>) -> KeyDownEvent {
        let mut keystroke = Keystroke::parse(name).unwrap();
        keystroke.key_char = ch.map(str::to_string);
        KeyDownEvent { keystroke, is_held: false }
    }

    #[test]
    fn typing_inserts_at_the_caret() {
        let mut field = TextField::new("ab");
        assert!(field.handle_key(&key("c", Some("c"))));
        assert_eq!(field.value, "abc");
        assert_eq!(field.caret, 3);
    }

    #[test]
    fn backspace_deletes_the_previous_char() {
        let mut field = TextField::new("abc");
        assert!(field.handle_key(&key("backspace", None)));
        assert_eq!(field.value, "ab");
        assert_eq!(field.caret, 2);
    }

    #[test]
    fn other_keys_bubble_up() {
        let mut field = TextField::new("abc");
        assert!(!field.handle_key(&key("enter", None)));
        assert!(!field.handle_key(&key("down", None)));
        assert!(!field.handle_key(&key("tab", None)));
        assert_eq!(field.value, "abc");
    }

    #[test]
    fn ctrl_a_selects_everything_and_typing_replaces_it() {
        let mut field = TextField::new("hello world");
        assert!(field.handle_key(&key("ctrl-a", None)));
        assert_eq!(field.selection(), Some((0, 11)));
        field.handle_key(&key("x", Some("x")));
        assert_eq!(field.value, "x");
        assert_eq!(field.caret, 1);
    }

    #[test]
    fn shift_left_extends_the_selection() {
        let mut field = TextField::new("abcd");
        field.handle_key(&key("shift-left", None));
        field.handle_key(&key("shift-left", None));
        assert_eq!(field.selection(), Some((2, 4)));
        // A plain arrow collapses to the near end.
        field.handle_key(&key("left", None));
        assert_eq!(field.selection(), None);
        assert_eq!(field.caret, 2);
    }

    #[test]
    fn ctrl_arrows_move_by_word() {
        let mut field = TextField::new("hello brave world");
        // From the end, Ctrl+Left lands on the start of the current word...
        assert!(field.handle_key(&key("ctrl-left", None)));
        assert_eq!(field.caret, 12, "start of 'world'");
        // ...then the previous one, and so on.
        field.handle_key(&key("ctrl-left", None));
        assert_eq!(field.caret, 6, "start of 'brave'");
        field.handle_key(&key("ctrl-left", None));
        assert_eq!(field.caret, 0, "start of 'hello'");
        // Ctrl+Right walks forward to the next word start.
        field.handle_key(&key("ctrl-right", None));
        assert_eq!(field.caret, 6);
    }

    #[test]
    fn ctrl_shift_arrows_extend_by_word() {
        let mut field = TextField::new("hello brave world");
        field.handle_key(&key("ctrl-shift-left", None));
        assert_eq!(field.selection(), Some((12, 17)));
    }

    #[test]
    fn backspace_deletes_the_selection() {
        let mut field = TextField::new("hello world");
        field.handle_key(&key("ctrl-a", None));
        field.handle_key(&key("backspace", None));
        assert_eq!(field.value, "");
    }

    #[test]
    fn word_boundaries_handle_edges() {
        assert_eq!(prev_word("hello brave world", 17), 12);
        assert_eq!(prev_word("hello", 0), 0);
        assert_eq!(next_word("hello", 5), 5);
        assert_eq!(next_word("hello brave", 0), 6);
    }

    #[test]
    fn ctrl_v_pastes_the_clipboard_at_the_caret() {
        let mut field = TextField::new("ab");
        field.handle_key(&key("left", None));
        assert!(field.handle_key_with_clipboard(&key("ctrl-v", None), Some("XY")));
        assert_eq!(field.value, "aXYb");
        assert_eq!(field.caret, 3);
    }

    #[test]
    fn ctrl_v_replaces_the_selection() {
        let mut field = TextField::new("hello world");
        field.handle_key(&key("ctrl-a", None));
        field.handle_key_with_clipboard(&key("ctrl-v", None), Some("pasted"));
        assert_eq!(field.value, "pasted");
    }

    #[test]
    fn ctrl_v_without_clipboard_text_bubbles() {
        let mut field = TextField::new("ab");
        assert!(!field.handle_key_with_clipboard(&key("ctrl-v", None), None));
        assert_eq!(field.value, "ab");
    }
}
