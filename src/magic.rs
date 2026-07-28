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
use crate::i18n;
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
                .map_err(|_| i18n::text("edit-usage"))?;
            session
                .input(number)
                .map(str::to_string)
                .map(Some)
                .ok_or_else(|| i18n::text_with("edit-not-found", &[("number", number.to_string())]))
        }
        _ => Err(i18n::text("edit-usage")),
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
        return Err(i18n::text("unicode-missing-expression"));
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
        .map_err(|_| i18n::text("unicode-invalid-count"))?;
    if count > MAX_EXPLICIT_UNITS {
        return Err(i18n::text_with(
            "unicode-count-limit",
            &[("limit", MAX_EXPLICIT_UNITS.to_string())],
        ));
    }
    if expression.is_empty() {
        return Err(i18n::text("unicode-missing-after-count"));
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoInclusion {
    Always,
    WhenAvailable,
}

fn auto_inclusion(header: &str) -> Option<AutoInclusion> {
    let include = format!("#include {header}");
    codegen::HEADERS
        .lines()
        .any(|line| line.trim() == include)
        .then_some(if header == "<uchar.h>" {
            AutoInclusion::WhenAvailable
        } else {
            AutoInclusion::Always
        })
}

fn cppreference_header_url(header: &str) -> Option<String> {
    let stem = header.strip_prefix('<')?.strip_suffix(".h>")?;
    stem.bytes()
        .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
        .then(|| format!("https://en.cppreference.com/c/header/{stem}"))
}

fn terminal_width(text: &str) -> usize {
    text.chars()
        .map(|character| if character.is_ascii() { 1 } else { 2 })
        .sum()
}

fn pad_to_width(text: &str, width: usize) -> String {
    format!(
        "{text}{}",
        " ".repeat(width.saturating_sub(terminal_width(text)))
    )
}

fn render_where_header_table(headers: &[&str], ui: &Ui) {
    let header_label = ui.text("where-header-column");
    let documentation_label = ui.text("where-doc-column");

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
        .map(|(header, _)| terminal_width(header))
        .max()
        .unwrap_or(0)
        .max(terminal_width(&header_label));
    let documentation_width = rows
        .iter()
        .map(|(_, url)| terminal_width(url))
        .max()
        .unwrap_or(0)
        .max(terminal_width(&documentation_label));
    let border = format!(
        "+-{}-+-{}-+",
        "-".repeat(header_width),
        "-".repeat(documentation_width)
    );

    println!("{}", ui.text("where-headers"));
    println!("{border}");
    println!(
        "| {} | {} |",
        pad_to_width(&header_label, header_width),
        pad_to_width(&documentation_label, documentation_width)
    );
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
            " ".repeat(documentation_width - terminal_width(&url))
        );
    }
    println!("{border}");
}

fn kind_label(kind: std_index::Kind, ui: &Ui) -> String {
    use std_index::Kind;
    let key = match kind {
        Kind::Function => "kind-function",
        Kind::FunctionLikeMacro => "kind-function-like-macro",
        Kind::ObjectLikeMacro => "kind-object-like-macro",
        Kind::TypeGenericMacro => "kind-type-generic-macro",
        Kind::Typedef => "kind-typedef",
        Kind::Type => "kind-type",
    };
    ui.text(key)
}

fn localized_index_note(note: &str, ui: &Ui) -> String {
    let key = match note {
        "deprecated; removed from ISO C in C11 because it cannot perform bounded input" => {
            "index-note-gets"
        }
        "provided by <stdbool.h> through C17; a language keyword in C23" => "index-note-stdbool",
        "provided by <stdalign.h> through C17; a language keyword in C23" => "index-note-stdalign",
        "provided by <assert.h> in C11/C17; a language keyword in C23" => "index-note-assert",
        "obsolescent in C23" => "index-note-obsolescent",
        "still provided by <stdnoreturn.h> in C23; prefer [[noreturn]] in new C23 code" => {
            "index-note-noreturn"
        }
        "an implementation may additionally provide a macro form" => "index-note-macro-form",
        _ => return note.to_string(),
    };
    ui.text(key)
}

