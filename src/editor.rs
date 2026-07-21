//! The line editor: C syntax highlighting as you type, and deciding when a
//! multi-line input is finished.

use std::borrow::Cow;

use rustyline::Helper;
use rustyline::completion::Completer;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
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

impl Validator for CHelper {
    fn validate(&self, ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        let input = ctx.input();
        let t = input.trim();
        if t.is_empty() || t.starts_with('%') {
            return Ok(ValidationResult::Valid(None));
        }
        let sc = lex::scan(input);
        if sc.depth > 0
            || sc.unterminated
            || input.trim_end().ends_with('\\')
            || lex::awaits_body(input)
        {
            return Ok(ValidationResult::Incomplete);
        }
        Ok(ValidationResult::Valid(None))
    }
}

impl Completer for CHelper {
    type Candidate = String;
}

impl Hinter for CHelper {
    type Hint = String;
}

impl Helper for CHelper {}
