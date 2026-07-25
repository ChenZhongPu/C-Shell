//! The line editor: C syntax highlighting as you type, deciding when a
//! multi-line input is finished, and the IPython-style completion menu.
//!
//! The editor integration is reedline: the completion dropdown is its
//! `IdeMenu`, multi-line continuation is driven by the [`Validator`], and the
//! prompt aligns wrapped lines under the first line's code. The C-specific
//! completeness and completion logic below is plain, editor-agnostic code.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyEvent};
use nu_ansi_term::{Color as NuColor, Style as NuStyle};
use reedline::{
    Completer, EditCommand, EditMode, Editor, Emacs, Highlighter, KeyCode, KeyModifiers, Menu,
    MenuEvent, Painter, Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus,
    ReedlineEvent, ReedlineRawEvent, Span, StyledText, Suggestion, ValidationResult, Validator,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use crate::lex;
use crate::ui::Ui;

/// The magic commands, for completion.
const MAGICS: &[&str] = &[
    "%help", "%quit", "%exit", "%clear", "%reset", "%src", "%header", "%edit", "%type", "%time",
    "%timeit", "%undo", "%cc", "%std",
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
    "memmove",
    "memset",
    "memcmp",
    "memchr",
    "strlen",
    "strcmp",
    "strncmp",
    "strcpy",
    "strncpy",
    "strcat",
    "strncat",
    "strchr",
    "strrchr",
    "strstr",
    "strtok",
    "atoi",
    "atof",
    "strtol",
    "strtod",
    "abs",
    "qsort",
    "exit",
    "fopen",
    "fclose",
    "fread",
    "fwrite",
    "fprintf",
];

/// Syntax highlighting for the edit buffer. Holds syntect's loaded sets so
/// they are not rebuilt on every keystroke.
pub struct CHighlighter {
    syntaxes: SyntaxSet,
    syntax: SyntaxReference,
    theme: Theme,
    color: bool,
    /// Shared edit-buffer snapshot, updated on every repaint so that
    /// [`CEditMode`] can read both the text and cursor position.
    state: Arc<Mutex<EditState>>,
}

impl CHighlighter {
    pub fn new(color: bool, state: Arc<Mutex<EditState>>) -> Self {
        // Loaded once: syntect's default sets are expensive enough that
        // rebuilding them per keystroke would be visible as input lag.
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let syntax = syntaxes
            .find_syntax_by_extension("c")
            .unwrap_or_else(|| syntaxes.find_syntax_plain_text())
            .clone();
        let theme = ThemeSet::load_defaults().themes["base16-ocean.dark"].clone();
        CHighlighter {
            syntaxes,
            syntax,
            theme,
            color,
            state,
        }
    }
}

/// The editor information exposed to [`CEditMode`] by the highlighter.
///
/// Reedline's `EditMode` receives only key events, not the current buffer or
/// cursor. The highlighter receives both immediately before each input event,
/// so it keeps this small shared snapshot for the custom indentation handler.
#[derive(Default)]
pub struct EditState {
    buffer: String,
    cursor: usize,
}

/// Tab completion against magics, C vocabulary and session identifiers. The
/// vocabulary is shared with the REPL loop, which refreshes it after each input.
pub struct CCompleter {
    idents: Arc<Mutex<Vec<String>>>,
}

impl CCompleter {
    pub fn new(idents: Arc<Mutex<Vec<String>>>) -> Self {
        CCompleter { idents }
    }
}

/// Decides, through [`is_incomplete`], whether Enter submits the buffer or
/// opens a continuation line.
pub struct CValidator;

/// The `In [n]:` prompt. reedline colors it, and for continuation lines
/// substitutes the multiline indicator — spaces as wide as the prompt so that
/// wrapped code aligns under the first line's code.
pub struct CPrompt {
    pub n: usize,
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
    // Only names in code position are worth completing. Characters inside a
    // string or char literal, or a comment, are text — `str` typed in "..."
    // is not the start of `strcat`. `lex::scan` marks each byte code-or-not.
    if !lex::scan(line).code.get(start).copied().unwrap_or(true) {
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

impl Highlighter for CHighlighter {
    fn highlight(&self, line: &str, cursor: usize) -> StyledText {
        // Snapshot the buffer for CEditMode; highlight is called after every
        // keystroke, so the edit mode always sees up-to-date content.
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.buffer.clear();
        state.buffer.push_str(line);
        state.cursor = cursor;

        let mut styled = StyledText::new();
        // Without color, or for a magic line (which is not C), emit one flat
        // segment carrying the whole buffer, newlines included, so reedline
        // can still weave the multiline indicator between continuation lines.
        if !self.color {
            styled.push((NuStyle::new(), line.to_string()));
            return styled;
        }
        if line.trim_start().starts_with('%') {
            styled.push((NuStyle::new().fg(NuColor::Cyan), line.to_string()));
            return styled;
        }
        // syntect highlights one physical line at a time; rejoin the lines
        // with explicit newline segments so the painter still sees them.
        for (i, physical) in line.split('\n').enumerate() {
            if i > 0 {
                styled.push((NuStyle::new(), "\n".to_string()));
            }
            let mut h = HighlightLines::new(&self.syntax, &self.theme);
            match h.highlight_line(physical, &self.syntaxes) {
                Ok(ranges) => {
                    for (syn, text) in ranges {
                        let c = syn.foreground;
                        let r = c.r;
                        let g = c.g;
                        let b = c.b;
                        let is_bracket_or_punct = text
                            .chars()
                            .any(|ch| matches!(ch, '{' | '}' | '(' | ')' | '[' | ']' | ';' | ','));
                        let brightness =
                            (299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b)) / 1000;

                        let style = if is_bracket_or_punct || brightness < 150 {
                            NuStyle::new().fg(NuColor::Rgb(220, 224, 230))
                        } else {
                            NuStyle::new().fg(NuColor::Rgb(r, g, b))
                        };
                        styled.push((style, text.to_string()));
                    }
                }
                Err(_) => styled.push((NuStyle::new(), physical.to_string())),
            }
        }
        styled
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
    let sc = lex::scan(input);
    let b = input.as_bytes();

    // Find the last control keyword in code position
    let mut last_ctrl_end = None;
    let mut i = 0;
    while i < b.len() {
        if !sc.code[i] {
            i += 1;
            continue;
        }
        if b[i] == b'_' || b[i].is_ascii_alphabetic() {
            let start = i;
            while i < b.len() && sc.code[i] && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            let word = &input[start..i];
            if matches!(word, "if" | "for" | "while" | "switch") {
                last_ctrl_end = Some(i);
            }
            continue;
        }
        i += 1;
    }

    let Some(ctrl_end) = last_ctrl_end else {
        return false;
    };

    // Find the first '(' at or after the last control keyword
    let Some(open) = (ctrl_end..b.len()).find(|&idx| sc.code[idx] && b[idx] == b'(') else {
        return true;
    };

    let mut depth = 0i32;
    let mut close = None;
    for (idx, &byte) in b.iter().enumerate().skip(open) {
        if !sc.code[idx] {
            continue;
        }
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(idx);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else { return true };
    !(close + 1..b.len()).any(|idx| sc.code[idx] && !b[idx].is_ascii_whitespace())
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

impl Validator for CValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        if is_incomplete(line) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Complete
        }
    }
}

/// Checks whether a single line is an unbraced control header (e.g. `if (...)`,
/// `else if (...)`, `else`, `for (...)`, `while (...)`, `do`) or function signature
/// that awaits a body without using `{`.
fn is_dangling_header(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.starts_with('%') || t.starts_with('#') {
        return false;
    }

    let sc = lex::scan(line);
    let bytes = line.as_bytes();
    let last_code_idx = (0..bytes.len())
        .rev()
        .find(|&i| sc.code[i] && !bytes[i].is_ascii_whitespace());

    let Some(last_idx) = last_code_idx else {
        return false;
    };

    let last_char = bytes[last_idx];
    if last_char == b';' || last_char == b'{' || last_char == b'}' {
        return false;
    }

    if lex::awaits_body(line) {
        return true;
    }

    if control_header_awaits_body(line) {
        return true;
    }

    let words = lex::identifiers(line);
    if let Some(last_word) = words.last()
        && (last_word == "else" || last_word == "do")
    {
        return true;
    }

    false
}

/// Replaces comment/literal bytes with spaces using a code mask obtained from
/// scanning the complete buffer. This lets the line-oriented header heuristic
/// retain lexical state across physical lines.
fn code_only_line(line: &str, code: &[bool]) -> String {
    let mut bytes = line.as_bytes().to_vec();
    for (byte, &is_code) in bytes.iter_mut().zip(code) {
        if !is_code && !byte.is_ascii_whitespace() {
            *byte = b' ';
        }
    }
    String::from_utf8(bytes).expect("masking bytes with ASCII spaces preserves UTF-8")
}

/// Calculates the auto-indent level (in 2-space units) for a continuation line.
/// Combines `{}` brace nesting with unbraced control structures (like `if`,
/// `else`, `for`, `while`, `do`, or function headers) that introduce an extra level of indentation.
pub fn compute_indent_level(input: &str) -> usize {
    let scan = lex::scan(input);
    let mut brace_depth = 0i32;
    let mut dangling_depth = 0usize;
    // `(brace depth, dangling floor)`: an outer unbraced statement remains
    // active while the inner braced statement serving as its body is open.
    let mut protected_dangling = Vec::<(i32, usize)>::new();
    let starts_with_do = first_code_word(input).as_deref() == Some("do");
    let mut offset = 0usize;

    for segment in input.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let line_code = &scan.code[offset..offset + line.len()];
        let code_line = code_only_line(line, line_code);
        let bytes = line.as_bytes();

        let mut last_code_char = None;
        let mut code_nonspace_count = 0usize;
        let mut has_brace = false;

        for (i, &b) in bytes.iter().enumerate() {
            if line_code[i] {
                if !b.is_ascii_whitespace() {
                    last_code_char = Some(b);
                    code_nonspace_count += 1;
                }
                if b == b'{' || b == b'}' {
                    has_brace = true;
                }
            }
        }

        let bare_open_brace = code_nonspace_count == 1 && last_code_char == Some(b'{');
        if has_brace {
            for (i, &b) in bytes.iter().enumerate() {
                if !line_code[i] {
                    continue;
                }
                match b {
                    b'{' => {
                        let floor = protected_dangling.last().map_or(0, |&(_, floor)| floor);
                        if bare_open_brace && dangling_depth > floor {
                            // In `if (x)\n{`, the brace replaces the pending
                            // unbraced indentation level.
                            dangling_depth -= 1;
                        }
                        brace_depth += 1;
                        if !bare_open_brace && dangling_depth > floor {
                            // In `if (x)\n  if (y) {`, retain the outer level
                            // until the inner compound statement closes.
                            protected_dangling.push((brace_depth, dangling_depth));
                        }
                    }
                    b'}' => {
                        brace_depth = (brace_depth - 1).max(0);
                        protected_dangling.retain(|&(depth, _)| depth <= brace_depth);
                        dangling_depth = protected_dangling.last().map_or(0, |&(_, floor)| floor);
                    }
                    _ => {}
                }
            }
        }

        let floor = protected_dangling.last().map_or(0, |&(_, floor)| floor);
        if last_code_char == Some(b';') {
            dangling_depth = floor;
        } else if (!has_brace || first_code_word(&code_line).as_deref() == Some("else"))
            && is_dangling_header(&code_line)
        {
            // A top-level `while (...)` after a completed `do` body is waiting
            // for its mandatory semicolon, not for another statement body.
            let is_do_while_tail = starts_with_do
                && brace_depth == 0
                && dangling_depth == 0
                && first_code_word(&code_line).as_deref() == Some("while");
            if !is_do_while_tail {
                dangling_depth += 1;
            }
        }

        offset += segment.len();
    }

    // `split_inclusive` yields no segment for an empty buffer.
    if input.is_empty() {
        return 0;
    }

    (brace_depth.max(0) as usize) + dangling_depth
}

/// Menu proxy that exposes whether completion is active to [`CEditMode`].
///
/// An active menu must receive Enter itself so Reedline can accept the selected
/// completion. Without this proxy the custom newline event would bypass that
/// higher-priority menu behavior.
pub struct TrackingMenu<M> {
    inner: M,
    active: Arc<AtomicBool>,
}

impl<M: Menu> TrackingMenu<M> {
    pub fn new(inner: M, active: Arc<AtomicBool>) -> Self {
        active.store(inner.is_active(), Ordering::Relaxed);
        Self { inner, active }
    }
}

impl<M: Menu> Menu for TrackingMenu<M> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn indicator(&self) -> &str {
        self.inner.indicator()
    }

    fn is_active(&self) -> bool {
        self.inner.is_active()
    }

    fn menu_event(&mut self, event: MenuEvent) {
        self.inner.menu_event(event);
        self.active.store(self.inner.is_active(), Ordering::Relaxed);
    }

    fn can_quick_complete(&self) -> bool {
        self.inner.can_quick_complete()
    }

    fn can_partially_complete(
        &mut self,
        values_updated: bool,
        editor: &mut Editor,
        completer: &mut dyn Completer,
    ) -> bool {
        self.inner
            .can_partially_complete(values_updated, editor, completer)
    }

    fn update_values(&mut self, editor: &mut Editor, completer: &mut dyn Completer) {
        self.inner.update_values(editor, completer);
    }

    fn update_working_details(
        &mut self,
        editor: &mut Editor,
        completer: &mut dyn Completer,
        painter: &Painter,
    ) {
        self.inner
            .update_working_details(editor, completer, painter);
    }

    fn replace_in_buffer(&self, editor: &mut Editor) {
        self.inner.replace_in_buffer(editor);
    }

    fn menu_required_lines(&self, terminal_columns: u16) -> u16 {
        self.inner.menu_required_lines(terminal_columns)
    }

    fn menu_string(&self, available_lines: u16, use_ansi_coloring: bool) -> String {
        self.inner.menu_string(available_lines, use_ansi_coloring)
    }

    fn min_rows(&self) -> u16 {
        self.inner.min_rows()
    }

    fn get_values(&self) -> &[Suggestion] {
        self.inner.get_values()
    }

    fn set_cursor_pos(&mut self, pos: (u16, u16)) {
        self.inner.set_cursor_pos(pos);
        self.active.store(self.inner.is_active(), Ordering::Relaxed);
    }
}

