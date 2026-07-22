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
use crate::eval::{Eval, Evaluator};
use crate::proc;
use crate::session::Session;
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

const HELP: &str = "\
Commands:
  %help              show this help
  %quit / %exit      quit (Ctrl-D works too)
  %clear             clear the screen without changing the session
  %reset             clear the session and start fresh
  %src [--raw]       show user C; --raw includes generated runtime/protocol
  %edit [n]          copy latest or In[n] into the prompt for editing
  %type <expression> query an expression's type without evaluating it
  %undo              undo the most recent retained state change
  %cc [path]         show or switch the C compiler
  %std [std]         show or switch the language standard (c11/c17/c23);
                     %std default returns to the compiler's own default

Notes:
  A bare expression prints its value; a trailing ';' runs it silently.
  A completed if waits for a blank continuation line; type else / else if
  there instead to continue it. Other closed blocks submit immediately.
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
  Struct values use designated members; nested known structs and arrays
  expand, but pointer members are shown only as addresses or NULL. Use an
  explicit member expression (p.name) or dereference (*ptr) to drill down.
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
        "help" | "h" | "?" => println!("{HELP}"),
        "quit" | "exit" | "q" => return Ok(Action::Quit),

        "clear" => {
            // ANSI erase-display + cursor-home. This is terminal control, not
            // color styling, so --no-color deliberately does not disable it.
            print!("\x1b[2J\x1b[H");
            let _ = std::io::stdout().flush();
        }

        "reset" => {
            session.reset();
            println!("{}", ui.dim("session cleared"));
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
                match ev.type_of(session, tail)? {
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
                        } else if let Some(name) = o.value {
                            println!("{name}");
                        } else {
                            println!("{}", ui.err("type query produced no result"));
                        }
                    }
                }
            }
        }

        "undo" => match session.undo() {
            Some(text) => println!("{} {text}", ui.dim("undone:")),
            None => println!("{}", ui.dim("nothing to undo")),
        },

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
}
