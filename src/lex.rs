//! A byte-level scan of C source: just enough state to know whether a given
//! byte is real code or sits inside a literal or comment.
//!
//! Deliberately not a parser. Two consumers need exactly this much and no
//! more: the multiline validator (is the input finished?) and the `_` ->
//! `_N` rewrite (don't touch underscores inside strings).

/// Result of scanning a source fragment.
pub struct Scan {
    /// Net nesting of `()`, `[]` and `{}` counted together. Negative means
    /// the input closed more than it opened.
    pub depth: i32,
    /// Still inside a string, char literal or block comment at end of input.
    pub unterminated: bool,
    /// One flag per byte: true when that byte is code rather than literal or
    /// comment content.
    pub code: Vec<bool>,
}

pub fn scan(s: &str) -> Scan {
    let b = s.as_bytes();
    let mut code = vec![false; b.len()];
    let mut depth = 0i32;
    let mut i = 0;
    // State is tracked with plain flags rather than an enum; the transitions
    // are few enough that a match would be more ceremony than it saves.
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut in_str = false;
    let mut in_chr = false;

    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();

        if in_line_comment {
            if c == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if c == b'*' && next == Some(b'/') {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_str || in_chr {
            // A backslash escapes the next byte, including the closing quote.
            if c == b'\\' {
                i += 2;
                continue;
            }
            if (in_str && c == b'"') || (in_chr && c == b'\'') {
                in_str = false;
                in_chr = false;
            }
            i += 1;
            continue;
        }

        // Plain code from here on.
        match c {
            b'/' if next == Some(b'/') => {
                in_line_comment = true;
                i += 2;
                continue;
            }
            b'/' if next == Some(b'*') => {
                in_block_comment = true;
                i += 2;
                continue;
            }
            b'"' => in_str = true,
            b'\'' => in_chr = true,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ => {}
        }
        code[i] = true;
        i += 1;
    }

    Scan {
        depth,
        unterminated: in_str || in_chr || in_block_comment,
        code,
    }
}

fn is_ident_byte(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

/// Every identifier appearing as code (not in comments or literals), in
/// order of first appearance. Fuel for completion: whatever the session has
/// mentioned is worth offering again.
pub fn identifiers(src: &str) -> Vec<String> {
    let sc = scan(src);
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if sc.code[i] && is_ident_byte(b[i]) {
            let start = i;
            while i < b.len() && sc.code[i] && is_ident_byte(b[i]) {
                i += 1;
            }
            if !b[start].is_ascii_digit() {
                out.push(src[start..i].to_string());
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Nothing but whitespace and comments — legal to type, nothing to run.
pub fn is_blank(src: &str) -> bool {
    let sc = scan(src);
    src.bytes()
        .zip(sc.code)
        .all(|(b, is_code)| !is_code || b.is_ascii_whitespace())
}

/// Could evaluating this expression change anything the session can observe?
///
/// An expression typed at the prompt is usually a question — `x + 1`,
/// `sizeof(int)` — and a question does not need to be replayed. Only what
/// might mutate state has to be kept, so this errs towards `true`: an extra
/// replay is cheap; silently dropped state is not.
///
/// A call is a `(` whose left operand — the previous non-whitespace *code*
/// byte, with comments and literal interiors invisible — ends in an
/// identifier, `)` or `]`. That nets `f()`, `f/**/()`, `(f)()`, `(*fp)()`
/// and `arr[0]()` alike. It also nets a cast like `(int)(x)`, which only a
/// symbol table could tell apart from a call; per the rule above it is kept.
pub fn may_have_side_effects(src: &str) -> bool {
    // Operators that only *contain* `=` without assigning.
    const COMPARISONS: [u8; 4] = *b"=!<>";
    // Call-like syntax that computes nothing at runtime.
    const PURE_CALLS: [&str; 3] = ["sizeof", "_Alignof", "alignof"];

    let sc = scan(src);
    let b = src.as_bytes();
    // Previous non-whitespace code byte, and the identifier ending there.
    let mut prev: u8 = 0;
    let mut prev_word: &str = "";

    let mut i = 0;
    while i < b.len() {
        if !sc.code[i] || b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let c = b[i];
        match c {
            b'=' => {
                // Token pairing is *raw* adjacency — `==` cannot span a
                // comment, and `f/**/()` is why the call check below must
                // NOT be raw. `<<=`/`>>=` assign even though their middle
                // byte looks like a comparison operator.
                let prev_raw = i.checked_sub(1).map(|p| b[p]);
                let prev2_raw = i.checked_sub(2).map(|p| b[p]);
                let shift_assign =
                    prev_raw.is_some_and(|p| p == b'<' || p == b'>') && prev2_raw == prev_raw;
                let is_comparison = !shift_assign
                    && (prev_raw.is_some_and(|p| COMPARISONS.contains(&p))
                        || b.get(i + 1) == Some(&b'='));
                if !is_comparison {
                    return true;
                }
            }
            b'+' | b'-' if b.get(i + 1) == Some(&c) => return true,
            b'(' => {
                if prev == b')' || prev == b']' {
                    return true;
                }
                if is_ident_byte(prev) && !PURE_CALLS.contains(&prev_word) {
                    return true;
                }
            }
            _ => {}
        }
        if is_ident_byte(c) {
            let start = i;
            while i < b.len() && sc.code[i] && is_ident_byte(b[i]) {
                i += 1;
            }
            prev_word = &src[start..i];
            prev = b[i - 1];
            continue;
        }
        prev = c;
        i += 1;
    }
    false
}

/// True when the input looks like a function signature whose body has not
/// been typed yet — `int f(int n)` with the `{` still to come on the next
/// line.
///
/// Brackets alone cannot tell this apart from a finished input: the parens
/// balance either way. Without the distinction, a function written in the
/// brace-on-its-own-line style is submitted a line early and, once the
/// missing-semicolon repair runs, quietly becomes a forward declaration.
pub fn awaits_body(input: &str) -> bool {
    // Keywords that make what follows an expression rather than a declarator,
    // so `return f(x)` is not mistaken for a signature.
    const NOT_DECLARATOR: &[&str] = &[
        "return", "sizeof", "if", "while", "for", "switch", "case", "goto", "do", "else",
    ];

    let sc = scan(input);
    if sc.depth != 0 || sc.unterminated {
        return false;
    }
    let t = input.trim_end();
    if !t.ends_with(')') {
        return false;
    }
    let b = t.as_bytes();
    let Some(open) = (0..b.len()).find(|&i| sc.code[i] && b[i] == b'(') else {
        return false;
    };
    let head = &t[..open];
    // A declarator's return type and name are just identifiers, `*` and
    // space. Any operator means this is an expression: `x = f(1)`.
    if !head
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '*' || c.is_whitespace())
    {
        return false;
    }
    let words: Vec<&str> = head
        .split(|c: char| c.is_whitespace() || c == '*')
        .filter(|s| !s.is_empty())
        .collect();
    // One word is a call, `f(1)`. Two or more is a type and a name.
    words.len() >= 2 && !NOT_DECLARATOR.contains(&words[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_signature_waiting_for_its_body() {
        assert!(awaits_body("int fact(int n)"));
        assert!(awaits_body("static unsigned long g(void)"));
        assert!(awaits_body("int *make(int n)"));
        assert!(awaits_body("int*make(int n)"));
    }

    #[test]
    fn leaves_finished_input_alone() {
        assert!(!awaits_body("fact(5)"));
        assert!(!awaits_body("printf(\"hi\")"));
        assert!(!awaits_body("return fact(5)"));
        assert!(!awaits_body("x = fact(5)"));
        // A forward declaration is complete as typed.
        assert!(!awaits_body("int fact(int n);"));
        // So is a definition whose body is already closed.
        assert!(!awaits_body("int fact(int n) { return 1; }"));
    }

    #[test]
    fn depth_ignores_brackets_in_literals() {
        assert_eq!(scan("f(\"(((\")").depth, 0);
        assert_eq!(scan("x = '{';").depth, 0);
        assert_eq!(scan("void f(void) {").depth, 1);
    }

    #[test]
    fn detects_unterminated() {
        assert!(scan("\"abc").unterminated);
        assert!(scan("/* open").unterminated);
        assert!(!scan("\"a\\\"b\"").unterminated);
    }

    #[test]
    fn harvests_code_identifiers_only() {
        assert_eq!(
            identifiers("int x = f(y); // z"),
            vec!["int", "x", "f", "y"]
        );
        assert_eq!(identifiers("puts(\"not_this\")"), vec!["puts"]);
        assert!(identifiers("42 + 0x1F").is_empty());
    }

    #[test]
    fn blank_means_comments_and_whitespace_only() {
        assert!(is_blank("// just a note"));
        assert!(is_blank("/* block */"));
        assert!(is_blank("   "));
        assert!(!is_blank("x // trailing"));
        assert!(!is_blank("\"text\""));
    }

    #[test]
    fn treats_questions_as_pure() {
        assert!(!may_have_side_effects("x + 1"));
        assert!(!may_have_side_effects("3.0 / 2"));
        assert!(!may_have_side_effects("sizeof(int)"));
        assert!(!may_have_side_effects("sizeof (arr) / sizeof (arr[0])"));
        assert!(!may_have_side_effects("a == b"));
        assert!(!may_have_side_effects("a != b && c <= d"));
        assert!(!may_have_side_effects("-1 > 0u"));
        // An `=` inside a literal is not an assignment.
        assert!(!may_have_side_effects("\"a = b\""));
    }

    #[test]
    fn treats_mutation_and_calls_as_impure() {
        assert!(may_have_side_effects("x = 5"));
        assert!(may_have_side_effects("x += 5"));
        assert!(may_have_side_effects("x <<= 1"));
        assert!(may_have_side_effects("x >>= 1"));
        assert!(may_have_side_effects("x++"));
        assert!(may_have_side_effects("--x"));
        assert!(may_have_side_effects("puts(\"hi\")"));
        assert!(may_have_side_effects("fact(5)"));
    }

    #[test]
    fn detects_calls_hidden_by_comments_or_parens() {
        // Each of these once slipped past the heuristic and lost session
        // state: the call ran, but its effects were never replayed.
        assert!(may_have_side_effects("f/**/()"));
        assert!(may_have_side_effects("f /* comment */ ()"));
        assert!(may_have_side_effects("(f)()"));
        assert!(may_have_side_effects("(*fp)()"));
        assert!(may_have_side_effects("arr[0]()"));
    }

    #[test]
    fn undecidable_forms_are_kept_not_dropped() {
        // A cast is indistinguishable from a call without a symbol table;
        // the conservative answer is to keep it.
        assert!(may_have_side_effects("(int)(x)"));
        // A comment splitting `+ +` is two unary pluses, not an increment.
        assert!(!may_have_side_effects("x +/**/+ y"));
    }
}