/// Wraps [`Emacs`] to auto-indent continuation lines: when Enter is pressed
/// and the buffer is still incomplete, a newline plus `2 × compute_indent_level`
/// spaces is inserted instead of a bare newline.
pub struct CEditMode {
    inner: Emacs,
    /// Shared with [`CHighlighter`], which snapshots the buffer and cursor on
    /// every repaint.
    state: Arc<Mutex<EditState>>,
    /// Shared with [`TrackingMenu`] so Enter can retain Reedline's normal
    /// completion-accept behavior while a menu is active.
    menu_active: Arc<AtomicBool>,
}

impl CEditMode {
    pub fn new(inner: Emacs, state: Arc<Mutex<EditState>>, menu_active: Arc<AtomicBool>) -> Self {
        CEditMode {
            inner,
            state,
            menu_active,
        }
    }
}

impl EditMode for CEditMode {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let raw: Event = event.into();
        if let Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            ..
        }) = &raw
        {
            if *code == KeyCode::Enter {
                // Reedline gives active completion menus first refusal on
                // Enter; preserve that behavior instead of inserting a line.
                if self.menu_active.load(Ordering::Relaxed) {
                    return ReedlineEvent::Enter;
                }

                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if is_incomplete(&state.buffer) {
                    let cursor = state.cursor.min(state.buffer.len());
                    let depth = compute_indent_level(&state.buffer[..cursor]);
                    let indent = "  ".repeat(depth);
                    return ReedlineEvent::Edit(vec![EditCommand::InsertString(format!(
                        "\n{indent}"
                    ))]);
                }
            } else if *code == KeyCode::Char('}') {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let cursor = state.cursor.min(state.buffer.len());
                let before_cursor = &state.buffer[..cursor];
                let line_start = before_cursor.rfind('\n').map_or(0, |i| i + 1);
                let current_prefix = &before_cursor[line_start..];
                if !current_prefix.is_empty() && current_prefix.chars().all(char::is_whitespace) {
                    let target_depth =
                        compute_indent_level(&state.buffer[..line_start]).saturating_sub(1);
                    let indent = "  ".repeat(target_depth);

                    let mut edits = vec![EditCommand::Backspace; current_prefix.chars().count()];
                    edits.push(EditCommand::InsertString(format!("{indent}}}")));
                    return ReedlineEvent::Edit(edits);
                }
            }
        }
        // Re-wrap and delegate to the inner Emacs mode for everything else.
        // The TryFrom can only fail for KeyRelease, which reedline already
        // filters out before calling parse_event.
        let re_wrapped = ReedlineRawEvent::try_from(raw).expect("non-release event");
        self.inner.parse_event(re_wrapped)
    }

    fn edit_mode(&self) -> PromptEditMode {
        self.inner.edit_mode()
    }
}

