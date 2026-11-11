//! Command history and `!` expansion.
//!
//! `!!` (last command), `!N` (entry N), `!-N` (N-th most recent) and
//! `!prefix` (most recent command starting with `prefix`). A `!` inside
//! single quotes, after a backslash, or followed by a space, `=`, `(` or the
//! end of the line is literal.

/// At most this many entries are kept (and saved).
const MAX_ENTRIES: usize = 1000;

#[derive(Debug, Default)]
pub struct History {
    entries: Vec<String>,
}

impl History {
    /// Loads entries from a history file's contents (one command per line).
    pub fn load(&mut self, text: &str) {
        for line in text.lines() {
            self.push(line);
        }
    }

    /// The history file's contents.
    pub fn to_text(&self) -> String {
        self.entries
            .iter()
            .map(|e: &String| -> String { format!("{e}\n") })
            .collect::<String>()
    }

    /// Records a command. Blank lines, lines starting with a space and
    /// repeats of the last entry are skipped.
    pub fn push(&mut self, line: &str) {
        let line: &str = line.trim_end();
        if line.trim().is_empty() || line.starts_with(' ') {
            return;
        }
        if self
            .entries
            .last()
            .is_some_and(|last: &String| -> bool { last == line })
        {
            return;
        }
        self.entries.push(String::from(line));
        if self.entries.len() > MAX_ENTRIES {
            let excess: usize = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..excess);
        }
    }

    /// Oldest first.
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// `history` builtin output: numbered entries.
    pub fn listing(&self) -> String {
        self.entries
            .iter()
            .enumerate()
            .map(|(i, e): (usize, &String)| -> String { format!("{:5}  {e}\n", i + 1) })
            .collect::<String>()
    }

    /// Expands `!` references. Returns `Ok(None)` if the line has none.
    pub fn expand(&self, line: &str) -> Result<Option<String>, String> {
        let chars: Vec<char> = line.chars().collect::<Vec<char>>();
        let mut out: String = String::new();
        let mut changed: bool = false;
        let mut in_single: bool = false;
        let mut i: usize = 0;
        while i < chars.len() {
            let c: char = chars[i];
            match c {
                '\'' => in_single = !in_single,
                '\\' if !in_single && i + 1 < chars.len() => {
                    out.push(c);
                    out.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                '!' if !in_single => {
                    let rest: &[char] = &chars[i + 1..];
                    if let Some((entry, used)) = self.event(rest)? {
                        out.push_str(entry);
                        changed = true;
                        i += 1 + used;
                        continue;
                    }
                }
                _ => {}
            }
            out.push(c);
            i += 1;
        }
        Ok(changed.then_some(out))
    }

    /// Resolves the event designator after a `!`. Returns the entry and how
    /// many chars of `rest` it used, or `None` if this `!` is literal.
    fn event(&self, rest: &[char]) -> Result<Option<(&str, usize)>, String> {
        let Some(&first) = rest.first() else {
            return Ok(None);
        };
        if first.is_whitespace() || matches!(first, '=' | '(' | '"') {
            return Ok(None);
        }
        if first == '!' {
            return self
                .nth_from_end(1, "!!")
                .map(|e: &str| -> Option<(&str, usize)> { Some((e, 1)) });
        }
        let (negative, digits_from): (bool, usize) =
            if first == '-' { (true, 1) } else { (false, 0) };
        let digits: usize = rest[digits_from..]
            .iter()
            .take_while(|c: &&char| -> bool { c.is_ascii_digit() })
            .count();
        if digits > 0 {
            let text: String = rest[digits_from..digits_from + digits]
                .iter()
                .collect::<String>();
            let n: usize = text
                .parse::<usize>()
                .map_err(|e: std::num::ParseIntError| -> String { e.to_string() })?;
            let used: usize = digits_from + digits;
            let designator: String = format!("!{}", rest[..used].iter().collect::<String>());
            let entry: &str = if negative {
                self.nth_from_end(n, &designator)?
            } else {
                n.checked_sub(1)
                    .and_then(|i: usize| -> Option<&String> { self.entries.get(i) })
                    .map(String::as_str)
                    .ok_or_else(|| -> String { format!("{designator}: event not found") })?
            };
            return Ok(Some((entry, used)));
        }
        let len: usize = rest
            .iter()
            .take_while(|c: &&char| -> bool {
                !c.is_whitespace() && !matches!(**c, ';' | '|' | '&' | '<' | '>' | '\'' | '"')
            })
            .count();
        let prefix: String = rest[..len].iter().collect::<String>();
        self.entries
            .iter()
            .rev()
            .find(|e: &&String| -> bool { e.starts_with(&prefix) })
            .map(|e: &String| -> Option<(&str, usize)> { Some((e.as_str(), len)) })
            .ok_or_else(|| -> String { format!("!{prefix}: event not found") })
    }

    fn nth_from_end(&self, n: usize, designator: &str) -> Result<&str, String> {
        self.entries
            .len()
            .checked_sub(n)
            .filter(|_: &usize| -> bool { n > 0 })
            .and_then(|i: usize| -> Option<&String> { self.entries.get(i) })
            .map(String::as_str)
            .ok_or_else(|| -> String { format!("{designator}: event not found") })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> History {
        let mut h: History = History::default();
        h.load("ls -l\necho one\necho one\n git secret\ncat file\n");
        h
    }

    #[test]
    fn push_skips_blank_space_prefixed_and_repeats() {
        assert_eq!(
            history().listing(),
            "    1  ls -l\n    2  echo one\n    3  cat file\n"
        );
    }

    #[test]
    fn bang_expansion() {
        let h: History = history();
        let exp = |line: &str| -> Result<Option<String>, String> { h.expand(line) };
        assert_eq!(exp("!!").unwrap().unwrap(), "cat file");
        assert_eq!(exp("sudo !! | wc").unwrap().unwrap(), "sudo cat file | wc");
        assert_eq!(exp("!1").unwrap().unwrap(), "ls -l");
        assert_eq!(exp("!-2").unwrap().unwrap(), "echo one");
        assert_eq!(exp("!ec;!ls").unwrap().unwrap(), "echo one;ls -l");
        assert_eq!(exp("echo 'hi!' \\!x a! != !").unwrap(), None);
        assert_eq!(exp("!nope").unwrap_err(), "!nope: event not found");
        assert_eq!(exp("!9").unwrap_err(), "!9: event not found");
        assert_eq!(exp("!-0").unwrap_err(), "!-0: event not found");
    }
}
