//! The line editor: C syntax highlighting as you type, and deciding when a
//! multi-line input is finished.

use std::borrow::Cow;

use rustyline::completion::Completer;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Cmd, ConditionalEventHandler, Event, EventContext, Helper, Movement, RepeatCount};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::as_24_bit_terminal_escaped;

use crate::lex;

/// The magic commands, for completion.
const MAGICS: &[&str] = &[
    "%help", "%quit", "%exit", "%reset", "%history", "%src", "%undo", "%cc", "%std",
];

/// C keywords, common types and stdlib staples worth offering at a C prompt.
const C_WORDS: &[&str] = &[
    "auto",
    "bool",
    "break",
    "case",
    "char",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extern",
    "false",
    "float",
    "for",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "register",
    "restrict",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "struct",
    "switch",
    "typedef",
    "union",
    "unsigned",
    "void",
    "volatile",
    "while",
    "_Bool",
    "_Generic",
    "true",
    "size_t",
    "int8_t",
    "int16_t",
    "int32_t",
    "int64_t",
    "uint8_t",
    "uint16_t",
    "uint32_t",
    "uint64_t",
    "intptr_t",
    "uintptr_t",
    "ptrdiff_t",
    "NULL",
    "printf",
    "scanf",
    "puts",
    "putchar",
    "getchar",
    "malloc",
    "calloc",
    "realloc",
    "free",
    "memcpy",
    "memset",
    "strlen",
    "strcmp",
    "strcpy",
    "strncpy",
    "fopen",
    "fclose",
    "fread",
    "fwrite",
    "fprintf",
];

pub struct CHelper {
    syntaxes: SyntaxSet,
    syntax: SyntaxReference,
    theme: Theme,
    pub color: bool,
    /// Session vocabulary, refreshed by the REPL loop after each input.
    idents: Arc<Mutex<Vec<String>>>,
}

impl CHelper {
    pub fn new(color: bool, idents: Arc<Mutex<Vec<String>>>) -> Self {
        // Loaded once: syntect's default sets are expensive enough that
        // rebuilding them per keystroke would be visible as input lag.
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let syntax = syntaxes
            .find_syntax_by_extension("c")
            .unwrap_or_else(|| syntaxes.find_syntax_plain_text())
            .clone();
        let theme = ThemeSet::load_defaults().themes["base16-ocean.dark"].clone();
        CHelper {
            syntaxes,
            syntax,
            theme,
            color,
            idents,
        }
    }
}

/// What Tab should offer at `pos`: `(replace_from, candidates)`.
///
/// A word starting the line with `%` completes against the magic commands;
/// an identifier completes against C keywords, stdlib staples and every name
/// the session has mentioned.
fn completion_candidates(line: &str, pos: usize, idents: &[String]) -> (usize, Vec<String>) {
    let b = line.as_bytes();
    let mut start = pos;
    while start > 0 && (b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'_') {
        start -= 1;
    }
    // `%word` is a magic only at the start of the line; elsewhere `%` is the
    // modulo operator and no part of the word.
    if start > 0 && b[start - 1] == b'%' && line[..start - 1].trim_start().is_empty() {
        let word = &line[start - 1..pos];
        let m = MAGICS
            .iter()
            .filter(|c| c.starts_with(word))
            .map(|s| s.to_string())
            .collect();
        return (start - 1, m);
    }
    let word = &line[start..pos];
    if word.is_empty() || word.as_bytes()[0].is_ascii_digit() {
        return (pos, Vec::new());
    }
    let mut out: Vec<String> = C_WORDS
        .iter()
        .filter(|k| k.starts_with(word))
        .map(|s| s.to_string())
        .chain(idents.iter().filter(|k| k.starts_with(word)).cloned())
        .collect();
    out.sort();
    out.dedup();
    (start, out)
}

impl Highlighter for CHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if !self.color || line.is_empty() {
            return Cow::Borrowed(line);
        }
        // Magic commands are not C; highlighting them as C looks wrong.
        if line.trim_start().starts_with('%') {
            return Cow::Owned(format!("\x1b[36m{line}\x1b[0m"));
        }
        let mut h = HighlightLines::new(&self.syntax, &self.theme);
        match h.highlight_line(line, &self.syntaxes) {
            Ok(ranges) => {
                let mut s = as_24_bit_terminal_escaped(&ranges[..], false);
                s.push_str("\x1b[0m");
                Cow::Owned(s)
            }
            Err(_) => Cow::Borrowed(line),
        }
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _forced: bool) -> bool {
        self.color
    }
}

/// Is this input still waiting for more lines?
///
/// Single source of truth, shared by the validator and the Enter handler —
/// if the two ever disagreed, Enter could refuse to submit an input the
/// validator considers finished, or vice versa.
pub fn is_incomplete(input: &str) -> bool {
    let t = input.trim();
    if t.is_empty() || t.starts_with('%') {
        return false;
    }
    let sc = lex::scan(input);
    sc.depth > 0 || sc.unterminated || input.trim_end().ends_with('\\') || lex::awaits_body(input)
}

impl Validator for CHelper {
    fn validate(&self, ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        if is_incomplete(ctx.input()) {
            Ok(ValidationResult::Incomplete)
        } else {
            Ok(ValidationResult::Valid(None))
        }
    }
}