impl Completer for CCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let idents = self.idents.lock().expect("ident vocabulary");
        let (start, candidates) = completion_candidates(line, pos, &idents);
        candidates
            .into_iter()
            .map(|value| Suggestion {
                value,
                span: Span::new(start, pos),
                ..Suggestion::default()
            })
            .collect()
    }
}

impl Prompt for CPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Owned(Ui::prompt_label(self.n))
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed(" ")
    }

    /// Continuation lines carry no `In [n]:`; padding them to the prompt width
    /// keeps wrapped code aligned under the first line's code.
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Owned(" ".repeat(Ui::prompt_width(self.n)))
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let prefix = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!(
            "({prefix}reverse-search: {}) ",
            history_search.term
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::IdeMenu;

    #[test]
    fn completes_magic_commands_at_line_start_only() {
        // `%h` is an ambiguous prefix and must surface every match, not one.
        let (start, c) = completion_candidates("%h", 2, &[]);
        assert_eq!(start, 0);
        assert!(c.contains(&"%help".to_string()) && c.contains(&"%header".to_string()));
        assert_eq!(
            completion_candidates("%he", 3, &[]).1,
            vec!["%help", "%header"]
        );
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
    fn an_ambiguous_prefix_offers_every_match_not_just_the_first() {
        // The gripe this addresses: `str` used to jump straight to one word.
        // Every family member must be present so List completion can show them.
        let (start, c) = completion_candidates("str", 3, &[]);
        assert_eq!(start, 0);
        for expected in ["strcat", "strcmp", "strcpy", "strlen", "strstr"] {
            assert!(
                c.contains(&expected.to_string()),
                "{expected} missing: {c:?}"
            );
        }
        assert!(
            c.windows(2).all(|w| w[0] < w[1]),
            "candidates not sorted: {c:?}"
        );
    }

    #[test]
    fn no_candidates_inside_numbers_or_empty() {
        assert!(completion_candidates("x = 0x1", 7, &[]).1.is_empty());
        assert!(completion_candidates("", 0, &[]).1.is_empty());
    }

    #[test]
    fn completes_only_names_in_code_position() {
        // Inside a string or char literal there is no name to complete.
        assert!(
            completion_candidates("printf(\"strc", 12, &[]).1.is_empty(),
            "completed inside a string literal"
        );
        assert!(
            completion_candidates("char c = 'a", 11, &[]).1.is_empty(),
            "completed inside a char literal"
        );
        // Right after `strcat(` the word is empty: nothing to complete.
        assert!(completion_candidates("strcat(", 7, &[]).1.is_empty());
        // An identifier in argument position is still a name worth completing.
        assert!(
            completion_candidates("strcat(s, str", 13, &[])
                .1
                .contains(&"strcat".to_string()),
            "suppressed a real identifier in argument position"
        );
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
    fn completer_maps_candidates_to_reedline_spans() {
        let mut completer = CCompleter::new(Arc::new(Mutex::new(vec!["siz_total".to_string()])));
        let suggestions = completer.complete("x = siz", 7);
        assert_eq!(
            suggestions.iter().map(|s| &s.value).collect::<Vec<_>>(),
            ["siz_total", "size_t", "sizeof"]
        );
        assert!(
            suggestions.iter().all(|s| s.span == Span::new(4, 7)),
            "completion spans must cover the typed prefix: {suggestions:?}"
        );
    }

    #[test]
    fn compute_indent_level_handles_braces_and_unbraced_control_headers() {
        assert_eq!(compute_indent_level("int add(int a, int b) {"), 1);
        assert_eq!(
            compute_indent_level("int add(int a, int b) {\n  return a + b;"),
            1
        );
        assert_eq!(
            compute_indent_level("int add(int a, int b) {\n  return a + b;\n}"),
            0
        );

        // Unbraced control statements & function signatures
        assert_eq!(compute_indent_level("int add(int a, int b)"), 1);
        assert_eq!(compute_indent_level("if (x > 0)"), 1);
        assert_eq!(compute_indent_level("if (x > 0)\n  return 1;"), 0);
        assert_eq!(compute_indent_level("if (x > 0)\n  if (y > 0)"), 2);
        assert_eq!(compute_indent_level("else"), 1);
        assert_eq!(compute_indent_level("else if (x > 0)"), 1);
        assert_eq!(compute_indent_level("for (int i = 0; i < 10; ++i)"), 1);
        assert_eq!(compute_indent_level("while (condition)"), 1);
        assert_eq!(compute_indent_level("do"), 1);
    }

    #[test]
    fn compute_indent_level_preserves_cross_line_lexical_and_control_state() {
        assert_eq!(
            compute_indent_level("int f(void) {\n  /*\n   * }\n   */"),
            1,
            "a brace inside a multiline comment is not code"
        );
        assert_eq!(
            compute_indent_level("if (x)\n  if (y) {"),
            2,
            "an inner braced statement must retain its unbraced parent"
        );
        assert_eq!(
            compute_indent_level("if (x)\n  if (y) {\n    f();"),
            2,
            "a statement inside the block must not consume its unbraced parent"
        );
        assert_eq!(
            compute_indent_level("if (x)\n  if (y) {\n    f();\n  }"),
            0,
            "closing the inner statement also completes the unbraced parent"
        );
        assert_eq!(
            compute_indent_level("if (x)\n{"),
            1,
            "a brace on its own line replaces the pending control indent"
        );
        assert_eq!(
            compute_indent_level("do\n  f();\nwhile (x)"),
            0,
            "the trailing while awaits a semicolon, not another body"
        );
    }

    #[test]
    fn c_edit_mode_auto_dedents_closing_brace_on_blank_line() {
        let text = "if (4 > 3) {\n  puts(\"4 > 3\");\n  ";
        let state = Arc::new(Mutex::new(EditState {
            buffer: text.to_string(),
            cursor: text.len(),
        }));
        let menu_active = Arc::new(AtomicBool::new(false));
        let mut mode = CEditMode::new(Emacs::default(), state, menu_active);

        let event = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Char('}'),
            KeyModifiers::NONE,
        )))
        .unwrap();

        match mode.parse_event(event) {
            ReedlineEvent::Edit(edits) => {
                // Should backspace the 2 leading spaces of the current line, then insert "}"
                assert_eq!(edits.len(), 3);
                assert_eq!(edits[0], EditCommand::Backspace);
                assert_eq!(edits[1], EditCommand::Backspace);
                assert_eq!(edits[2], EditCommand::InsertString("}".to_string()));
            }
            other => panic!("expected ReedlineEvent::Edit, got {other:?}"),
        }
    }

    #[test]
    fn c_edit_mode_uses_the_line_at_the_cursor() {
        let text = "if (x) {\n  \n  puts(\"x\");\n}";
        let cursor = text.find("\n  \n").expect("blank line") + 3;
        let state = Arc::new(Mutex::new(EditState {
            buffer: text.to_string(),
            cursor,
        }));
        let menu_active = Arc::new(AtomicBool::new(false));
        let mut mode = CEditMode::new(Emacs::default(), state, menu_active);
        let event = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Char('}'),
            KeyModifiers::NONE,
        )))
        .unwrap();

        match mode.parse_event(event) {
            ReedlineEvent::Edit(edits) => {
                assert_eq!(
                    edits,
                    [
                        EditCommand::Backspace,
                        EditCommand::Backspace,
                        EditCommand::InsertString("}".to_string())
                    ]
                );
            }
            other => panic!("expected ReedlineEvent::Edit, got {other:?}"),
        }
    }

    #[test]
    fn c_edit_mode_never_dedents_from_trailing_whitespace_after_the_cursor() {
        let text = "if (x) {\n  puts(\"x\");\n  ";
        let state = Arc::new(Mutex::new(EditState {
            buffer: text.to_string(),
            cursor: 2,
        }));
        let menu_active = Arc::new(AtomicBool::new(false));
        let mut mode = CEditMode::new(Emacs::default(), state, menu_active);
        let event = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Char('}'),
            KeyModifiers::NONE,
        )))
        .unwrap();

        assert_eq!(
            mode.parse_event(event),
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('}')])
        );
    }

    #[test]
    fn c_edit_mode_computes_enter_indent_from_the_cursor_prefix() {
        let text = "if (x) {\n  puts(\"x\");\n}";
        let cursor = text.find('\n').expect("multiline buffer") + 1;
        let state = Arc::new(Mutex::new(EditState {
            buffer: text.to_string(),
            cursor,
        }));
        let menu_active = Arc::new(AtomicBool::new(false));
        let mut mode = CEditMode::new(Emacs::default(), state, menu_active);
        let event = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();

        assert_eq!(
            mode.parse_event(event),
            ReedlineEvent::Edit(vec![EditCommand::InsertString("\n  ".to_string())])
        );
    }

    #[test]
    fn c_edit_mode_leaves_enter_to_an_active_completion_menu() {
        let text = "if (x) {\n  ret";
        let state = Arc::new(Mutex::new(EditState {
            buffer: text.to_string(),
            cursor: text.len(),
        }));
        let menu_active = Arc::new(AtomicBool::new(true));
        let mut mode = CEditMode::new(Emacs::default(), state, menu_active);
        let event = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();

        assert_eq!(mode.parse_event(event), ReedlineEvent::Enter);
    }

    #[test]
    fn tracking_menu_reports_activation_and_deactivation() {
        let active = Arc::new(AtomicBool::new(false));
        let mut menu = TrackingMenu::new(IdeMenu::default(), Arc::clone(&active));

        menu.menu_event(MenuEvent::Activate(false));
        assert!(active.load(Ordering::Relaxed));
        menu.menu_event(MenuEvent::Deactivate);
        assert!(!active.load(Ordering::Relaxed));
    }

    #[test]
    fn control_header_awaits_body_handles_multi_statement_and_embedded_keywords() {
        // Multi-statement line where the last control header is incomplete
        assert!(control_header_awaits_body("do_something(); if (x)"));
        // Complete control statement on the same line
        assert!(!control_header_awaits_body("if (x) puts(\"hi\");"));
        // String literal containing keyword 'if' shouldn't confuse control header detection
        assert!(!control_header_awaits_body("printf(\"if (x)\");"));
    }
}
