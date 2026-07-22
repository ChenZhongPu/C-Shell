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
    "%help", "%quit", "%exit", "%clear", "%reset", "%src", "%edit", "%type", "%undo", "%cc", "%std",
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

/// Is this input grammatically forced to continue? Unlike `is_incomplete`,
/// this has no interactive blank-line policy and is suitable for scripts.
pub fn is_structurally_incomplete(input: &str) -> bool {
    let t = input.trim();
    if t.is_empty() || t.starts_with('%') {
        return false;
    }
    let sc = lex::scan(input);
    // Conditional-preprocessor groups are complete at their matching #endif;
    // braces in an inactive branch must not drive the C bracket heuristic.
    if t.starts_with('#') {
        return sc.unterminated
            || input.trim_end().ends_with('\\')
            || preprocessor_depth(input) > 0;
    }
    sc.depth > 0
        || sc.unterminated
        || input.trim_end().ends_with('\\')
        || lex::awaits_body(input)
        || control_header_awaits_body(input)
        || do_awaits_while(input)
        || tag_definition_awaits_semicolon(input)
}

/// Is this interactive input still waiting for more lines?
///
/// C cannot tell whether a complete `if` will be followed by `else`. A
/// completed leading `if` therefore needs a blank continuation line to confirm
/// submission. Functions and control blocks submit as soon as their required
/// syntax closes; a braced struct/union/enum definition remains structurally
/// incomplete until its mandatory declaration semicolon.
///
/// Single source of truth, shared by the validator and Enter handler.
pub fn is_incomplete(input: &str) -> bool {
    if is_structurally_incomplete(input) {
        return true;
    }
    needs_blank_confirmation(input) && !ends_in_blank_line(input)
}

/// A complete batch input beginning with `if` is held for one-line lookahead,
/// because an `else` on the next physical line belongs to the same C statement.
pub fn can_accept_else(input: &str) -> bool {
    !input.trim_start().starts_with('#')
        && !is_structurally_incomplete(input)
        && first_code_word(input).as_deref() == Some("if")
}

pub fn starts_with_else(input: &str) -> bool {
    !input.trim_start().starts_with('#') && first_code_word(input).as_deref() == Some("else")
}

fn needs_blank_confirmation(input: &str) -> bool {
    !input.trim_start().starts_with('#') && first_code_word(input).as_deref() == Some("if")
}

fn ends_in_blank_line(input: &str) -> bool {
    input
        .rfind('\n')
        .is_some_and(|i| input[i + 1..].trim().is_empty())
}

fn preprocessor_depth(input: &str) -> i32 {
    let mut depth = 0i32;
    for line in input.lines() {
        let Some(directive) = line.trim_start().strip_prefix('#') else {
            continue;
        };
        let word = directive
            .trim_start()
            .split(|c: char| c.is_whitespace() || c == '(')
            .next()
            .unwrap_or("");
        match word {
            "if" | "ifdef" | "ifndef" => depth += 1,
            "endif" => depth -= 1,
            _ => {}
        }
    }
    depth
}

fn first_code_word(input: &str) -> Option<String> {
    lex::identifiers(input).into_iter().next()
}

