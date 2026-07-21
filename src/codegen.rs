//! Assembling a complete C program from the session's accumulated input.
//!
//! The model is accumulate-and-replay: every evaluation rebuilds and reruns
//! the whole program from scratch. Session variables are therefore just
//! ordinary locals in `main` that get re-created on each run — no globals, no
//! symbol table, no splitting of declarations from their initialisers.
//!
//! The cost is that side effects replay too. Markers written into the output
//! streams let the caller show only what the newest input produced.

use std::collections::BTreeSet;

use crate::lex;
use crate::session::Session;

/// Everything after this marker in a stream came from the newest input.
pub const M_NEW: &str = "\x01\x02c-shell/new\x02\x01";
/// Everything after this marker is the `Out[n]` value rather than program output.
pub const M_VAL: &str = "\x01\x02c-shell/val\x02\x01";
/// Reaching this marker proves that the newest input returned normally.
pub const M_DONE: &str = "\x01\x02c-shell/done\x02\x01";

const HEADERS: &str = "\
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <stddef.h>
#include <limits.h>
#include <ctype.h>
";

/// Value printing dispatches through `_Generic` on the *function*, not on the
/// call. Selecting a call expression would make every unselected branch get
/// type-checked against the wrong argument type and fail to compile.
///
/// Each printer opens by marking the stream, and that placement is load
/// bearing: an argument is fully evaluated before the call it belongs to, so
/// anything the expression printed on its own way to a value has already been
/// flushed by the time the marker appears. Emitting the marker at the call
/// site instead would file `puts("hi")`'s own output under its return value.
const RUNTIME: &str = r##"
#define CS_VAL() fputs(CS_M_VAL, stdout)

static inline void cs_p_b (_Bool v)              { CS_VAL(); printf("%s\n", v ? "true" : "false"); }
static inline void cs_p_c (char v)               { CS_VAL(); printf("'%c' (%d)\n", (v >= 32 && v < 127) ? v : '?', (int)v); }
static inline void cs_p_i (long long v)          { CS_VAL(); printf("%lld\n", v); }
static inline void cs_p_u (unsigned long long v) { CS_VAL(); printf("%llu\n", v); }
static inline void cs_p_d (double v)             { CS_VAL(); printf("%g\n", v); }
static inline void cs_p_ld(long double v)        { CS_VAL(); printf("%Lg\n", v); }
static inline void cs_p_s (const char *v)        { CS_VAL(); if (v) printf("\"%s\"\n", v); else printf("(null)\n"); }
static inline void cs_p_p (const void *v)        { CS_VAL(); printf("%p\n", (void *)v); }

#define CS_PRINT(x) _Generic((x),                                        \
    _Bool: cs_p_b,              char: cs_p_c,                            \
    signed char: cs_p_i,        unsigned char: cs_p_u,                   \
    short: cs_p_i,              unsigned short: cs_p_u,                  \
    int: cs_p_i,                unsigned int: cs_p_u,                    \
    long: cs_p_i,               unsigned long: cs_p_u,                   \
    long long: cs_p_i,          unsigned long long: cs_p_u,              \
    float: cs_p_d,              double: cs_p_d,                          \
    long double: cs_p_ld,                                                \
    char *: cs_p_s,             const char *: cs_p_s,                    \
    default: cs_p_p)(x)

/* C has no general type reflection.  This portable _Generic table reports
   scalar types and scalar pointers.  The controlling expression is not
   evaluated; its normal lvalue/array/function conversions still apply. */
#define CS_PTR_TYPES(T, N)                                                \
    T *: N " *",                    const T *: "const " N " *",         \
    volatile T *: "volatile " N " *",                                   \
    const volatile T *: "const volatile " N " *"

#define CS_TYPE_ASSOCIATIONS                                             \
    _Bool: "_Bool",             char: "char",                           \
    signed char: "signed char", unsigned char: "unsigned char",         \
    short: "short",             unsigned short: "unsigned short",       \
    int: "int",                 unsigned int: "unsigned int",           \
    long: "long",               unsigned long: "unsigned long",         \
    long long: "long long",     unsigned long long: "unsigned long long", \
    float: "float",             double: "double",                       \
    long double: "long double",                                         \
    CS_PTR_TYPES(void, "void"),                                          \
    CS_PTR_TYPES(_Bool, "_Bool"),                                        \
    CS_PTR_TYPES(char, "char"),                                          \
    CS_PTR_TYPES(signed char, "signed char"),                            \
    CS_PTR_TYPES(unsigned char, "unsigned char"),                        \
    CS_PTR_TYPES(short, "short"),                                        \
    CS_PTR_TYPES(unsigned short, "unsigned short"),                      \
    CS_PTR_TYPES(int, "int"),                                            \
    CS_PTR_TYPES(unsigned int, "unsigned int"),                          \
    CS_PTR_TYPES(long, "long"),                                          \
    CS_PTR_TYPES(unsigned long, "unsigned long"),                        \
    CS_PTR_TYPES(long long, "long long"),                                \
    CS_PTR_TYPES(unsigned long long, "unsigned long long"),              \
    CS_PTR_TYPES(float, "float"),                                        \
    CS_PTR_TYPES(double, "double"),                                      \
    CS_PTR_TYPES(long double, "long double")

