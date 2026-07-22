//! `%` commands, in the spirit of IPython's magics.
//!
//! A line starting with `%` is never valid C, so the prefix needs no escaping
//! rule to stay unambiguous.

use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Write as _};
use std::path::Path;
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
    Submit(String),
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

enum EditResult {
    Submit(String),
    Unchanged,
    Empty,
}

fn edit_text(original: &str) -> Result<EditResult> {
    edit_text_with(original, launch_editor)
}

fn edit_text_with(
    original: &str,
    launch: impl FnOnce(&Path) -> Result<bool>,
) -> Result<EditResult> {
    let path = tempfile::Builder::new()
        .prefix("c-shell-edit-")
        .suffix(".c")
        .tempfile()
        .context("cannot create editor temporary file")?
        .into_temp_path();
    // Drop the open file handle before launching GUI editors; Windows editors
    // otherwise vary in whether they can replace the temporary file.
    std::fs::write(&path, original).context("cannot prepare editor temporary file")?;
    if !launch(&path)? {
        bail!("editor exited unsuccessfully");
    }
    let edited = std::fs::read_to_string(&path).context("cannot read edited input")?;
    let edited = edited.trim();
    if edited.is_empty() {
        return Ok(EditResult::Empty);
    }
    if edited == original.trim() {
        return Ok(EditResult::Unchanged);
    }
    Ok(EditResult::Submit(edited.to_string()))
}

fn launch_editor(path: &Path) -> Result<bool> {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".to_string()
            } else {
                "vi".to_string()
            }
        });

    #[cfg(windows)]
    let status = Command::new("cmd")
        .arg("/C")
        .arg(format!("{editor} \"{}\"", path.display()))
        .status();
    #[cfg(not(windows))]
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("c-shell-edit")
        .arg(path)
        .status();

    Ok(status.context("cannot launch $VISUAL/$EDITOR")?.success())
}

const HELP: &str = "\
Commands:
  %help              show this help
  %quit / %exit      quit (Ctrl-D works too)
  %clear             clear the screen without changing the session
  %reset             clear the session and start fresh
  %src [--raw]       show user C; --raw includes generated runtime/protocol
  %edit [n]          edit latest or In[n] in $VISUAL/$EDITOR, then submit
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
  saved text is submitted under a new number and the original is unchanged.
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
  Every evaluation re-runs the whole session. Known file/input/process APIs
  trigger a one-time warning because their external effects may repeat.";

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

        "edit" => {
            let original = match rest.as_slice() {
                [] => match session.last_input() {
                    Some(input) => input.to_string(),
                    None => {
                        println!("{}", ui.dim("nothing to edit"));
                        return Ok(Action::Continue);
                    }
                },
                [number] => {
                    let Ok(number) = number.parse::<usize>() else {
                        println!("{}", ui.err("usage: %edit [input-number]"));
                        return Ok(Action::Continue);
                    };
                    let Some(input) = session.input(number) else {
                        println!("{}", ui.err(&format!("no C input In[{number}]")));
                        return Ok(Action::Continue);
                    };
                    input.to_string()
                }
                _ => {
                    println!("{}", ui.err("usage: %edit [input-number]"));
                    return Ok(Action::Continue);
                }
            };

            if !std::io::stdin().is_terminal() {
                println!("{}", ui.err("%edit requires an interactive terminal"));
                return Ok(Action::Continue);
            }
            match edit_text(&original) {
                Ok(EditResult::Submit(edited)) => return Ok(Action::Submit(edited)),
                Ok(EditResult::Unchanged) => println!("{}", ui.dim("edit cancelled: unchanged")),
                Ok(EditResult::Empty) => println!("{}", ui.dim("edit cancelled: empty input")),
                Err(e) => println!("{}", ui.err(&format!("edit failed: {e}"))),
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_file_changes_are_returned_and_unchanged_edits_cancel() {
        let changed = edit_text_with("int value = 1;", |path| {
            std::fs::write(path, "int value = 2;\n")?;
            Ok(true)
        })
        .expect("edited text");
        assert!(matches!(
            changed,
            EditResult::Submit(ref text) if text == "int value = 2;"
        ));

        let unchanged = edit_text_with("int value = 1;", |_| Ok(true)).expect("unchanged edit");
        assert!(matches!(unchanged, EditResult::Unchanged));
    }
}