/// What Enter should insert at `pos`, or `None` to let the default
/// accept-or-insert behavior run (which submits complete input).
///
/// Continuation lines have no prompt in front of them, so without help they
/// start at column 0 — visually *left* of the first line's code, which sits
/// after `In [n]: `. `base` is that prompt's width: every continuation line
/// is padded to it so code aligns under code, then indented two spaces per
/// unclosed bracket. Inside an unterminated string or comment a bare
/// newline is inserted instead: padding there would become literal content.
fn continuation_insert(line: &str, pos: usize, base: usize) -> Option<String> {
    if !is_incomplete(line) {
        return None;
    }
    let before = lex::scan(&line[..pos]);
    if before.unterminated {
        return Some("\n".to_string());
    }
    let depth = before.depth.max(0) as usize;
    Some(format!("\n{}", " ".repeat(base + 2 * depth)))
}

/// True when a `}` typed at `pos` should first remove one indent level:
/// the cursor sits at the end of a line holding nothing but spaces, so the
/// brace belongs one two-space level out from where auto-indent left it.
fn brace_dedents(line: &str, pos: usize) -> bool {
    let start = line[..pos].rfind('\n').map_or(0, |i| i + 1);
    let cur = &line[start..pos];
    cur.len() >= 2 && cur.bytes().all(|b| b == b' ')
}

/// Enter: submit when the input is complete, otherwise insert a newline
/// followed by the right amount of indentation.
///
/// Holds the current prompt's width, updated by the REPL loop each
/// iteration — `In [10]: ` is one column wider than `In [9]: `.
pub struct EnterIndents(pub Arc<AtomicUsize>);

impl ConditionalEventHandler for EnterIndents {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, ctx: &EventContext) -> Option<Cmd> {
        let base = self.0.load(Ordering::Relaxed);
        continuation_insert(ctx.line(), ctx.pos(), base).map(|s| Cmd::Insert(1, s))
    }
}

/// `}`: on a whitespace-only line, step back one indent level first, the
/// way any code editor closes a block.
pub struct BraceDedents;

impl ConditionalEventHandler for BraceDedents {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, ctx: &EventContext) -> Option<Cmd> {
        if brace_dedents(ctx.line(), ctx.pos()) {
            Some(Cmd::Replace(
                Movement::BackwardChar(2),
                Some("}".to_string()),
            ))
        } else {
            None
        }
    }
}

impl Completer for CHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let idents = self.idents.lock().expect("ident vocabulary");
        Ok(completion_candidates(line, pos, &idents))
    }
}

impl Hinter for CHelper {
    type Hint = String;
}

impl Helper for CHelper {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_magic_commands_at_line_start_only() {
        let (start, c) = completion_candidates("%h", 2, &[]);
        assert_eq!(start, 0);
        assert_eq!(c, vec!["%help", "%history"]);
        // Mid-line `%` is the modulo operator.
        let (_, c) = completion_candidates("a %h", 4, &[]);
        assert!(c.is_empty());
    }

    #[test]
    fn completes_keywords_and_session_names_merged() {
        let idents = vec!["siz_total".to_string()];
        let (start, c) = completion_candidates("x = siz", 7, &idents);
        assert_eq!(start, 4);
        assert_eq!(c, vec!["siz_total", "size_t", "sizeof"]);
    }

    #[test]
    fn no_candidates_inside_numbers_or_empty() {
        assert!(completion_candidates("x = 0x1", 7, &[]).1.is_empty());
        assert!(completion_candidates("", 0, &[]).1.is_empty());
    }

    #[test]
    fn complete_input_falls_through_to_submit() {
        assert_eq!(continuation_insert("int x = 1;", 10, 8), None);
        assert_eq!(continuation_insert("x + 1", 5, 8), None);
        assert_eq!(continuation_insert("%help", 5, 8), None);
    }

    #[test]
    fn continuation_aligns_under_the_prompt_then_two_per_depth() {
        // `In [1]: ` is 8 wide; body of one open brace = 8 + 2.
        assert_eq!(
            continuation_insert("struct P {", 10, 8).as_deref(),
            Some("\n          ")
        );
        let l = "int f(void) { if (x) {";
        assert_eq!(
            continuation_insert(l, l.len(), 8).as_deref(),
            Some(format!("\n{}", " ".repeat(8 + 4)).as_str())
        );
    }

    #[test]
    fn signature_awaiting_body_aligns_at_base() {
        // Depth 0: the `{` line goes directly under the signature's start.
        assert_eq!(
            continuation_insert("int fact(int n)", 15, 8).as_deref(),
            Some("\n        ")
        );
    }

    #[test]
    fn no_padding_inside_unterminated_literal() {
        let l = "puts(\"abc";
        assert_eq!(continuation_insert(l, l.len(), 8).as_deref(), Some("\n"));
    }

    #[test]
    fn closing_brace_dedents_only_on_blank_indent() {
        let l = "struct P {\n          ";
        assert!(brace_dedents(l, l.len()));
        assert!(!brace_dedents("int x", 5));
        // Text already on the line: the brace is not closing an indent.
        let l2 = "struct P {\n          int x;";
        assert!(!brace_dedents(l2, l2.len()));
    }
}
