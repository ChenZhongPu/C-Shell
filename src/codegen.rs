//! Assembling a complete C program from the session's accumulated input.
//!
//! The model is accumulate-and-replay: every evaluation rebuilds and reruns
//! the whole program from scratch. Session variables are therefore just
//! ordinary locals in `main` that get re-created on each run — no globals, no
//! symbol table, no splitting of declarations from their initialisers.
//!
//! The cost is that side effects replay too. Markers written into the output
//! streams let the caller show only what the newest input produced.

use crate::session::Session;

/// Everything after this marker in a stream came from the newest input.
pub const M_NEW: &str = "\x01\x02c-shell/new\x02\x01";
/// Everything after this marker is the `Out[n]` value rather than program output.
pub const M_VAL: &str = "\x01\x02c-shell/val\x02\x01";

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
}

/// Build the program for evaluating `input` in `slot` against `session`.
pub fn build(session: &Session, input: &str, slot: Slot) -> Program {
    let mut src = String::with_capacity(4096);
    let mut new_start_line = 1usize;

    src.push_str(HEADERS);
    // Ahead of the runtime, which expands them.
    src.push_str(&format!("#define CS_M_NEW \"{}\"\n", escape(M_NEW)));
    src.push_str(&format!("#define CS_M_VAL \"{}\"\n", escape(M_VAL)));
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
            new_start_line = src.lines().count() + 2;
            src.push_str(&format!("    CS_PRINT((\n{input}\n    ));\n"));
        }
    }

    src.push_str("    return 0;\n}\n");
    let new_line_count = input.lines().count().max(1);
    Program {
        src,
        new_start_line,
        new_line_count,
    }
}

fn escape(s: &str) -> String {
    s.bytes().map(|b| format!("\\x{b:02x}")).collect()
}