fn render_where(name: &str, ev: &Evaluator, ui: &Ui) {
    let Some(found) = std_index::lookup(name) else {
        let message = ui.text_with("where-not-found", &[("name", name.to_string())]);
        println!("{}", ui.err(&message));
        return;
    };

    println!("{} {}", ui.text("where-name"), found.name);
    let kinds = found
        .kinds()
        .into_iter()
        .map(|kind| kind_label(kind, ui))
        .collect::<Vec<_>>()
        .join(" / ");
    println!("{} {kinds}", ui.text("where-kind"));

    let headers = found.headers();
    render_where_header_table(&headers, ui);
    if let Some(signature) = found.signature {
        println!("{} {signature}", ui.text("where-signature"));
    }

    let since = found.since();
    if let Some(removed) = found.removed() {
        let last = removed
            .previous()
            .expect("no indexed identifier is removed in C89");
        println!(
            "{}",
            ui.text_with(
                "where-availability-range",
                &[
                    ("since", since.label().to_string()),
                    ("last", last.label().to_string()),
                    ("removed", removed.label().to_string()),
                ],
            )
        );
    } else {
        println!(
            "{}",
            ui.text_with(
                "where-availability-later",
                &[("since", since.label().to_string())],
            )
        );
    }

    let mode = if ev.tc.std.is_empty() {
        ev.tc.default_std.as_deref()
    } else {
        Some(ev.tc.std.as_str())
    };
    if let Some(mode) = mode {
        if let Some(standard) = std_index::CStandard::from_mode(mode) {
            let availability = if found.available_in(standard) {
                ui.text("where-available")
            } else {
                ui.text("where-unavailable")
            };
            println!("{} {mode} ({availability})", ui.text("where-selected-mode"));
        } else {
            println!("{} {mode}", ui.text("where-selected-mode"));
        }
    }

    let always = headers
        .iter()
        .copied()
        .filter(|header| auto_inclusion(header) == Some(AutoInclusion::Always))
        .collect::<Vec<_>>();
    let conditional = headers
        .iter()
        .copied()
        .filter(|header| auto_inclusion(header) == Some(AutoInclusion::WhenAvailable))
        .collect::<Vec<_>>();
    if always.is_empty() && conditional.is_empty() {
        println!("{}", ui.text("where-auto-no"));
    } else {
        if !always.is_empty() {
            println!("{} ({})", ui.text("where-auto-yes"), always.join(", "));
        }
        if !conditional.is_empty() {
            println!(
                "{} {}",
                ui.text("where-auto-conditional"),
                conditional.join(", ")
            );
        }
    }
    if let Some(note) = found.note {
        println!(
            "{} {}",
            ui.text("where-note"),
            localized_index_note(note, ui)
        );
    }
}

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
            [] => println!("{}", ui.text("magic-help")),
            [flag] if *flag == "--verbose" => println!(
                "{}

{}",
                ui.text("magic-help"),
                ui.text("magic-help-notes")
            ),
            _ => println!("{}", ui.err(&ui.text("help-usage"))),
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
            println!("{}", ui.dim(&ui.text("session-cleared")));
        }

        "header" | "headers" => {
            println!("{}", ui.dim(&ui.text("headers-intro")));
            print!("{}", codegen::HEADERS);
        }

        "src" => match rest.as_slice() {
            [] => println!("{}", format_c(&codegen::build_user_view(session))),
            [flag] if *flag == "--raw" => {
                let prog = codegen::build(session, "", Slot::Stmt);
                println!("{}", format_c(&prog.src));
            }
            _ => println!("{}", ui.err(&ui.text("src-usage"))),
        },

        "edit" => match edit_input(&rest, session) {
            Ok(Some(input)) => return Ok(Action::Prefill(input)),
            Ok(None) => println!("{}", ui.dim(&ui.text("nothing-to-edit"))),
            Err(message) => println!("{}", ui.err(&message)),
        },

        "type" => {
            if tail.is_empty() {
                println!("{}", ui.err(&ui.text("type-usage")));
            } else {
                render_probe(ev.type_of(session, tail)?, ui, &ui.text("type-no-result"));
            }
        }

        bits_command @ ("bits" | "Bits") => {
            let uppercase = bits_command == "Bits";
            if tail.is_empty() {
                let usage = ui.text_with("bits-usage", &[("command", bits_command.to_string())]);
                println!("{}", ui.err(&usage));
            } else {
                let result = ev.bits_of(session, tail, uppercase)?;
                match result {
                    Eval::CompileError(diag) if unsupported_bits_type(&diag) => {
                        let message = ui.text_with(
                            "bits-unsupported",
                            &[("command", bits_command.to_string())],
                        );
                        println!("{}", ui.err(&message));
                    }
                    result => render_probe(result, ui, &ui.text("bits-no-result")),
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
                Err(message) => {
                    let usage = ui.text_with(
                        "unicode-usage",
                        &[
                            ("command", unicode_command.to_string()),
                            ("message", message),
                        ],
                    );
                    println!("{}", ui.err(&usage));
                }
                Ok((count, expression)) => {
                    let result = ev.unicode_of(session, expression, encoding, count)?;
                    match result {
                        Eval::CompileError(diag) if unsupported_unicode_type(&diag) => {
                            let message = ui.text_with(
                                "unicode-unsupported",
                                &[("command", unicode_command.to_string())],
                            );
                            println!("{}", ui.err(&message));
                        }
                        result => {
                            render_probe(result, ui, &ui.text("unicode-no-result"));
                        }
                    }
                }
            }
        }

        "where" => match rest.as_slice() {
            [name] if is_c_identifier(name) => render_where(name, ev, ui),
            _ => println!("{}", ui.err(&ui.text("where-usage"))),
        },

        "time" => {
            if tail.is_empty() {
                println!("{}", ui.err(&ui.text("time-usage")));
            } else {
                let n = session.counter + 1;
                session.counter = n;
                session.remember_input(n, tail);

                match ev.time(session, tail)? {
                    Eval::CompileError(diag) => println!("{}", ui.err(diag.trim_end())),
                    Eval::Done(o) => {
                        if o.streamed_output_needs_newline {
                            println!("{}", ui.inserted_newline_marker());
                        }
                        let note = |s: &str| println!("{}", ui.dim(s));
                        if o.rewritten.is_some() {
                            note(&ui.text("note-missing-semicolon"));
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
                                note(&ui.text("note-input-not-kept"));
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
                        if let Some(duration) = o.timed_duration {
                            let label = ui.text("wall-time");
                            println!(
                                "{}",
                                ui.dim(&format!("{label} {}", format_duration(duration)))
                            );
                        } else {
                            println!("{}", ui.dim(&ui.text("wall-time-unavailable")));
                        }
                    }
                }
            }
        }

        "timeit" => {
            if tail.is_empty() {
                println!("{}", ui.err(&ui.text("timeit-usage")));
            } else {
                if session.may_have_side_effects(tail) {
                    println!("{}", ui.warn(&ui.text("timeit-state-warning")));
                }
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
                        ui.err(
                            &ui.text_with("std-unsupported", &[("standard", want.to_string())],)
                        )
                    ),
                }
            }
        }

        other => {
            let message = ui.text_with("unknown-command", &[("command", other.to_string())]);
            println!("{}", ui.err(&message));
        }
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

    #[test]
    fn reports_uchar_as_a_conditional_default_header() {
        assert_eq!(
            auto_inclusion("<uchar.h>"),
            Some(AutoInclusion::WhenAvailable)
        );
        assert_eq!(auto_inclusion("<wchar.h>"), Some(AutoInclusion::Always));
        assert_eq!(auto_inclusion("<tgmath.h>"), None);
    }
}
