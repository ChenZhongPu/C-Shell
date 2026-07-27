//! `%` commands, in the spirit of IPython's magics.
//!
//! A line starting with `%` is never valid C, so the prefix needs no escaping
//! rule to stay unambiguous.

use anyhow::Result;
use std::io::Write as _;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use crate::codegen::{self, Slot};
use crate::eval::{Eval, Evaluator, UnicodeEncoding};
use crate::proc;
use crate::session::Session;
use crate::std_index;
use crate::ui::Ui;

pub enum Action {
    Continue,
    Quit,
    /// Pre-populate the next interactive prompt without evaluating anything.
    Prefill(String),
}

/// Pretty-print C source through clang-format when it is available, or
/// return it unchanged when it is not.
///
/// Both source views are built by string concatenation, and interactively
/// typed inputs carry prompt-width indentation padding. Formatting is
/// presentation only: evaluation always compiles codegen's unformatted raw
/// program, regardless of which `%src` view is printed.
fn format_c(src: &str) -> String {
    static CLANG_FORMAT: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    let Some(path) = CLANG_FORMAT.get_or_init(|| which::which("clang-format").ok()) else {
        return src.to_string();
    };
    let run = || -> std::io::Result<String> {
        let mut cmd = Command::new(path);
        // Match the generated code's own 4-space style.
        cmd.arg("-style={BasedOnStyle: LLVM, IndentWidth: 4}");
        let out = proc::run_with_input(&mut cmd, Duration::from_secs(3), src.as_bytes())?;
        if out.status.is_some_and(|st| st.success()) && !out.stdout_truncated {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(std::io::Error::other("clang-format failed or timed out"))
        }
    };
    run().unwrap_or_else(|_| src.to_string())
}

/// Resolve `%edit [n]` without changing the session. `None` means that the
/// no-argument form had no previous C input; malformed or missing numbered
/// forms carry the exact user-facing diagnostic.
fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1e-6 {
        format!("{} ns", d.as_nanos())
    } else if secs < 1e-3 {
        format!("{:.2} µs", secs * 1e6)
    } else if secs < 1.0 {
        format!("{:.2} ms", secs * 1e3)
    } else {
        format!("{:.2} s", secs)
    }
}

fn edit_input(args: &[&str], session: &Session) -> std::result::Result<Option<String>, String> {
    match args {
        [] => Ok(session.last_input().map(str::to_string)),
        [number] => {
            let number = number
                .parse::<usize>()
                .map_err(|_| "usage: %edit [input-number]".to_string())?;
            session
                .input(number)
                .map(str::to_string)
                .map(Some)
                .ok_or_else(|| format!("no C input In[{number}]"))
        }
        _ => Err("usage: %edit [input-number]".to_string()),
    }
}

fn render_probe(result: Eval, ui: &Ui, missing: &str) {
    match result {
        Eval::CompileError(diag) => println!("{}", ui.err(diag.trim_end())),
        Eval::Done(o) => {
            if o.streamed_output_needs_newline {
                println!();
            }
            if !o.warnings.trim().is_empty() {
                println!("{}", ui.warn(o.warnings.trim_end()));
            }
            if !o.output.is_empty() {
                print!("{}", o.output);
                if !o.output.ends_with('\n') {
                    println!();
                }
            }
            if !o.errors.is_empty() {
                print!("{}", ui.err(&o.errors));
            }
            if let Some(msg) = o.abnormal {
                println!("{}", ui.err(&msg));
            } else if let Some(value) = o.value {
                println!("{value}");
            } else {
                println!("{}", ui.err(missing));
            }
        }
    }
}

/// Compilers anchor an unsupported `_Generic` selection differently. Clang
/// keeps the primary error on the user expression, while GCC/MSVC may leave
/// only a macro-expansion note after generated diagnostics are removed.
fn unsupported_bits_type(diag: &str) -> bool {
    if diag.trim().is_empty() {
        return true;
    }
    let lower = diag.to_ascii_lowercase();
    let names_dispatch = lower.contains("_generic")
        || lower.contains("generic association")
        || lower.contains("cs_bits");
    let reports_mismatch = lower.contains("not compatible")
        || lower.contains("no compatible")
        || lower.contains("expansion of macro");
    names_dispatch && reports_mismatch
}