#define CS_TYPE_NAME(x) _Generic((x), CS_TYPE_ASSOCIATIONS,              \
    default: "<unrecognized type>")

#define CS_MARK(m) do { \
    fputs(m, stdout); fflush(stdout); \
    fputs(m, stderr); fflush(stderr); \
} while (0)
"##;

/// Where a new input is spliced into the generated program.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    /// An expression, printed via `_Generic` on its own type.
    Expr,
    /// A statement or block-scope declaration, appended inside `main`.
    Stmt,
    /// A function definition, `#include`, `#define` or type definition.
    FileScope,
}

pub struct Program {
    pub src: String,
    /// 1-based line in `src` where the user's new text starts. Diagnostics are
    /// mapped back to input-relative lines through this.
    pub new_start_line: usize,
    /// How many lines that text occupies, so diagnostics landing just past it
    /// are not misattributed to the user.
    pub new_line_count: usize,
    /// True when the input sits inside a `CS_PRINT((` or `CS_TYPE_NAME((`
    /// wrapper. MSVC's traditional preprocessor attributes diagnostics from a
    /// multi-line macro invocation to the invocation's *first* line — the
    /// wrapper line just above the input — so the remapper must know it may
    /// pull those anchors back into the input.
    pub wrapped: bool,
}

/// Build the program for evaluating `input` in `slot` against `session`.
pub fn build(session: &Session, input: &str, slot: Slot) -> Program {
    build_inner(session, input, slot, false, false)
}

/// Build a program that evaluates an expression without trying to print it.
/// Used only after the normal value-printer trial failed, to distinguish an
/// unsupported value category (struct/complex/void/etc.) from invalid C.
pub fn build_expr_probe(session: &Session, input: &str) -> Program {
    build_inner(session, input, Slot::Expr, true, false)
}

/// Build a non-mutating `%type` query. `_Generic` selects a type-name string
/// without evaluating `input`; M_VAL keeps the result out of live program
/// output so the magic renderer can print it itself.
pub fn build_type_probe(session: &Session, input: &str) -> Program {
    build_inner(session, input, Slot::Expr, false, true)
}

