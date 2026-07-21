//! `%` commands, in the spirit of IPython's magics.
//!
//! A line starting with `%` is never valid C, so the prefix needs no escaping
//! rule to stay unambiguous.

use anyhow::Result;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::codegen::{self, Slot};
use crate::eval::Evaluator;
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
        let mut child = Command::new(path)
            // Match the generated code's own 4-space style.
            .arg("-style={BasedOnStyle: LLVM, IndentWidth: 4}")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        child
            .stdin
            .take()
            .expect("stdin piped")
            .write_all(src.as_bytes())?;
        let out = child.wait_with_output()?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(std::io::Error::other("clang-format failed"))
        }
    };
    run().unwrap_or_else(|_| src.to_string())
}

const HELP: &str = "\
Commands:
  %help              show this help
  %quit / %exit      quit (Ctrl-D works too)
  %reset             clear the session and start fresh
  %history           show everything entered this session
  %src               show the full C program the session assembles
  %undo              drop the most recently accepted input
  %cc [path]         show or switch the C compiler
  %std [std]         show or switch the language standard (c11/c17/c23);
                     %std default returns to the compiler's own default
  %flags [flags...]  show or set extra compiler flags (no args clears them)

Notes:
  A bare expression prints its value; a trailing ';' runs it silently.
  Function definitions, #include and typedef go to file scope automatically.
  Pure expressions (x + 1, sizeof(int)) are evaluated and forgotten; only
  inputs that may change state (assignment, ++/--, calls) are kept.
  Every evaluation re-runs the whole session, so input-reading statements
  like scanf will execute again.";

pub fn handle(line: &str, session: &mut Session, ev: &mut Evaluator, ui: &Ui) -> Result<Action> {
    let body = line.trim().trim_start_matches('%');
    let mut parts = body.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();

    match cmd {
        "help" | "h" | "?" => println!("{HELP}"),
        "quit" | "exit" | "q" => return Ok(Action::Quit),

        "reset" => {
            session.reset();
            println!("{}", ui.dim("session cleared"));
        }

        "history" => {
            if session.history.is_empty() {
                println!("{}", ui.dim("(no input yet)"));
            }
            for (i, h) in session.history.iter().enumerate() {
                println!("{} {h}", ui.dim(&format!("{:>4}:", i + 1)));
            }
        }

        "src" => {
            let prog = codegen::build(session, "", Slot::Stmt);
            println!("{}", format_c(&prog.src));
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
                let extra = ev.tc.extra.clone();
                let std_opt = if std.is_empty() {
                    None
                } else {
                    Some(std.as_str())
                };
                match crate::toolchain::Toolchain::detect(Some(rest[0]), std_opt, extra) {
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
                match crate::toolchain::Toolchain::detect(Some(&path), req, ev.tc.extra.clone()) {
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

        "flags" => {
            let mut tc = ev.tc.clone();
            tc.extra = rest.iter().map(|s| s.to_string()).collect();
            ev.set_toolchain(tc);
            if ev.tc.extra.is_empty() {
                println!("{}", ui.dim("(no extra flags)"));
            } else {
                println!("{}", ev.tc.extra.join(" "));
            }
        }

        other => println!(
            "{}",
            ui.err(&format!("unknown command %{other} — try %help"))
        ),
    }
    Ok(Action::Continue)
}