fn unsupported_unicode_type(diag: &str) -> bool {
    if diag.trim().is_empty() {
        return true;
    }
    let lower = diag.to_ascii_lowercase();
    let names_dispatch = lower.contains("_generic")
        || lower.contains("generic association")
        || lower.contains("cs_print_unicode");
    let reports_mismatch = lower.contains("not compatible")
        || lower.contains("no compatible")
        || lower.contains("expansion of macro");
    names_dispatch && reports_mismatch
}

fn unicode_query(tail: &str) -> std::result::Result<(Option<usize>, &str), String> {
    const MAX_EXPLICIT_UNITS: usize = 4096;

    let tail = tail.trim();
    if tail.is_empty() {
        return Err("missing expression".to_string());
    }
    let Some(after_flag) = tail.strip_prefix("-n") else {
        return Ok((None, tail));
    };
    if after_flag
        .as_bytes()
        .first()
        .is_some_and(|byte| !byte.is_ascii_whitespace())
    {
        return Ok((None, tail));
    }

    let after_flag = after_flag.trim_start();
    let count_end = after_flag
        .find(char::is_whitespace)
        .unwrap_or(after_flag.len());
    let count_text = &after_flag[..count_end];
    let expression = after_flag[count_end..].trim();
    let count = count_text
        .parse::<usize>()
        .map_err(|_| "-n requires a non-negative code-unit count".to_string())?;
    if count > MAX_EXPLICIT_UNITS {
        return Err(format!("-n is limited to {MAX_EXPLICIT_UNITS} code units"));
    }
    if expression.is_empty() {
        return Err("missing expression after -n <count>".to_string());
    }
    Ok((Some(count), expression))
}