/// `if (x)` / `for (...)` / `while (...)` / `switch (...)` need a body even
/// though their parentheses are balanced. Without this check missing-semicolon
/// repair can silently turn `if (x)` into the empty statement `if (x);`.
fn control_header_awaits_body(input: &str) -> bool {
    let Some(first) = first_code_word(input) else {
        return false;
    };
    if !matches!(first.as_str(), "if" | "for" | "while" | "switch") {
        return false;
    }
    let sc = lex::scan(input);
    let b = input.as_bytes();
    let Some(open) = (0..b.len()).find(|&i| sc.code[i] && b[i] == b'(') else {
        return true;
    };
    let mut depth = 0i32;
    let mut close = None;
    for (i, &byte) in b.iter().enumerate().skip(open) {
        if !sc.code[i] {
            continue;
        }
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else { return true };
    !(close + 1..b.len()).any(|i| sc.code[i] && !b[i].is_ascii_whitespace())
}

/// A `do` statement is not complete at its closing body brace; its trailing
/// `while (condition);` is mandatory.
fn do_awaits_while(input: &str) -> bool {
    if first_code_word(input).as_deref() != Some("do") {
        return false;
    }
    let sc = lex::scan(input);
    let b = input.as_bytes();
    let mut depth = 0i32;
    let mut saw_while = false;
    let mut i = 0;
    while i < b.len() {
        if !sc.code[i] {
            i += 1;
            continue;
        }
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ if depth == 0 && (b[i] == b'_' || b[i].is_ascii_alphabetic()) => {
                let start = i;
                while i < b.len() && sc.code[i] && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                if start > 0 && &input[start..i] == "while" {
                    saw_while = true;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    let ends_with_semicolon = (0..b.len())
        .rev()
        .find(|&i| sc.code[i] && !b[i].is_ascii_whitespace())
        .is_some_and(|i| b[i] == b';');
    !saw_while || !ends_with_semicolon
}

/// A braced tag specifier is still a declaration and therefore cannot finish
/// at its `}`. Keep the editor open for `;` (or a declarator followed by `;`).
/// Restrict this to declaration-shaped prefixes so compound literals such as
/// `(struct P){1, 2}` remain complete expressions, and a function returning a
/// previously declared struct still submits at its body brace.
fn tag_definition_awaits_semicolon(input: &str) -> bool {
    const TAGS: &[&str] = &["struct", "union", "enum"];

    let sc = lex::scan(input);
    if sc.depth != 0 || sc.unterminated {
        return false;
    }
    let b = input.as_bytes();
    let Some(first) = next_code_nonspace(b, &sc.code, 0) else {
        return false;
    };
    let Some((first_word, first_end)) = code_identifier(input, &sc.code, first) else {
        return false;
    };
    let Some(last) = (0..b.len())
        .rev()
        .find(|&i| sc.code[i] && !b[i].is_ascii_whitespace())
    else {
        return false;
    };
    if b[last] == b';' {
        return false;
    }

    if TAGS.contains(&first_word) {
        return tag_word_is_followed_by_brace(input, &sc.code, first_end);
    }
    if first_word != "typedef" {
        return false;
    }

    // Qualifiers may occur between `typedef` and the tag keyword. Search code
    // identifiers only; comments and string contents are invisible.
    let mut i = first_end;
    while let Some(next) = next_code_nonspace(b, &sc.code, i) {
        let Some((word, end)) = code_identifier(input, &sc.code, next) else {
            i = next + 1;
            continue;
        };
        if TAGS.contains(&word) && tag_word_is_followed_by_brace(input, &sc.code, end) {
            return true;
        }
        i = end;
    }
    false
}

fn tag_word_is_followed_by_brace(input: &str, code: &[bool], word_end: usize) -> bool {
    let b = input.as_bytes();
    let Some(mut next) = next_code_nonspace(b, code, word_end) else {
        return false;
    };
    // The tag name is optional (`typedef struct { ... } Name;`).
    if let Some((_, end)) = code_identifier(input, code, next) {
        let Some(after_name) = next_code_nonspace(b, code, end) else {
            return false;
        };
        next = after_name;
    }
    b.get(next) == Some(&b'{')
}

fn next_code_nonspace(bytes: &[u8], code: &[bool], mut from: usize) -> Option<usize> {
    while from < bytes.len() && (!code[from] || bytes[from].is_ascii_whitespace()) {
        from += 1;
    }
    (from < bytes.len()).then_some(from)
}

fn code_identifier<'a>(input: &'a str, code: &[bool], start: usize) -> Option<(&'a str, usize)> {
    let b = input.as_bytes();
    if !code.get(start).copied().unwrap_or(false)
        || !(b[start] == b'_' || b[start].is_ascii_alphabetic())
    {
        return None;
    }
    let mut end = start + 1;
    while end < b.len() && code[end] && (b[end] == b'_' || b[end].is_ascii_alphanumeric()) {
        end += 1;
    }
    Some((&input[start..end], end))
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
        assert_eq!(c, vec!["%help"]);
        assert_eq!(completion_candidates("%cl", 3, &[]).1, vec!["%clear"]);
        assert_eq!(completion_candidates("%ed", 3, &[]).1, vec!["%edit"]);
        assert_eq!(completion_candidates("%ty", 3, &[]).1, vec!["%type"]);
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
    fn if_waits_for_else_or_blank_confirmation() {
        let closed = "if (x) {\n  puts(\"yes\");\n}";
        assert!(!is_structurally_incomplete(closed));
        assert!(is_incomplete(closed));
        assert!(!is_incomplete(&format!("{closed}\n        ")));

        let with_else = format!("{closed}\nelse {{\n  puts(\"no\");\n}}");
        assert!(is_incomplete(&with_else));
        assert!(!is_incomplete(&format!("{with_else}\n        ")));
    }

    #[test]
    fn control_header_and_do_while_are_structurally_incomplete() {
        assert!(is_structurally_incomplete("if (x)"));
        assert!(is_structurally_incomplete("for (int i = 0; i < 2; ++i)"));
        assert!(is_structurally_incomplete("do { puts(\"x\"); }"));
        assert!(!is_structurally_incomplete(
            "do { puts(\"x\"); } while (0);"
        ));
    }

    #[test]
    fn conditional_preprocessor_group_waits_for_endif() {
        assert!(is_structurally_incomplete("#if FEATURE"));
        assert!(is_structurally_incomplete("#if FEATURE\n#if INNER\n#endif"));
        // An unmatched C brace in an inactive branch does not keep the editor
        // waiting once the preprocessor group itself is closed.
        assert!(!is_structurally_incomplete("#if 0\n{\n#endif"));
        assert!(!is_structurally_incomplete("#if FEATURE\n#endif"));
    }

    #[test]
    fn function_submits_at_its_closing_brace() {
        let function = "int f(void)\n{\n  return 1;\n}";
        assert!(!is_structurally_incomplete(function));
        assert!(!is_incomplete(function));
        assert!(!is_incomplete("struct P *make(void) {\n  return 0;\n}"));
        assert!(!is_incomplete("while (0) {\n}"));
        assert!(!is_incomplete("do {\n} while (0);"));
    }

    #[test]
    fn braced_tag_definitions_wait_for_their_semicolon() {
        let definition = "struct P { int x; int y; }";
        assert!(is_structurally_incomplete(definition));
        assert!(is_incomplete(definition));
        assert!(!is_incomplete(&format!("{definition};")));
        assert!(is_incomplete("typedef union { int i; double d; } Value"));
        assert!(is_incomplete("enum Color { RED, GREEN }"));

        // These braces do not end a tag declaration.
        assert!(!is_incomplete("(struct P){ 1, 2 }"));
        assert!(!is_incomplete(
            "struct P make(void) { return (struct P){0}; }"
        ));
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
