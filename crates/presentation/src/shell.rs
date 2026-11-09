//! Display-only shell tokens and RTK routing elision, shared by both frontends.
//!
//! This is a tolerant lexer, not a shell parser. Byte ranges always refer to the
//! supplied text; execution, model text and clipboard payloads must use the source.
use std::{collections::VecDeque, iter::Peekable, ops::Range, str::CharIndices};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellToken {
    Program,
    Flag,
    Number,
    Variable,
    String,
    Operator,
    Keyword,
    Comment,
    Assignment,
    Routing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellDisplay {
    pub text: String,
    pub spans: Vec<(Range<usize>, ShellToken)>,
}

/// Hide only confirmed routing words and their following whitespace, rebasing
/// all other highlights onto the displayed text. Never use this for copying.
pub fn display(command: &str) -> ShellDisplay {
    let mut text = String::with_capacity(command.len());
    let mut spans = Vec::new();
    let mut cursor = 0;
    for (range, token) in tokens(command) {
        text.push_str(&command[cursor..range.start]);
        if token == ShellToken::Routing {
            cursor = range.end
                + command[range.end..]
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .map(char::len_utf8)
                    .sum::<usize>();
        } else {
            let start = text.len();
            text.push_str(&command[range.clone()]);
            spans.push((start..text.len(), token));
            cursor = range.end;
        }
    }
    text.push_str(&command[cursor..]);
    ShellDisplay { text, spans }
}

/// Highlight unchanged shell text, marking routing words in comment ink.
/// Incomplete/streaming input is accepted, including every UTF-8 prefix.
pub fn tokens(command: &str) -> Vec<(Range<usize>, ShellToken)> {
    Lexer::new(command).lex()
}

struct Part {
    range: Range<usize>,
    token: Option<ShellToken>,
}
struct Routing {
    parts: Vec<usize>,
    subcommand: bool,
}
struct Heredoc {
    delimiter: String,
    strip_tabs: bool,
}
struct Lexer<'a> {
    input: &'a str,
    chars: Peekable<CharIndices<'a>>,
    parts: Vec<Part>,
    command_position: bool,
    routing: Option<Routing>,
    redirect_target: bool,
    heredoc_target: Option<bool>,
    heredocs: VecDeque<Heredoc>,
    backtick: bool,
}
impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            chars: input.char_indices().peekable(),
            parts: Vec::new(),
            command_position: true,
            routing: None,
            redirect_target: false,
            heredoc_target: None,
            heredocs: VecDeque::new(),
            backtick: false,
        }
    }
    fn pos(&mut self) -> usize {
        self.chars.peek().map_or(self.input.len(), |&(i, _)| i)
    }
    fn peek(&mut self) -> Option<char> {
        self.chars.peek().map(|&(_, c)| c)
    }
    fn starts(&mut self, text: &str) -> bool {
        let pos = self.pos();
        self.input[pos..].starts_with(text)
    }
    fn advance(&mut self, bytes: usize) {
        let end = self.pos() + bytes;
        while self.pos() < end {
            self.chars.next();
        }
    }
    fn part(&mut self, range: Range<usize>, token: Option<ShellToken>) -> usize {
        let index = self.parts.len();
        if !range.is_empty() {
            self.parts.push(Part { range, token });
        }
        index
    }
    fn lex(mut self) -> Vec<(Range<usize>, ShellToken)> {
        while let Some(c) = self.peek() {
            let start = self.pos();
            if self.starts("\\\n") {
                self.advance(2);
                continue;
            }
            if c.is_whitespace() && c != '\n' {
                self.chars.next();
                continue;
            }
            if c == '#' {
                while self.peek().is_some_and(|c| c != '\n') {
                    self.chars.next();
                }
                let end = self.pos();
                self.part(start..end, Some(ShellToken::Comment));
                self.routing = None;
                continue;
            }
            if let Some((bytes, heredoc)) = self.redirection() {
                self.advance(bytes);
                let end = self.pos();
                self.part(start..end, Some(ShellToken::Operator));
                self.redirect_target = true;
                self.heredoc_target = heredoc;
                continue;
            }
            if let Some(separator) = [
                "$(", ";;", "&&", "||", "|&", "\n", ";", "&", "|", "(", ")", "`",
            ]
            .into_iter()
            .find(|s| self.starts(s) && !(*s == "$(" && self.starts("$((")))
            {
                self.advance(separator.len());
                let end = self.pos();
                self.part(start..end, Some(ShellToken::Operator));
                self.routing = None;
                self.redirect_target = false;
                self.heredoc_target = None;
                self.command_position = match separator {
                    ")" => false,
                    "`" => {
                        self.backtick = !self.backtick;
                        self.backtick
                    }
                    _ => true,
                };
                if separator == "\n" {
                    self.heredoc_bodies();
                }
                continue;
            }
            self.word();
        }
        self.parts
            .into_iter()
            .filter_map(|part| part.token.map(|token| (part.range, token)))
            .collect()
    }
    /// A redirection may start with a file descriptor, but never consumes its
    /// target. That target must not accidentally become the next program.
    fn redirection(&mut self) -> Option<(usize, Option<bool>)> {
        let start = self.pos();
        let fd = self.input[start..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        let rest = &self.input[start + fd..];
        let op = [
            "&>>", "<<<", "<<-", ">>", "<<", ">&", "<&", "&>", ">|", ">", "<",
        ]
        .into_iter()
        .find(|op| rest.starts_with(op))?;
        let heredoc = match op {
            "<<" => Some(false),
            "<<-" => Some(true),
            _ => None,
        };
        Some((fd + op.len(), heredoc))
    }
    fn word(&mut self) {
        let start = self.pos();
        let mut segments = Vec::new();
        while let Some(c) = self.peek() {
            if c.is_whitespace()
                || self.starts("\\\n")
                || matches!(c, ';' | '&' | '|' | '(' | ')' | '`' | '<' | '>')
            {
                break;
            }
            let at = self.pos();
            if c == '\\' {
                self.chars.next();
                self.chars.next();
            } else if c == '\'' || c == '"' || self.starts("$'") {
                if c == '$' {
                    self.chars.next();
                }
                let quote = self.peek().unwrap_or('\'');
                self.quoted(quote, c == '$' || quote == '"');
                segments.push((at..self.pos(), ShellToken::String));
            } else if c == '$' {
                if self.starts("$(") && !self.starts("$((") {
                    break;
                }
                if self.variable() {
                    segments.push((at..self.pos(), ShellToken::Variable));
                }
            } else {
                self.chars.next();
            }
        }
        let end = self.pos();
        let word = &self.input[start..end];
        let word_token = self.classify(word);
        let first = self.parts.len();
        let mut cursor = start;
        for (range, token) in segments {
            self.part(cursor..range.start, word_token);
            cursor = range.end;
            self.part(range, Some(token));
        }
        self.part(cursor..end, word_token);
        // Routing candidates are always bare words (no quoted sub-spans), so
        // each has one part, even when it is an otherwise unstyled argument.
        if !self.redirect_target && self.routing.is_none() && self.command_position && word == "rtk"
        {
            self.routing = Some(Routing {
                parts: vec![first],
                subcommand: false,
            });
            self.command_position = false;
        } else if !self.redirect_target
            && let Some(routing) = &mut self.routing
        {
            routing.parts.push(first);
            if matches!(word, "proxy" | "run") {
                routing.subcommand = true;
            }
        }
        if self.redirect_target {
            if let Some(strip_tabs) = self.heredoc_target.take() {
                self.heredocs.push_back(Heredoc {
                    delimiter: unquote(word),
                    strip_tabs,
                });
            }
            self.redirect_target = false;
        } else if self.routing.is_none() {
            self.command_position = word_token == Some(ShellToken::Assignment)
                || (word_token == Some(ShellToken::Keyword) && continues_command(word));
        }
    }
    fn classify(&mut self, word: &str) -> Option<ShellToken> {
        if self.redirect_target {
            return argument(word);
        }
        if let Some(routing) = self.routing.take() {
            if !routing.subcommand && (global_option(word) || matches!(word, "proxy" | "run")) {
                self.routing = Some(routing);
                return argument(word);
            }
            if !word.starts_with('-') {
                for part in routing.parts {
                    self.parts[part].token = Some(ShellToken::Routing);
                }
                self.command_position = true;
            }
        }
        if self.command_position {
            if assignment(word) {
                Some(ShellToken::Assignment)
            } else if keyword(word) {
                Some(ShellToken::Keyword)
            } else {
                Some(ShellToken::Program)
            }
        } else {
            argument(word)
        }
    }
    fn quoted(&mut self, quote: char, escapes: bool) {
        self.chars.next();
        while let Some((_, c)) = self.chars.next() {
            if c == quote {
                break;
            }
            if c == '\\' && escapes {
                self.chars.next();
            }
        }
    }
    fn variable(&mut self) -> bool {
        self.chars.next(); // $
        if self.starts("((") {
            self.balanced('(', ')');
        } else if self.peek() == Some('{') {
            self.balanced('{', '}');
        } else if self
            .peek()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            while self
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                self.chars.next();
            }
        } else if self
            .peek()
            .is_some_and(|c| c.is_ascii_digit() || "@*#?$!-".contains(c))
        {
            self.chars.next();
        } else {
            return false;
        }
        true
    }
    fn balanced(&mut self, open: char, close: char) {
        let mut depth = 0usize;
        while let Some((_, c)) = self.chars.next() {
            if c == open {
                depth += 1;
            } else if c == close {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            } else if c == '\\' {
                self.chars.next();
            }
        }
    }
    fn heredoc_bodies(&mut self) {
        while let Some(heredoc) = self.heredocs.pop_front() {
            let start = self.pos();
            while self.peek().is_some() {
                let line_start = self.pos();
                while self.peek().is_some_and(|c| c != '\n') {
                    self.chars.next();
                }
                let line_end = self.pos();
                let line = self.input[line_start..line_end].trim_end_matches('\r');
                let line = if heredoc.strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line
                };
                let done = line == heredoc.delimiter;
                if self.peek() == Some('\n') {
                    self.chars.next();
                }
                if done {
                    break;
                }
            }
            let end = self.pos();
            self.part(start..end, Some(ShellToken::String));
        }
    }
}
fn argument(word: &str) -> Option<ShellToken> {
    if word.starts_with('-') {
        Some(ShellToken::Flag)
    } else if word.bytes().any(|c| c.is_ascii_digit())
        && word.bytes().all(|c| c.is_ascii_digit() || c == b'.')
        && word.bytes().filter(|&c| c == b'.').count() <= 1
    {
        Some(ShellToken::Number)
    } else {
        None
    }
}
fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        let mut chars = name.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}
fn keyword(word: &str) -> bool {
    matches!(
        word,
        "if" | "then"
            | "else"
            | "elif"
            | "do"
            | "while"
            | "until"
            | "!"
            | "time"
            | "{"
            | "}"
            | "fi"
            | "done"
            | "for"
            | "select"
            | "in"
            | "case"
            | "esac"
            | "function"
            | "coproc"
            | "[["
            | "]]"
    )
}
fn continues_command(word: &str) -> bool {
    matches!(
        word,
        "if" | "then" | "else" | "elif" | "do" | "while" | "until" | "!" | "time" | "{"
    )
}
fn global_option(word: &str) -> bool {
    matches!(
        word,
        "-v" | "-vv" | "-vvv" | "--verbose" | "--ultra-compact" | "--skip-env"
    )
}
fn unquote(word: &str) -> String {
    let mut result = String::new();
    let mut quote = None;
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' if quote == Some(c) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(c),
            '\\' if quote != Some('\'') => result.extend(chars.next()),
            _ => result.push(c),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLES: &[&str] = &[
        "rtk git status",
        "rtk proxy python3 -c 'import json; fr",
        "rtk -v --ultra-compact cargo test",
        "cd a && rtk rg -n \"x\" src | head -20",
        "FOO=1 rtk cargo test",
        "if rtk test -f x; then rtk cargo build; fi",
        "rtk proxy cat <<'EOF'\nrtk cargo test\nEOF\nrtk echo done",
        "rtk",
        "rtk --version",
        "echo rtk",
        "./rtk",
        "rtk | cat",
        "RTK",
        "rtk -vvv --skip-env run cargo test >out 2>&1 & rtk echo $NAME $1 $@ ${name:-🐚} $((1 + 2)) # rtk comment",
        "(rtk git status); echo $(rtk pwd) `rtk date`",
        "echo --title='a b' $'c\\'d' \"unterminated 🖉",
        "rtk cat <<-END\n\tこんにちは\n\tEND\nrtk echo café",
        "rtk cat <<A <<\"B\"\none\nA\ntwo\nB\nrtk echo done",
        "rtk \\\n git log\necho ${unfinished",
        "echo $((unterminated",
        ">out FOO=1 rtk git status |& rtk run cat <in >>out",
        "rtk >out proxy <in git status",
        "rtk\\\n git status >|out 0<&3 2>&1 &>>out",
        "rtk >out # no program follows",
        "echo 中文-👩🏽‍💻 \\#not-comment # 注释\nrtk -v",
    ];
    #[test]
    fn strips_only_confirmed_routing_at_command_positions() {
        for (command, expected) in [
            (SAMPLES[0], "git status"),
            (SAMPLES[1], "python3 -c 'import json; fr"),
            (SAMPLES[2], "cargo test"),
            (SAMPLES[3], "cd a && rg -n \"x\" src | head -20"),
            (SAMPLES[4], "FOO=1 cargo test"),
            (SAMPLES[5], "if test -f x; then cargo build; fi"),
            (SAMPLES[6], "cat <<'EOF'\nrtk cargo test\nEOF\necho done"),
            ("rtk --verbose --skip-env run git status", "git status"),
            ("rtk -vv proxy git status", "git status"),
            (
                "(rtk pwd); echo $(rtk pwd) `rtk date`",
                "(pwd); echo $(pwd) `date`",
            ),
            (
                "rtk a || rtk b ;; rtk c & rtk d\nrtk e",
                "a || b ;; c & d\ne",
            ),
            (">out FOO=1 rtk git status", ">out FOO=1 git status"),
            ("rtk >out proxy <in git status", ">out <in git status"),
        ] {
            assert_eq!(display(command).text, expected, "{command:?}");
        }
    }
    #[test]
    fn leaves_commands_arguments_and_incomplete_routing_exact() {
        for command in [
            "rtk",
            "rtk --version",
            "echo rtk",
            "./rtk",
            "rtk | cat",
            "RTK",
            "rtk -v",
            "rtk proxy",
            "rtk run",
            "rtk --help x",
            "rtk\ngit status",
            "rtk # comment\ngit",
            "rtk >out",
        ] {
            assert_eq!(display(command).text, command);
            assert!(
                tokens(command)
                    .iter()
                    .all(|(_, token)| *token != ShellToken::Routing)
            );
        }
    }
    #[test]
    fn shell_inks_cover_redirections_expansions_comments_and_quoted_segments() {
        let command = "FOO=1 rtk proxy cmd --title='a b' -n 42 2>&1 &>>out <<<x $NAME $1 $@ ${v:-x} $((1 + 2)) # note";
        let spans = tokens(command);
        let has = |text, token| {
            spans
                .iter()
                .any(|(r, t)| &command[r.clone()] == text && *t == token)
        };
        for (text, token) in [
            ("FOO=1", ShellToken::Assignment),
            ("rtk", ShellToken::Routing),
            ("proxy", ShellToken::Routing),
            ("cmd", ShellToken::Program),
            ("--title=", ShellToken::Flag),
            ("'a b'", ShellToken::String),
            ("42", ShellToken::Number),
            ("2>&", ShellToken::Operator),
            ("1", ShellToken::Number),
            ("&>>", ShellToken::Operator),
            ("<<<", ShellToken::Operator),
            ("$NAME", ShellToken::Variable),
            ("$1", ShellToken::Variable),
            ("$@", ShellToken::Variable),
            ("${v:-x}", ShellToken::Variable),
            ("$((1 + 2))", ShellToken::Variable),
            ("# note", ShellToken::Comment),
        ] {
            assert!(has(text, token), "missing {token:?}: {text}");
        }
        assert!(
            !spans
                .iter()
                .any(|(r, t)| &command[r.clone()] == "out" && *t == ShellToken::Program)
        );
        let command = "rtk cat <<'EOF'\nrtk cargo test\nEOF\nrtk echo done";
        assert!(tokens(command).iter().any(
            |(r, t)| *t == ShellToken::String && &command[r.clone()] == "rtk cargo test\nEOF\n"
        ));
    }
    fn valid_spans(text: &str, spans: &[(Range<usize>, ShellToken)]) {
        let mut end = 0;
        for (range, _) in spans {
            assert!(
                range.start >= end && range.start < range.end && range.end <= text.len(),
                "{text:?}: {spans:?}"
            );
            assert!(text.is_char_boundary(range.start) && text.is_char_boundary(range.end));
            end = range.end;
        }
    }
    #[test]
    fn all_utf8_prefixes_have_valid_spans_and_exact_elision_and_rebasing() {
        for sample in SAMPLES {
            for end in sample.char_indices().map(|(i, _)| i).chain([sample.len()]) {
                let input = &sample[..end];
                let original = tokens(input);
                valid_spans(input, &original);
                let shown = display(input);
                valid_spans(&shown.text, &shown.spans);
                let mut removals = Vec::new();
                for (range, token) in &original {
                    if *token == ShellToken::Routing {
                        let end = range.end
                            + input[range.end..]
                                .chars()
                                .take_while(|c| c.is_whitespace())
                                .map(char::len_utf8)
                                .sum::<usize>();
                        removals.push(range.start..end);
                    }
                }
                let mut expected = input.to_owned();
                for range in removals.iter().rev() {
                    expected.replace_range(range.clone(), "");
                }
                assert_eq!(shown.text, expected, "{input:?}");
                let shifted: Vec<_> = original
                    .iter()
                    .filter(|(_, t)| *t != ShellToken::Routing)
                    .map(|(range, token)| {
                        let removed: usize = removals
                            .iter()
                            .filter(|r| r.end <= range.start)
                            .map(|r| r.len())
                            .sum();
                        (range.start - removed..range.end - removed, *token)
                    })
                    .collect();
                assert_eq!(shown.spans, shifted, "{input:?}");
            }
        }
    }
}
