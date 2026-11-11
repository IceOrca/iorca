//! A small line editor for the interactive prompt.
//!
//! It is pure: [`Editor::feed`] takes the raw bytes the terminal sent (in
//! non-canonical mode) plus the history, and returns what to write back
//! (the redrawn line) and whether the line is finished. The shell owns the
//! terminal mode and the I/O.
//!
//! Keys: ←/→ (Ctrl+B/F), Home/End (Ctrl+A/E), Ctrl+←/→ (word), ↑/↓ (Ctrl+P/N)
//! for history, Backspace, Delete, Ctrl+U/K/W (kill to start / to end /
//! previous word), Ctrl+D (exit on an empty line, else delete), Ctrl+L
//! (clear screen), Enter. Ctrl+C, Ctrl+\ and Ctrl+Z still raise signals.
//!
//! Lines longer than the terminal width are not handled specially.

/// A decoded key press.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    WordLeft,
    WordRight,
    Up,
    Down,
    Home,
    End,
    KillToStart,
    KillToEnd,
    KillWord,
    CtrlD,
    ClearScreen,
    /// Anything else (Tab, unknown escape sequences, Alt+key).
    Ignored,
}

/// How an editing session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Finish {
    /// Enter: the line.
    Line(String),
    /// Ctrl+D on an empty line (or end of input).
    Eof,
}

/// The result of [`Editor::feed`].
#[derive(Debug, Default)]
pub struct Fed {
    /// Bytes to write to the terminal.
    pub output: Vec<u8>,
    pub finish: Option<Finish>,
    /// Bytes after the end of the line (typed ahead or pasted), for the next
    /// session.
    pub rest: Vec<u8>,
}

/// One line being edited.
#[derive(Debug)]
pub struct Editor {
    prompt: String,
    line: Vec<char>,
    /// Cursor position in `line`, in chars.
    cursor: usize,
    /// Index of the history entry shown, while browsing with ↑/↓.
    browsing: Option<usize>,
    /// The line being typed before browsing started.
    draft: Vec<char>,
    /// An incomplete escape sequence or UTF-8 character.
    pending: Vec<u8>,
}