fn build_inner(
    session: &Session,
    input: &str,
    slot: Slot,
    silent_expr: bool,
    type_probe: bool,
) -> Program {
    let mut src = String::with_capacity(4096);
    let mut new_start_line = 1usize;

    src.push_str(HEADERS);
    // Ahead of the runtime, which expands them.
    src.push_str(&format!("#define CS_M_NEW \"{}\"\n", escape(M_NEW)));
    src.push_str(&format!("#define CS_M_VAL \"{}\"\n", escape(M_VAL)));
    src.push_str(&format!("#define CS_M_DONE \"{}\"\n", escape(M_DONE)));
    src.push_str(RUNTIME);

    for item in &session.file_items {
        src.push_str(item);
        src.push('\n');
    }

    if slot == Slot::FileScope {
        new_start_line = src.lines().count() + 1;
        src.push_str(input);
        src.push('\n');
    }

    src.push_str("\nint main(void)\n{\n");
    for stmt in &session.stmts {
        src.push_str(stmt);
        src.push('\n');
    }
    src.push_str("    CS_MARK(CS_M_NEW);\n");

    match slot {
        Slot::FileScope => {}
        Slot::Stmt => {
            new_start_line = src.lines().count() + 1;
            src.push_str(input);
            src.push('\n');
        }
        Slot::Expr => {
            // The input sits on a line of its own so a diagnostic inside it
            // lands on text the user actually typed, at the column they see.
            new_start_line = src.lines().count() + if type_probe { 3 } else { 2 };
            if type_probe {
                let aggregates = aggregate_types(session, input);
                src.push_str("    CS_VAL();\n");
                if aggregates.is_empty() {
                    src.push_str(&format!("    puts(CS_TYPE_NAME((\n{input}\n    )));\n"));
                } else {
                    src.push_str(&format!("    puts(_Generic((\n{input}\n    ),\n"));
                    src.push_str("        CS_TYPE_ASSOCIATIONS,\n");
                    for (ty, category) in aggregates {
                        let name = ty
                            .strip_prefix("struct ")
                            .or_else(|| ty.strip_prefix("union "))
                            .unwrap_or(&ty);
                        src.push_str(&format!("        {ty}: \"{category} {name}\",\n"));
                    }
                    src.push_str("        default: \"<unrecognized type>\"));\n");
                }
            } else if silent_expr {
                src.push_str(&format!("    (void)(\n{input}\n    );\n"));
            } else {
                src.push_str(&format!("    CS_PRINT((\n{input}\n    ));\n"));
            }
        }
    }

    src.push_str("    CS_MARK(CS_M_DONE);\n");
    src.push_str("    return 0;\n}\n");
    let new_line_count = input.lines().count().max(1);
    Program {
        src,
        new_start_line,
        new_line_count,
        wrapped: slot == Slot::Expr && !silent_expr,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TypeToken {
    Ident(String),
    LBrace,
    RBrace,
    Semi,
    Other,
}

/// Collect aggregate types that have a reusable spelling. `_Generic` has no
/// wildcard for "any struct", so each complete named tag (plus the common
/// anonymous-typedef form) is added to this query's association list.
fn aggregate_types(session: &Session, input: &str) -> Vec<(String, &'static str)> {
    let mut found = BTreeSet::new();
    for src in session
        .file_items
        .iter()
        .chain(session.stmts.iter())
        .map(String::as_str)
        .chain(std::iter::once(input))
    {
        collect_aggregate_types(src, &mut found);
    }
    found.into_iter().collect()
}

fn collect_aggregate_types(src: &str, found: &mut BTreeSet<(String, &'static str)>) {
    let tokens = type_tokens(src);
    for (i, token) in tokens.iter().enumerate() {
        let TypeToken::Ident(keyword) = token else {
            continue;
        };
        let category = match keyword.as_str() {
            "struct" => "Struct",
            "union" => "Union",
            _ => continue,
        };

        // `struct Tag { ... }`: the tag is complete and can legally appear as
        // a generic association type. Mere forward declarations are skipped.
        if let (Some(TypeToken::Ident(name)), Some(TypeToken::LBrace)) =
            (tokens.get(i + 1), tokens.get(i + 2))
        {
            found.insert((format!("{keyword} {name}"), category));
            continue;
        }

        // `typedef struct { ... } Name;`: anonymous aggregates have no tag,
        // but this simple and overwhelmingly common typedef gives us a stable
        // spelling. More elaborate multi-declarator typedefs are left unknown
        // rather than guessed incorrectly.
        let preceded_by_typedef = i
            .checked_sub(1)
            .and_then(|j| tokens.get(j))
            .is_some_and(|t| matches!(t, TypeToken::Ident(word) if word == "typedef"));
        if !preceded_by_typedef || tokens.get(i + 1) != Some(&TypeToken::LBrace) {
            continue;
        }
        let mut depth = 0usize;
        let mut close = None;
        for (j, t) in tokens.iter().enumerate().skip(i + 1) {
            match t {
                TypeToken::LBrace => depth += 1,
                TypeToken::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(j);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else { continue };
        if let (Some(TypeToken::Ident(alias)), Some(TypeToken::Semi)) =
            (tokens.get(close + 1), tokens.get(close + 2))
        {
            found.insert((alias.clone(), category));
        }
    }
}

fn type_tokens(src: &str) -> Vec<TypeToken> {
    let sc = lex::scan(src);
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !sc.code[i] {
            i += 1;
            continue;
        }
        if b[i] == b'_' || b[i].is_ascii_alphabetic() {
            let start = i;
            i += 1;
            while i < b.len() && sc.code[i] && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            out.push(TypeToken::Ident(src[start..i].to_string()));
            continue;
        }
        match b[i] {
            b'{' => out.push(TypeToken::LBrace),
            b'}' => out.push(TypeToken::RBrace),
            b';' => out.push(TypeToken::Semi),
            c if !c.is_ascii_whitespace() => out.push(TypeToken::Other),
            _ => {}
        }
        i += 1;
    }
    out
}

fn escape(s: &str) -> String {
    s.bytes().map(|b| format!("\\x{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_reusable_struct_and_union_type_spellings() {
        let mut found = BTreeSet::new();
        collect_aggregate_types(
            "struct Point { int x; int y; };\n\
             union Value { int i; double d; };\n\
             typedef struct { int code; } Result;\n\
             typedef union { int code; } *ResultPtr;\n\
             struct Forward; struct Forward *ptr;\n\
             /* union Fake { int no; }; */",
            &mut found,
        );
        assert!(found.contains(&("struct Point".into(), "Struct")));
        assert!(found.contains(&("union Value".into(), "Union")));
        assert!(found.contains(&("Result".into(), "Struct")));
        assert!(!found.iter().any(|(ty, _)| ty.contains("ResultPtr")));
        assert!(!found.iter().any(|(ty, _)| ty.contains("Forward")));
        assert!(!found.iter().any(|(ty, _)| ty.contains("Fake")));
    }

    #[test]
    fn type_probe_adds_session_aggregate_associations() {
        let mut session = Session::default();
        session.file_items.push("struct Pair { int x, y; };".into());
        session.stmts.push("struct Pair pair = { 1, 2 };".into());
        let program = build_type_probe(&session, "pair");
        assert!(program.src.contains("struct Pair: \"Struct Pair\""));
    }
}
