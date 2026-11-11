//! Lexer, parser and word expansion for iosh's command language.
//!
//! ```text
//! list     := pipeline ((';' | '&&' | '||') pipeline)* [';']
//! pipeline := command ('|' command)*
//! command  := (word | redirect)+
//! redirect := [fd] ('<' | '>' | '>>' | '<<' | '<<-') word | [fd] '>&' fd
//! ```
//!
//! `&&` and `||` have equal precedence and associate left, as in POSIX sh.
//! Words support '...', "..." (with `$` expansion and `\` escapes), `\x`,
//! `$NAME`, `${NAME}`, `$?` and a leading `~`. Expansion happens when a
//! command runs, so `$?` sees the status of the previous command in the list.

use std::collections::VecDeque;

/// One piece of a word, expanded when the command runs.
#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    /// Literal text (quoted or not).
    Lit(String),
    /// `$NAME` or `${NAME}`.
    Var(String),
    /// `$?`: the last exit status.
    Status,
    /// A leading unquoted `~`: `$HOME`.
    Tilde,
}

/// A word before expansion.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Word {
    pub parts: Vec<Part>,
    /// Some part of the word was quoted or escaped.
    pub quoted: bool,
}

impl Word {
    /// The word's source-like text, without expansion (for heredoc
    /// delimiters).
    fn literal(&self) -> String {
        self.parts
            .iter()
            .map(|part: &Part| -> String {
                match part {
                    Part::Lit(s) => s.clone(),
                    Part::Var(name) => format!("${name}"),
                    Part::Status => String::from("$?"),
                    Part::Tilde => String::from("~"),
                }
            })
            .collect::<String>()
    }
}

