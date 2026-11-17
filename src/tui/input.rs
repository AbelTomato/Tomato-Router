//! Minimal single-line text editor for TUI form fields.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone)]
pub struct TextEdit {
    value: String,
    cursor: usize,
}

impl TextEdit {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.len();
        Self { value, cursor }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn into_value(self) -> String {
        self.value
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> EditOutcome {
        match key.code {
            KeyCode::Enter => EditOutcome::Commit,
            KeyCode::Esc => EditOutcome::Cancel,
            KeyCode::Tab => EditOutcome::Next,
            KeyCode::BackTab => EditOutcome::Previous,
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let previous = previous_boundary(&self.value, self.cursor);
                    self.value.replace_range(previous..self.cursor, "");
                    self.cursor = previous;
                }
                EditOutcome::Continue
            }
            KeyCode::Delete => {
                if self.cursor < self.value.len() {
                    let next = next_boundary(&self.value, self.cursor);
                    self.value.replace_range(self.cursor..next, "");
                }
                EditOutcome::Continue
            }
            KeyCode::Left => {
                self.cursor = previous_boundary(&self.value, self.cursor);
                EditOutcome::Continue
            }
            KeyCode::Right => {
                self.cursor = next_boundary(&self.value, self.cursor);
                EditOutcome::Continue
            }
            KeyCode::Home => {
                self.cursor = 0;
                EditOutcome::Continue
            }
            KeyCode::End => {
                self.cursor = self.value.len();
                EditOutcome::Continue
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert(character);
                EditOutcome::Continue
            }
            _ => EditOutcome::Continue,
        }
    }

    pub fn insert_str(&mut self, text: &str) {
        for character in text.chars().filter(|character| !character.is_control()) {
            self.insert(character);
        }
    }

    fn insert(&mut self, character: char) {
        self.value.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOutcome {
    Continue,
    Commit,
    Cancel,
    Next,
    Previous,
}

fn previous_boundary(value: &str, cursor: usize) -> usize {
    value[..cursor]
        .char_indices()
        .next_back()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn next_boundary(value: &str, cursor: usize) -> usize {
    value[cursor..]
        .chars()
        .next()
        .map(|character| cursor + character.len_utf8())
        .unwrap_or(value.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn edits_unicode_text_by_char_boundaries() {
        let mut edit = TextEdit::new("hé");
        edit.handle_key(key(KeyCode::Backspace));
        assert_eq!(edit.value(), "h");
        edit.insert_str("llo");
        assert_eq!(edit.value(), "hllo");
    }

    #[test]
    fn paste_strips_control_characters() {
        let mut edit = TextEdit::new("");
        edit.insert_str("sk-test\r\n");
        assert_eq!(edit.value(), "sk-test");
    }
}
