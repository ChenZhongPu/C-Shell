//! The line editor: C syntax highlighting as you type, and deciding when a
//! multi-line input is finished.

use std::borrow::Cow;

use rustyline::completion::Completer;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Cmd, ConditionalEventHandler, Event, EventContext, Helper, Movement, RepeatCount};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::as_24_bit_terminal_escaped;

use crate::lex;

pub struct CHelper {
    syntaxes: SyntaxSet,
    syntax: SyntaxReference,
    theme: Theme,
    pub color: bool,
}

impl CHelper {
    pub fn new(color: bool) -> Self {
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
        }
    }
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
fn is_incomplete(input: &str) -> bool {
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
}

impl Hinter for CHelper {
    type Hint = String;
}

impl Helper for CHelper {}

#[cfg(test)]
mod tests {
    use super::*;

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