/// A here-document: its body is read from input after the line is parsed.
#[derive(Clone, Debug, PartialEq)]
pub struct Heredoc {
    pub delimiter: String,
    /// Unquoted delimiter: expand `$` in the body.
    pub expand: bool,
    /// `<<-`: strip leading tabs from body lines and the delimiter line.
    pub strip_tabs: bool,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RedirectKind {
    /// `< file`
    Read(Word),
    /// `> file`
    Write(Word),
    /// `>> file`
    Append(Word),
    /// `<< DELIM`
    Heredoc(Heredoc),
    /// `N>&M`: make this fd a copy of fd `M`.
    Dup(u8),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Redirect {
    /// 0, 1 or 2.
    pub fd: u8,
    pub kind: RedirectKind,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Command {
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pipeline {
    pub commands: Vec<Command>,
}

/// How a pipeline is joined to the one before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connector {
    /// `;` (or the first pipeline): always run.
    Seq,
    /// `&&`: run if the previous status is 0.
    And,
    /// `||`: run if the previous status is non-zero.
    Or,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct List {
    pub items: VecDeque<(Connector, Pipeline)>,
}

impl List {
    /// Every here-document in source order, for filling in bodies.
    pub fn heredocs_mut(&mut self) -> Vec<&mut Heredoc> {
        self.items
            .iter_mut()
            .flat_map(|(_, p): &mut (Connector, Pipeline)| p.commands.iter_mut())
            .flat_map(|c: &mut Command| c.redirects.iter_mut())
            .filter_map(|r: &mut Redirect| -> Option<&mut Heredoc> {
                match &mut r.kind {
                    RedirectKind::Heredoc(h) => Some(h),
                    _ => None,
                }
            })
            .collect::<Vec<&mut Heredoc>>()
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Word(Word),
    Pipe,
    Or,
    And,
    Semi,
    Amp,
    /// A redirection operator with its fd (`2>` etc.).
    Redir(u8, RedirOp),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum RedirOp {
    Read,
    Write,
    Append,
    Heredoc,
    HeredocTabs,
    /// `>&N`
    Dup(u8),
}

/// Parses one input line.
pub fn parse(line: &str) -> Result<List, String> {
    let tokens: Vec<Token> = lex(line)?;
    let mut list: List = List::default();
    let mut connector: Connector = Connector::Seq;
    let mut commands: Vec<Command> = Vec::new();
    let mut cmd: Command = Command::default();
    let mut iter: std::iter::Peekable<std::vec::IntoIter<Token>> = tokens.into_iter().peekable();

    while let Some(token) = iter.next() {
        match token {
            Token::Word(w) => cmd.words.push(w),
            Token::Redir(fd, op) => {
                let kind: RedirectKind = match op {
                    RedirOp::Dup(to) => RedirectKind::Dup(to),
                    _ => {
                        let Some(Token::Word(target)) = iter.next() else {
                            return Err(String::from("expected a word after redirection"));
                        };
                        match op {
                            RedirOp::Read => RedirectKind::Read(target),
                            RedirOp::Write => RedirectKind::Write(target),
                            RedirOp::Append => RedirectKind::Append(target),
                            RedirOp::Heredoc | RedirOp::HeredocTabs => {
                                RedirectKind::Heredoc(Heredoc {
                                    delimiter: target.literal(),
                                    expand: !target.quoted,
                                    strip_tabs: op == RedirOp::HeredocTabs,
                                    body: String::new(),
                                })
                            }
                            RedirOp::Dup(_) => unreachable!(),
                        }
                    }
                };
                cmd.redirects.push(Redirect { fd, kind });
            }
            Token::Pipe => {
                if cmd == Command::default() {
                    return Err(String::from("unexpected `|`"));
                }
                commands.push(std::mem::take(&mut cmd));
            }
            Token::Semi | Token::And | Token::Or => {
                if cmd == Command::default() {
                    let op: &str = match token {
                        Token::Semi => ";",
                        Token::And => "&&",
                        _ => "||",
                    };
                    return Err(format!("unexpected `{op}`"));
                }
                commands.push(std::mem::take(&mut cmd));
                list.items.push_back((
                    connector,
                    Pipeline {
                        commands: std::mem::take(&mut commands),
                    },
                ));
                connector = match token {
                    Token::Semi => Connector::Seq,
                    Token::And => Connector::And,
                    _ => Connector::Or,
                };
                // A trailing `;` is allowed; a trailing `&&`/`||` is not.
                if token == Token::Semi && iter.peek().is_none() {
                    return Ok(list);
                }
            }
            Token::Amp => return Err(String::from("background jobs (`&`) are not supported")),
        }
    }
    if cmd != Command::default() {
        commands.push(cmd);
        list.items.push_back((connector, Pipeline { commands }));
    } else if !commands.is_empty() || !list.items.is_empty() {
        return Err(String::from("unexpected end of line"));
    }
    Ok(list)
}

fn is_operator(c: char) -> bool {
    matches!(c, '|' | '&' | ';' | '<' | '>')
}

fn lex(line: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = line.chars().collect::<Vec<char>>();
    let mut tokens: Vec<Token> = Vec::new();
    let mut i: usize = 0;
    while i < chars.len() {
        let c: char = chars[i];
        let next: Option<char> = chars.get(i + 1).copied();
        match c {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            '#' => break,
            '|' if next == Some('|') => {
                tokens.push(Token::Or);
                i += 2;
            }
            '|' => {
                tokens.push(Token::Pipe);
                i += 1;
            }
            '&' if next == Some('&') => {
                tokens.push(Token::And);
                i += 2;
            }
            '&' => {
                tokens.push(Token::Amp);
                i += 1;
            }
            ';' => {
                tokens.push(Token::Semi);
                i += 1;
            }
            '0'..='2' if matches!(next, Some('<' | '>')) => {
                let fd: u8 = u8::try_from(u32::from(c) - u32::from('0')).unwrap_or(0);
                i += 1;
                tokens.push(redirect(&chars, &mut i, fd)?);
            }
            '<' => tokens.push(redirect(&chars, &mut i, 0)?),
            '>' => tokens.push(redirect(&chars, &mut i, 1)?),
            _ => tokens.push(Token::Word(word(&chars, &mut i)?)),
        }
    }
    Ok(tokens)
}

/// Lexes a redirection operator at `chars[*i]` (`<` or `>`).
fn redirect(chars: &[char], i: &mut usize, fd: u8) -> Result<Token, String> {
    let rest: String = chars[*i..].iter().take(3).collect::<String>();
    let (op, len): (RedirOp, usize) = if rest.starts_with("<<-") {
        (RedirOp::HeredocTabs, 3)
    } else if rest.starts_with("<<") {
        (RedirOp::Heredoc, 2)
    } else if rest.starts_with('<') {
        (RedirOp::Read, 1)
    } else if rest.starts_with(">>") {
        (RedirOp::Append, 2)
    } else if rest.starts_with(">&") {
        match rest.chars().nth(2) {
            Some(d @ '0'..='2') => {
                let to: u8 = u8::try_from(u32::from(d) - u32::from('0')).unwrap_or(0);
                (RedirOp::Dup(to), 3)
            }
            _ => return Err(String::from("`>&` must be followed by 0, 1 or 2")),
        }
    } else {
        (RedirOp::Write, 1)
    };
    *i += len;
    Ok(Token::Redir(fd, op))
}

/// Lexes one word starting at `chars[*i]`.
fn word(chars: &[char], i: &mut usize) -> Result<Word, String> {
    let mut w: Word = Word::default();
    let mut lit: String = String::new();
    let start: usize = *i;
    while *i < chars.len() {
        let c: char = chars[*i];
        if c.is_whitespace() || is_operator(c) {
            break;
        }
        match c {
            '\'' => {
                w.quoted = true;
                *i += 1;
                loop {
                    match chars.get(*i) {
                        None => return Err(String::from("unterminated '")),
                        Some('\'') => break,
                        Some(&q) => lit.push(q),
                    }
                    *i += 1;
                }
                *i += 1;
            }
            '"' => {
                w.quoted = true;
                *i += 1;
                loop {
                    match chars.get(*i) {
                        None => return Err(String::from("unterminated \"")),
                        Some('"') => break,
                        Some('\\') if matches!(chars.get(*i + 1), Some('"' | '\\' | '$' | '`')) => {
                            lit.push(chars[*i + 1]);
                            *i += 2;
                            continue;
                        }
                        Some('$') => {
                            if let Some(part) = variable(chars, i) {
                                flush(&mut w, &mut lit);
                                w.parts.push(part);
                                continue;
                            }
                            lit.push('$');
                        }
                        Some(&q) => lit.push(q),
                    }
                    *i += 1;
                }
                *i += 1;
            }
            '\\' => {
                w.quoted = true;
                match chars.get(*i + 1) {
                    Some(&e) => lit.push(e),
                    None => return Err(String::from("trailing `\\`")),
                }
                *i += 2;
            }
            '$' => {
                if let Some(part) = variable(chars, i) {
                    flush(&mut w, &mut lit);
                    w.parts.push(part);
                } else {
                    lit.push('$');
                    *i += 1;
                }
            }
            '~' if *i == start && matches!(chars.get(*i + 1), None | Some('/')) => {
                w.parts.push(Part::Tilde);
                *i += 1;
            }
            _ => {
                lit.push(c);
                *i += 1;
            }
        }
    }
    flush(&mut w, &mut lit);
    Ok(w)
}

fn flush(w: &mut Word, lit: &mut String) {
    if !lit.is_empty() {
        w.parts.push(Part::Lit(std::mem::take(lit)));
    }
}

/// Parses `$?`, `$NAME` or `${NAME}` at `chars[*i]` (a `$`), advancing past
/// it. Returns `None` (without advancing) if the `$` starts no variable.
fn variable(chars: &[char], i: &mut usize) -> Option<Part> {
    match chars.get(*i + 1) {
        Some('?') => {
            *i += 2;
            Some(Part::Status)
        }
        Some('{') => {
            let close: usize = chars[*i + 2..]
                .iter()
                .position(|c: &char| -> bool { *c == '}' })?;
            let name: String = chars[*i + 2..*i + 2 + close].iter().collect::<String>();
            *i += close + 3;
            Some(Part::Var(name))
        }
        Some(c) if c.is_ascii_alphabetic() || *c == '_' => {
            let len: usize = chars[*i + 1..]
                .iter()
                .take_while(|c: &&char| -> bool { c.is_ascii_alphanumeric() || **c == '_' })
                .count();
            let name: String = chars[*i + 1..*i + 1 + len].iter().collect::<String>();
            *i += len + 1;
            Some(Part::Var(name))
        }
        _ => None,
    }
}

/// What expansion needs from the shell.
pub trait Env {
    /// The value of a variable (shell overrides, then the environment).
    fn var(&self, name: &str) -> Option<String>;
    /// The last exit status.
    fn status(&self) -> i32;
}

/// Expands a word to a single argument: no field splitting or globbing.
pub fn expand(word: &Word, env: &dyn Env) -> String {
    word.parts
        .iter()
        .map(|part: &Part| -> String {
            match part {
                Part::Lit(s) => s.clone(),
                Part::Var(name) => env.var(name).unwrap_or_default(),
                Part::Status => env.status().to_string(),
                Part::Tilde => env
                    .var("HOME")
                    .unwrap_or_else(|| -> String { String::from("~") }),
            }
        })
        .collect::<String>()
}

/// Expands a command's words into argv. Unquoted words that expand to
/// nothing are dropped, as in sh.
pub fn expand_argv(words: &[Word], env: &dyn Env) -> Vec<String> {
    words
        .iter()
        .filter_map(|w: &Word| -> Option<String> {
            let arg: String = expand(w, env);
            (w.quoted || !arg.is_empty()).then_some(arg)
        })
        .collect::<Vec<String>>()
}

/// Expands `$` variables (and `\$`, `\\`) in a here-document body.
pub fn expand_text(text: &str, env: &dyn Env) -> String {
    let chars: Vec<char> = text.chars().collect::<Vec<char>>();
    let mut out: String = String::new();
    let mut i: usize = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if matches!(chars.get(i + 1), Some('$' | '\\')) => {
                out.push(chars[i + 1]);
                i += 2;
            }
            '$' => match variable(&chars, &mut i) {
                Some(part) => {
                    let w: Word = Word {
                        parts: vec![part],
                        quoted: false,
                    };
                    out.push_str(&expand(&w, env));
                }
                None => {
                    out.push('$');
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct TestEnv(HashMap<&'static str, &'static str>);

    impl Env for TestEnv {
        fn var(&self, name: &str) -> Option<String> {
            self.0
                .get(name)
                .map(|v: &&str| -> String { String::from(*v) })
        }
        fn status(&self) -> i32 {
            7
        }
    }

    fn env() -> TestEnv {
        TestEnv(HashMap::from([("HOME", "/home/me"), ("X", "ex")]))
    }

    fn argv(line: &str) -> Vec<Vec<String>> {
        let list: List = parse(line).unwrap();
        list.items
            .iter()
            .flat_map(|(_, p): &(Connector, Pipeline)| p.commands.iter())
            .map(|c: &Command| -> Vec<String> { expand_argv(&c.words, &env()) })
            .collect::<Vec<Vec<String>>>()
    }

    #[test]
    fn words_quotes_and_expansion() {
        assert_eq!(
            argv(r#"echo 'a b' "c $X" d\ e $X${X}$? ~/f "~" $NOPE "" # comment"#),
            vec![vec![
                "echo",
                "a b",
                "c ex",
                "d e",
                "exex7",
                "/home/me/f",
                "~",
                ""
            ]]
        );
    }

    #[test]
    fn lists_and_pipelines() {
        let list: List = parse("a | b && c || d; e;").unwrap();
        let shape: Vec<(Connector, usize)> = list
            .items
            .iter()
            .map(|(c, p): &(Connector, Pipeline)| -> (Connector, usize) { (*c, p.commands.len()) })
            .collect::<Vec<(Connector, usize)>>();
        assert_eq!(
            shape,
            vec![
                (Connector::Seq, 2),
                (Connector::And, 1),
                (Connector::Or, 1),
                (Connector::Seq, 1)
            ]
        );
        assert_eq!(parse("   ").unwrap(), List::default());
    }

    #[test]
    fn redirections() {
        let list: List = parse("cmd <in >out 2>>err 2>&1 >>app <<-'EOF'").unwrap();
        let redirects: &Vec<Redirect> = &list.items[0].1.commands[0].redirects;
        let lit = |s: &str| -> Word {
            Word {
                parts: vec![Part::Lit(String::from(s))],
                quoted: false,
            }
        };
        assert_eq!(
            redirects[0],
            Redirect {
                fd: 0,
                kind: RedirectKind::Read(lit("in"))
            }
        );
        assert_eq!(
            redirects[1],
            Redirect {
                fd: 1,
                kind: RedirectKind::Write(lit("out"))
            }
        );
        assert_eq!(
            redirects[2],
            Redirect {
                fd: 2,
                kind: RedirectKind::Append(lit("err"))
            }
        );
        assert_eq!(
            redirects[3],
            Redirect {
                fd: 2,
                kind: RedirectKind::Dup(1)
            }
        );
        assert_eq!(
            redirects[4],
            Redirect {
                fd: 1,
                kind: RedirectKind::Append(lit("app"))
            }
        );
        let RedirectKind::Heredoc(h) = &redirects[5].kind else {
            panic!()
        };
        assert_eq!(
            (h.delimiter.as_str(), h.expand, h.strip_tabs),
            ("EOF", false, true)
        );
    }

    #[test]
    fn syntax_errors() {
        for bad in [
            "| a", "a |", "a &&", "&& a", "a ; ; b", "a &", "echo 'x", "cat <", "a >&x",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn heredoc_body_expansion() {
        assert_eq!(expand_text("$X \\$X ${X}! $?\n", &env()), "ex $X ex! 7\n");
    }
}