impl Editor {
    pub fn new(prompt: String) -> Self {
        Self {
            prompt,
            line: Vec::new(),
            cursor: 0,
            browsing: None,
            draft: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// The initial screen output: the prompt.
    pub fn start(&self) -> Vec<u8> {
        self.render()
    }

    /// Processes input bytes. Stops at the end of the line; later bytes are
    /// returned in [`Fed::rest`].
    pub fn feed(&mut self, bytes: &[u8], history: &[String]) -> Fed {
        let mut input: Vec<u8> = std::mem::take(&mut self.pending);
        input.extend_from_slice(bytes);
        let mut fed: Fed = Fed::default();
        let mut i: usize = 0;
        let mut dirty: bool = false;
        while i < input.len() {
            let Some((key, used)) = decode(&input[i..]) else {
                // Incomplete: wait for the rest.
                self.pending = input[i..].to_vec();
                break;
            };
            i += used;
            match self.apply(key, history) {
                Step::Nothing => {}
                Step::Redraw => dirty = true,
                Step::Clear => {
                    fed.output.extend_from_slice(b"\x1b[H\x1b[2J");
                    dirty = true;
                }
                Step::Finish(finish) => {
                    if matches!(finish, Finish::Line(_)) {
                        self.cursor = self.line.len();
                        fed.output.extend_from_slice(&self.render());
                        fed.output.extend_from_slice(b"\r\n");
                        dirty = false;
                    }
                    fed.finish = Some(finish);
                    fed.rest = input[i..].to_vec();
                    break;
                }
            }
        }
        if dirty {
            fed.output.extend_from_slice(&self.render());
        }
        fed
    }

    fn apply(&mut self, key: Key, history: &[String]) -> Step {
        match key {
            Key::Char(c) => {
                self.line.insert(self.cursor, c);
                self.cursor += 1;
            }
            Key::Enter => {
                return Step::Finish(Finish::Line(self.line.iter().collect::<String>()));
            }
            Key::CtrlD if self.line.is_empty() => return Step::Finish(Finish::Eof),
            Key::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.line.remove(self.cursor);
            }
            Key::Delete | Key::CtrlD if self.cursor < self.line.len() => {
                self.line.remove(self.cursor);
            }
            Key::Left if self.cursor > 0 => self.cursor -= 1,
            Key::Right if self.cursor < self.line.len() => self.cursor += 1,
            Key::WordLeft => self.cursor = self.word_start(),
            Key::WordRight => self.cursor = self.word_end(),
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = self.line.len(),
            Key::KillToStart => {
                self.line.drain(..self.cursor);
                self.cursor = 0;
            }
            Key::KillToEnd => self.line.truncate(self.cursor),
            Key::KillWord => {
                let start: usize = self.word_start();
                self.line.drain(start..self.cursor);
                self.cursor = start;
            }
            Key::Up => {
                let index: usize = match self.browsing {
                    None if !history.is_empty() => {
                        self.draft = std::mem::take(&mut self.line);
                        history.len() - 1
                    }
                    Some(i) if i > 0 => i - 1,
                    _ => return Step::Nothing,
                };
                self.show(Some(index), history);
            }
            Key::Down => match self.browsing {
                Some(i) if i + 1 < history.len() => self.show(Some(i + 1), history),
                Some(_) => self.show(None, history),
                None => return Step::Nothing,
            },
            Key::ClearScreen => return Step::Clear,
            _ => return Step::Nothing,
        }
        Step::Redraw
    }

    /// Shows history entry `index`, or the draft for `None`.
    fn show(&mut self, index: Option<usize>, history: &[String]) {
        self.browsing = index;
        self.line = match index {
            Some(i) => history[i].chars().collect::<Vec<char>>(),
            None => std::mem::take(&mut self.draft),
        };
        self.cursor = self.line.len();
    }

    /// Start of the word before the cursor.
    fn word_start(&self) -> usize {
        let mut i: usize = self.cursor;
        while i > 0 && self.line[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.line[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    /// End of the word after the cursor.
    fn word_end(&self) -> usize {
        let mut i: usize = self.cursor;
        while i < self.line.len() && self.line[i].is_whitespace() {
            i += 1;
        }
        while i < self.line.len() && !self.line[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Redraws the prompt and line, and puts the cursor in place.
    fn render(&self) -> Vec<u8> {
        let line: String = self.line.iter().collect::<String>();
        let mut out: String = format!("\r{}{line}\x1b[K", self.prompt);
        let back: usize = self.line.len() - self.cursor;
        if back > 0 {
            out.push_str(&format!("\x1b[{back}D"));
        }
        out.into_bytes()
    }

    #[cfg(test)]
    fn state(&self) -> (String, usize) {
        (self.line.iter().collect::<String>(), self.cursor)
    }
}

enum Step {
    Nothing,
    Redraw,
    Clear,
    Finish(Finish),
}

/// Decodes one key from the start of `bytes`: the key and the bytes used,
/// or `None` if `bytes` is an incomplete sequence.
fn decode(bytes: &[u8]) -> Option<(Key, usize)> {
    let first: u8 = *bytes.first()?;
    let key: Key = match first {
        b'\r' | b'\n' => Key::Enter,
        0x7f | 0x08 => Key::Backspace,
        0x01 => Key::Home,
        0x02 => Key::Left,
        0x04 => Key::CtrlD,
        0x05 => Key::End,
        0x06 => Key::Right,
        0x0b => Key::KillToEnd,
        0x0c => Key::ClearScreen,
        0x0e => Key::Down,
        0x10 => Key::Up,
        0x15 => Key::KillToStart,
        0x17 => Key::KillWord,
        0x1b => return escape(bytes),
        0x00..=0x1f => Key::Ignored,
        0x20..=0x7e => Key::Char(char::from(first)),
        _ => return utf8(bytes),
    };
    Some((key, 1))
}

/// Decodes `ESC ...`: CSI (`ESC [`) and SS3 (`ESC O`) sequences.
fn escape(bytes: &[u8]) -> Option<(Key, usize)> {
    match bytes.get(1)? {
        b'[' => {
            // Parameters, then a final byte in 0x40..=0x7e.
            let end: usize = bytes[2..]
                .iter()
                .position(|b: &u8| -> bool { (0x40..=0x7e).contains(b) })?
                + 2;
            let params: &[u8] = &bytes[2..end];
            let key: Key = match (params, bytes[end]) {
                (b"" | b"1", b'A') => Key::Up,
                (b"" | b"1", b'B') => Key::Down,
                (b"" | b"1", b'C') => Key::Right,
                (b"" | b"1", b'D') => Key::Left,
                (b"1;5" | b"1;3", b'C') => Key::WordRight,
                (b"1;5" | b"1;3", b'D') => Key::WordLeft,
                (b"" | b"1", b'H') | (b"1" | b"7", b'~') => Key::Home,
                (b"" | b"1", b'F') | (b"4" | b"8", b'~') => Key::End,
                (b"3", b'~') => Key::Delete,
                _ => Key::Ignored,
            };
            Some((key, end + 1))
        }
        b'O' => {
            let key: Key = match bytes.get(2)? {
                b'A' => Key::Up,
                b'B' => Key::Down,
                b'C' => Key::Right,
                b'D' => Key::Left,
                b'H' => Key::Home,
                b'F' => Key::End,
                _ => Key::Ignored,
            };
            Some((key, 3))
        }
        // Alt+key: ignored.
        _ => Some((Key::Ignored, 2)),
    }
}

/// Decodes one multi-byte UTF-8 character.
fn utf8(bytes: &[u8]) -> Option<(Key, usize)> {
    let len: usize = match bytes[0] {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return Some((Key::Ignored, 1)),
    };
    let seq: &[u8] = bytes.get(..len)?;
    match std::str::from_utf8(seq)
        .ok()
        .and_then(|s: &str| -> Option<char> { s.chars().next() })
    {
        Some(c) => Some((Key::Char(c), len)),
        None => Some((Key::Ignored, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> Vec<String> {
        vec![String::from("echo one"), String::from("ls -l")]
    }

    /// Feeds `input`, expecting the line to stay open.
    fn edit(editor: &mut Editor, input: &[u8]) -> (String, usize) {
        let fed: Fed = editor.feed(input, &history());
        assert_eq!(fed.finish, None);
        editor.state()
    }

    #[test]
    fn typing_and_cursor_movement() {
        let mut e: Editor = Editor::new(String::from("$ "));
        assert_eq!(edit(&mut e, b"helo"), (String::from("helo"), 4));
        assert_eq!(edit(&mut e, b"\x1b[D\x1b[Dl"), (String::from("hello"), 3));
        assert_eq!(edit(&mut e, b"\x01X\x05Y"), (String::from("XhelloY"), 7));
        assert_eq!(
            edit(&mut e, b"\x7f\x1b[H\x1b[3~"),
            (String::from("hello"), 0)
        );
        assert_eq!(
            edit(&mut e, "é ü".as_bytes()),
            (String::from("é ühello"), 3)
        );
    }

    #[test]
    fn kill_and_word_keys() {
        let mut e: Editor = Editor::new(String::new());
        assert_eq!(
            edit(&mut e, b"git commit -m"),
            (String::from("git commit -m"), 13)
        );
        assert_eq!(edit(&mut e, b"\x17"), (String::from("git commit "), 11));
        assert_eq!(
            edit(&mut e, b"\x1b[1;5D\x1b[1;5D"),
            (String::from("git commit "), 0)
        );
        assert_eq!(edit(&mut e, b"\x1b[1;5C\x0b"), (String::from("git"), 3));
        assert_eq!(edit(&mut e, b"\x02\x15"), (String::from("t"), 0));
    }

    #[test]
    fn history_browsing_keeps_the_draft() {
        let mut e: Editor = Editor::new(String::new());
        edit(&mut e, b"dra");
        assert_eq!(edit(&mut e, b"\x1b[A"), (String::from("ls -l"), 5));
        assert_eq!(edit(&mut e, b"\x1b[A\x1b[A"), (String::from("echo one"), 8));
        assert_eq!(edit(&mut e, b"\x1bOB"), (String::from("ls -l"), 5));
        assert_eq!(edit(&mut e, b"\x0e"), (String::from("dra"), 3));
        assert_eq!(edit(&mut e, b"\x0e"), (String::from("dra"), 3));
        let fed: Fed = e.feed(b"\x10 x\nnext", &history());
        assert_eq!(fed.finish, Some(Finish::Line(String::from("ls -l x"))));
        assert_eq!(fed.rest, b"next");
        assert!(fed.output.ends_with(b"ls -l x\x1b[K\r\n"));
    }

    #[test]
    fn split_sequences_and_ctrl_d() {
        let mut e: Editor = Editor::new(String::new());
        // An escape sequence and a UTF-8 char split across reads.
        assert_eq!(edit(&mut e, b"ab\x1b"), (String::from("ab"), 2));
        assert_eq!(edit(&mut e, b"[D\xc3"), (String::from("ab"), 1));
        assert_eq!(edit(&mut e, b"\xa9"), (String::from("aéb"), 2));
        // Ctrl+D deletes under the cursor, and ends input on an empty line.
        assert_eq!(edit(&mut e, b"\x04\x01\x0b"), (String::new(), 0));
        assert_eq!(e.feed(b"\x04", &history()).finish, Some(Finish::Eof));
    }

    #[test]
    fn render_places_the_cursor() {
        let mut e: Editor = Editor::new(String::from("> "));
        assert_eq!(e.start(), b"\r> \x1b[K");
        let fed: Fed = e.feed(b"abc\x1b[D\x1b[D", &history());
        assert_eq!(fed.output, b"\r> abc\x1b[K\x1b[2D");
    }
}
