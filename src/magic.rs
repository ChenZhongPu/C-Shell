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
}

/// Pretty-print C source through clang-format when it is available, or
/// return it unchanged when it is not.
///
/// The assembled program is honest but ragged: it is built by string
/// concatenation, and interactively-typed inputs carry the prompt-width
/// padding the auto-indent inserted. Formatting is presentation only — the
/// compiler always receives the raw text, so `%src` shows the same program,
/// just readable.
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

const HELP: &str = "\
Commands:
  %help              show this help
  %quit / %exit      quit (Ctrl-D works too)
  %clear             clear the screen without changing the session
  %reset             clear the session and start fresh
  %history           show everything entered this session
  %src               show the full C program the session assembles
  %type <expression> query an expression's type without evaluating it
  %undo              drop the most recently accepted input
  %cc [path]         show or switch the C compiler
  %std [std]         show or switch the language standard (c11/c17/c23);
                     %std default returns to the compiler's own default

Notes:
  A bare expression prints its value; a trailing ';' runs it silently.
  A completed if waits for a blank continuation line; type else / else if
  there instead to continue it. Other closed blocks submit immediately.
  Function definitions, #include and typedef go to file scope automatically.
  %type uses _Generic matching: scalar types and scalar pointers are named;
  complete named structs/unions report e.g. Struct Point or Union Value;
  simple anonymous typedefs use the typedef name. Other aliases and top-level
  qualifiers are canonicalized, and arrays/functions undergo their normal
  expression conversions.
  Pure bare expressions (x + 1, sizeof(int)) are evaluated and forgotten.
  Statements and bare expressions that may have effects are kept.
  Every evaluation re-runs the whole session, so input-reading statements
  like scanf will execute again.";

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

        "history" => {
            if session.history.is_empty() {
                println!("{}", ui.dim("(no input yet)"));
            }
            // Two sequences, kept visibly distinct: evaluated inputs carry
            // their In[n] number, magic commands a dash — they never
            // consumed one.
            for e in &session.history {
                let tag = match e.n {
                    Some(n) => format!("In[{n:>3}]"),
                    None => "  --   ".to_string(),
                };
                println!("{} {}", ui.dim(&tag), e.text);
            }
        }

        "src" => {
            let prog = codegen::build(session, "", Slot::Stmt);
            println!("{}", format_c(&prog.src));
        }

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