fn is_c_identifier(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn auto_included(header: &str) -> bool {
    let include = format!("#include {header}");
    codegen::HEADERS.lines().any(|line| line.trim() == include)
}

fn cppreference_header_url(header: &str) -> Option<String> {
    let stem = header.strip_prefix('<')?.strip_suffix(".h>")?;
    stem.bytes()
        .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
        .then(|| format!("https://en.cppreference.com/c/header/{stem}"))
}

fn render_where_header_table(headers: &[&str], ui: &Ui) {
    const HEADER_LABEL: &str = "header";
    const DOCUMENTATION_LABEL: &str = "documentation";

    let rows = headers
        .iter()
        .map(|header| {
            (
                *header,
                cppreference_header_url(header).unwrap_or_else(|| "-".to_string()),
            )
        })
        .collect::<Vec<_>>();
    let header_width = rows
        .iter()
        .map(|(header, _)| header.len())
        .max()
        .unwrap_or(0)
        .max(HEADER_LABEL.len());
    let documentation_width = rows
        .iter()
        .map(|(_, url)| url.len())
        .max()
        .unwrap_or(0)
        .max(DOCUMENTATION_LABEL.len());
    let border = format!(
        "+-{}-+-{}-+",
        "-".repeat(header_width),
        "-".repeat(documentation_width)
    );

    println!("headers:");
    println!("{border}");
    println!("| {HEADER_LABEL:<header_width$} | {DOCUMENTATION_LABEL:<documentation_width$} |");
    println!("{border}");
    for (header, url) in rows {
        let documentation = if url == "-" {
            url.clone()
        } else {
            ui.hyperlink(&url)
        };
        // Pad from the visible URL width, not from the OSC 8-wrapped string:
        // hyperlink control bytes occupy no terminal columns.
        println!(
            "| {header:<header_width$} | {documentation}{} |",
            " ".repeat(documentation_width - url.len())
        );
    }
    println!("{border}");
}

fn render_where(name: &str, ev: &Evaluator, ui: &Ui) {
    let Some(found) = std_index::lookup(name) else {
        println!(
            "{}",
            ui.err(&format!(
                "{name} was not found in c-shell's ISO C standard library index"
            ))
        );
        return;
    };

    println!("name: {}", found.name);
    let kinds = found
        .kinds()
        .into_iter()
        .map(std_index::Kind::label)
        .collect::<Vec<_>>()
        .join(" / ");
    println!("kind: {kinds}");

    let headers = found.headers();
    render_where_header_table(&headers, ui);
    if let Some(signature) = found.signature {
        println!("signature: {signature}");
    }

    let since = found.since();
    if let Some(removed) = found.removed() {
        let last = removed
            .previous()
            .expect("no indexed identifier is removed in C89");
        println!(
            "ISO C availability: {}–{}; removed in {}",
            since.label(),
            last.label(),
            removed.label()
        );
    } else {
        println!("ISO C availability: {} and later", since.label());
    }

    let mode = if ev.tc.std.is_empty() {
        ev.tc.default_std.as_deref()
    } else {
        Some(ev.tc.std.as_str())
    };
    if let Some(mode) = mode {
        if let Some(standard) = std_index::CStandard::from_mode(mode) {
            println!(
                "selected mode: {mode} ({})",
                if found.available_in(standard) {
                    "available"
                } else {
                    "not available as an ISO C library identifier"
                }
            );
        } else {
            println!("selected mode: {mode}");
        }
    }

    let included = headers
        .iter()
        .copied()
        .filter(|header| auto_included(header))
        .collect::<Vec<_>>();
    if included.is_empty() {
        println!("auto-included by c-shell: no");
    } else {
        println!("auto-included by c-shell: yes ({})", included.join(", "));
    }
    if let Some(note) = found.note {
        println!("note: {note}");
    }
}

const HELP: &str = "Commands:
  %help [--verbose]  show commands; --verbose adds usage notes
  %quit / %exit      quit (Ctrl-D works too)
  %clear             clear the screen without changing the session
  %reset             clear the session and start fresh
  %src [--raw]       show user C; --raw includes generated runtime/protocol
  %header            list the headers included in every program
  %edit [n]          copy latest or In[n] into the prompt for editing
  %type <expression> query an expression's type without evaluating it
  %bits <expression> inspect a scalar value using lowercase hexadecimal
  %Bits <expression> same as %bits, with uppercase hexadecimal
  %utf8/%utf16/%utf32 [-n N] <expression> decode Unicode code units
  %where <identifier> find an ISO C library identifier's standard header
  %time <code...>    time the execution of a statement or expression once
  %timeit <code...>  benchmark a statement or expression over multiple loops
  %undo              undo the most recent retained state change
  %cc [path]         show or switch the C compiler
  %std [std]         show or switch the language standard (c11/c17/c23);
                     %std default returns to the compiler's own default";

/// The behavioral rules that are not guessable from the command list. Kept
/// behind  so the default help stays a one-screen reference.
const HELP_NOTES: &str = "Notes:
  A bare expression prints its value; a trailing ';' runs it silently.
  A completed if waits for a blank continuation line; type else / else if
  there instead to continue it. Other closed blocks submit immediately, but
  a struct/union/enum definition waits for its mandatory trailing ';'.
  Function definitions, #include and typedef go to file scope automatically.
  %edit n can reopen any C In[n] from this session, including a failed one;
  it only fills the next prompt. Modify it and press Enter to submit it under
  a new number; the original numbered input remains unchanged.
  c-shell supplies main(); enter its body as statements and omit final return.
  Redeclaring a local opens a nested shadowing scope. Redefining a function
  or type replaces the prior file-scope input only if the compiler accepts
  the complete rewritten session; functions are never demoted into main.
  %type uses _Generic matching: scalar types and scalar pointers are named;
  complete named structs/unions report e.g. Struct Point or Union Value;
  simple anonymous typedefs use the typedef name. Other aliases and top-level
  qualifiers are canonicalized, and arrays/functions undergo their normal
  expression conversions.
  %bits/%Bits evaluate a scalar expression once, then show its type, value,
  size, hexadecimal and binary representation, memory bytes and byte order.
  Command spelling is case-sensitive; only %Bits selects uppercase hex.
  IEEE-754 float/double values also show sign, exponent and fraction fields;
  aggregates, arrays and function pointers are not supported.
  %utf8/%utf16/%utf32 read integer arrays or pointers as Unicode code units.
  By default they stop at NUL or 100 units; -n N reads exactly N units, up to
  4096. Invalid Unicode is reported without replacement characters. Pointer
  reads are explicit but still require the addressed memory to be valid.
  %where uses a built-in ISO C89-C23 index, not the host's transitive header
  visibility. It also links each matching header to cppreference. POSIX,
  platform/compiler extensions and user names are excluded.
  %time evaluates an expression or statement once, displays its output/value,
  and reports wall-clock execution time. Side effects are retained in session.
  %timeit benchmarks an expression or statement in an auto-ranged loop over
  multiple runs without modifying the session state.
  Struct values use designated members; nested known structs and arrays
  expand, but pointer members are shown only as addresses or NULL. Use an
  explicit member expression (p.name) or dereference (*ptr) to drill down.
  Arrays print their elements, one or two dimensions deep, bounded at 100 per
  dimension; a real pointer still prints as an address, and elements with no
  printer of their own show <unprintable>.
  Pure bare expressions (x + 1, sizeof(int)) are evaluated and forgotten.
  Statements and bare expressions that may have effects are kept.
  Direct scanf calls record one private input line per dynamic request; later
  replay uses that tape, including calls in functions and loops. Other known
  file/input/process APIs warn because their external effects may repeat.";

pub fn handle(line: &str, session: &mut Session, ev: &mut Evaluator, ui: &Ui) -> Result<Action> {
    let body = line.trim().trim_start_matches('%');
    let mut parts = body.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    // Unlike compiler paths/standards, a type query may itself contain spaces;
    // preserve its spelling rather than rebuilding it from split words.
    let tail = body.get(cmd.len()..).unwrap_or("").trim();

    match cmd {
        "help" | "h" | "?" => match rest.as_slice() {
            [] => println!("{HELP}"),
            [flag] if *flag == "--verbose" => println!(
                "{HELP}

{HELP_NOTES}"
            ),
            _ => println!("{}", ui.err("usage: %help [--verbose]")),
        },
        "quit" | "exit" | "q" => return Ok(Action::Quit),

        "clear" => {
            // ANSI erase-display + cursor-home. This is terminal control, not
            // color styling, so --no-color deliberately does not disable it.
            print!("[2J[H");
            let _ = std::io::stdout().flush();
        }

        "reset" => {
            session.reset();
            println!("{}", ui.dim("session cleared"));
        }

        "undo" => {
            if session.undo().is_some() {
                println!("{}", ui.dim("undo successful"));
            } else {
                println!("{}", ui.err("nothing to undo"));
            }
        }

        "header" | "headers" => {
            println!(
                "{}",
                ui.dim("Included in every program (no #include needed):")
            );
            print!("{}", codegen::HEADERS);
        }

        "src" => match rest.as_slice() {
            [] => println!("{}", format_c(&codegen::build_user_view(session))),
            [flag] if *flag == "--raw" => {
                let prog = codegen::build(session, "", Slot::Stmt);
                println!("{}", format_c(&prog.src));
            }
            _ => println!("{}", ui.err("usage: %src [--raw]")),
        },

        "edit" => match edit_input(&rest, session) {
            Ok(Some(input)) => return Ok(Action::Prefill(input)),
            Ok(None) => println!("{}", ui.dim("nothing to edit")),
            Err(message) => println!("{}", ui.err(&message)),
        },

        "type" => {
            if tail.is_empty() {
                println!("{}", ui.err("usage: %type <expression>"));
            } else {
                render_probe(
                    ev.type_of(session, tail)?,
                    ui,
                    "type query produced no result",
                );
            }
        }

        bits_command @ ("bits" | "Bits") => {
            let uppercase = bits_command == "Bits";
            if tail.is_empty() {
                println!(
                    "{}",
                    ui.err(&format!("usage: %{bits_command} <expression>"))
                );
            } else {
                let result = ev.bits_of(session, tail, uppercase)?;
                match result {
                    Eval::CompileError(diag) if unsupported_bits_type(&diag) => {
                        println!(
                            "{}",
                            ui.err(&format!(
                                "%{bits_command} supports standard scalar values and pointers to scalar types"
                            ))
                        );
                    }
                    result => render_probe(result, ui, "bits query produced no result"),
                }
            }
        }

        unicode_command @ ("utf8" | "utf16" | "utf32") => {
            let encoding = match unicode_command {
                "utf8" => UnicodeEncoding::Utf8,
                "utf16" => UnicodeEncoding::Utf16,
                "utf32" => UnicodeEncoding::Utf32,
                _ => unreachable!(),
            };
            match unicode_query(tail) {
                Err(message) => println!(
                    "{}",
                    ui.err(&format!(
                        "usage: %{unicode_command} [-n code-units] <expression> ({message})"
                    ))
                ),
                Ok((count, expression)) => {
                    let result = ev.unicode_of(session, expression, encoding, count)?;
                    match result {
                        Eval::CompileError(diag) if unsupported_unicode_type(&diag) => {
                            println!(
                                "{}",
                                ui.err(&format!(
                                    "%{unicode_command} supports pointers and arrays of integer code-unit types"
                                ))
                            );
                        }
                        result => {
                            render_probe(result, ui, "Unicode query produced no result");
                        }
                    }
                }
            }
        }

        "where" => match rest.as_slice() {
            [name] if is_c_identifier(name) => render_where(name, ev, ui),
            _ => println!("{}", ui.err("usage: %where <identifier>")),
        },

        "time" => {
            if tail.is_empty() {
                println!("{}", ui.err("usage: %time <expression or statement>"));
            } else {
                let n = session.counter + 1;
                session.counter = n;
                session.remember_input(n, tail);

                match ev.eval(session, tail)? {
                    Eval::CompileError(diag) => println!("{}", ui.err(diag.trim_end())),
                    Eval::Done(o) => {
                        if o.streamed_output_needs_newline {
                            println!("{}", ui.inserted_newline_marker());
                        }
                        let note = |s: &str| println!("{}", ui.dim(s));
                        if o.rewritten.is_some() {
                            note("(missing semicolon added automatically)");
                        }
                        if !o.warnings.trim().is_empty() {
                            println!("{}", ui.warn(o.warnings.trim_end()));
                        }

                        if !o.errors.is_empty() {
                            print!("{}", ui.err(&o.errors));
                        }
                        if let Some(v) = &o.value {
                            println!("{}{}", ui.out_label(n), v);
                        }
                        let committed = o.rewritten.clone().unwrap_or_else(|| tail.to_string());
                        match &o.abnormal {
                            Some(msg) => {
                                println!("{}", ui.err(msg));
                                note("(input not kept in the session)");
                            }
                            None => {
                                if let Some(index) = o.file_replacement {
                                    session.replace_file(index, &committed);
                                } else if o.scoped_rebind {
                                    session.commit_scoped(&committed);
                                } else {
                                    session.commit(&committed, o.slot);
                                }
                                session.attach_stdin_events(o.stdin_events.clone());
                            }
                        }
                        println!(
                            "{}",
                            ui.dim(&format!("Wall time: {}", format_duration(o.duration)))
                        );
                    }
                }
            }
        }

        "timeit" => {
            if tail.is_empty() {
                println!("{}", ui.err("usage: %timeit <expression or statement>"));
            } else {
                match ev.timeit(session, tail)? {
                    Eval::CompileError(diag) => println!("{}", ui.err(diag.trim_end())),
                    Eval::Done(o) => {
                        if o.streamed_output_needs_newline {
                            println!("{}", ui.inserted_newline_marker());
                        }

                        if let Some(msg) = o.abnormal {
                            println!("{}", ui.err(&msg));
                        } else if let Some(report) = o.value {
                            println!("{report}");
                        }
                    }
                }
            }
        }

        "cc" => {
            if rest.is_empty() {
                println!("{}", ev.tc.describe());
            } else {
                let std = ev.tc.std.clone();
                let std_opt = if std.is_empty() {
                    None
                } else {
                    Some(std.as_str())
                };
                match crate::toolchain::Toolchain::detect(Some(rest[0]), std_opt) {
                    Ok(tc) => {
                        ev.set_toolchain(tc);
                        println!("{}", ev.tc.describe());
                    }
                    Err(e) => println!("{}", ui.err(&format!("{e}"))),
                }
            }
        }

        "std" => {
            if rest.is_empty() {
                println!("{}", ev.tc.describe());
            } else {
                let want = rest[0].trim_start_matches("-std=");
                // `%std default` hands control back to the compiler; the
                // re-detection re-runs the _Generic viability check too.
                let req = if want == "default" { None } else { Some(want) };
                let path = ev.tc.path.display().to_string();
                match crate::toolchain::Toolchain::detect(Some(&path), req) {
                    Ok(new) if req.is_none() || new.std == want => {
                        ev.set_toolchain(new);
                        println!("{}", ev.tc.describe());
                    }
                    _ => println!(
                        "{}",
                        ui.err(&format!("this compiler does not support -std={want}"))
                    ),
                }
            }
        }

        other => println!(
            "{}",
            ui.err(&format!("unknown command %{other} — try %help"))
        ),
    }
    Ok(Action::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_resolves_latest_and_numbered_inputs_without_mutating_them() {
        let mut session = Session::default();
        assert_eq!(edit_input(&[], &session).expect("empty lookup"), None);

        session.remember_input(1, "int value = 1;");
        session.remember_input(2, "value + 1");
        assert_eq!(
            edit_input(&[], &session).expect("latest lookup").as_deref(),
            Some("value + 1")
        );
        assert_eq!(
            edit_input(&["1"], &session)
                .expect("numbered lookup")
                .as_deref(),
            Some("int value = 1;")
        );
        assert_eq!(
            edit_input(&["7"], &session).expect_err("missing lookup"),
            "no C input In[7]"
        );
    }

    #[test]
    fn recognizes_cross_compiler_bits_type_mismatches() {
        assert!(unsupported_bits_type(
            "error: controlling expression type 'struct Pair' not compatible with any generic association type"
        ));
        assert!(unsupported_bits_type(
            "note: in expansion of macro ‘CS_BITS’"
        ));
        assert!(!unsupported_bits_type(
            "error: use of undeclared identifier 'missing_name'"
        ));
        assert!(!unsupported_bits_type(
            "compiler timed out after 10s and was killed"
        ));
    }

    #[test]
    fn parses_bounded_unicode_queries_without_rebuilding_expressions() {
        assert_eq!(unicode_query("text").unwrap(), (None, "text"));
        assert_eq!(
            unicode_query("-n 7 pointer + offset").unwrap(),
            (Some(7), "pointer + offset")
        );
        assert_eq!(unicode_query("-number").unwrap(), (None, "-number"));
        assert!(unicode_query("").is_err());
        assert!(unicode_query("-n nope text").is_err());
        assert!(unicode_query("-n 4097 text").is_err());
        assert!(unicode_query("-n 2").is_err());
    }

    #[test]
    fn recognizes_cross_compiler_unicode_type_mismatches() {
        assert!(unsupported_unicode_type(
            "error: controlling expression type 'struct Pair *' not compatible with any generic association type"
        ));
        assert!(unsupported_unicode_type(
            "note: in expansion of macro ‘CS_PRINT_UNICODE’"
        ));
        assert!(!unsupported_unicode_type(
            "error: use of undeclared identifier 'missing_name'"
        ));
    }

    #[test]
    fn recognizes_c_identifiers_for_where_queries() {
        assert!(is_c_identifier("printf"));
        assert!(is_c_identifier("_Exit"));
        assert!(!is_c_identifier(""));
        assert!(!is_c_identifier("printf()"));
        assert!(!is_c_identifier("struct tm"));
    }

    #[test]
    fn maps_standard_headers_to_cppreference_pages() {
        assert_eq!(
            cppreference_header_url("<uchar.h>").as_deref(),
            Some("https://en.cppreference.com/c/header/uchar")
        );
        assert_eq!(
            cppreference_header_url("<stdckdint.h>").as_deref(),
            Some("https://en.cppreference.com/c/header/stdckdint")
        );
        assert_eq!(cppreference_header_url("stdio.h"), None);
        assert_eq!(cppreference_header_url("<../stdio.h>"), None);
    }
}
