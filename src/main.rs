//! c-shell — an interactive prompt for C.
//!
//! Type an expression to see its value, a statement to run it, a function to
//! define it. Everything is compiled by a real C compiler, because the point
//! is to find out what *your* compiler does, not what an interpreter would.

mod codegen;
mod editor;
mod errmap;
mod eval;
mod lex;
mod magic;
mod proc;
mod session;
mod toolchain;
mod ui;

use anyhow::{Context, Result};
use clap::Parser;
use reedline::{
    EditCommand, Emacs, IdeMenu, KeyCode, KeyModifiers, MenuBuilder, Reedline, ReedlineEvent,
    ReedlineMenu, Signal, default_emacs_keybindings,
};
use std::io::{BufRead, IsTerminal};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::codegen::Slot;
use crate::eval::{Eval, Evaluator};
use crate::session::Session;
use crate::ui::Ui;

#[derive(Parser)]
#[command(name = "c-shell", version, about = "An interactive prompt for C")]
struct Args {
    /// C compiler to use (default: $CC, then the first one found on PATH)
    #[arg(long, value_name = "PATH")]
    cc: Option<String>,

    /// Language standard, e.g. c11, c17, c23 (default: whatever the
    /// compiler itself defaults to, e.g. gnu23 for current gcc)
    #[arg(long, value_name = "STD")]
    std: Option<String>,

    /// Evaluate CODE and exit (repeatable). Values print bare, diagnostics
    /// go to stderr, and a failing input sets a nonzero exit code
    #[arg(
        short = 'e',
        long = "eval",
        value_name = "CODE",
        conflicts_with = "script"
    )]
    eval: Vec<String>,

    /// Run inputs from FILE and exit; nonzero exit code on any failure
    #[arg(long, value_name = "FILE")]
    script: Option<std::path::PathBuf>,

    /// Suppress the banner and hints
    #[arg(short = 'q', long)]
    quiet: bool,

    /// Seconds each compilation or program run may take
    #[arg(long, value_name = "SECS", default_value_t = 10)]
    timeout: u64,

    /// Disable colored output
    #[arg(long)]
    no_color: bool,
}

/// How results are rendered.
#[derive(Clone, Copy, PartialEq)]
enum Style {
    /// `Out[n]:` labels, notes, everything on stdout — a transcript.
    Repl,
    /// Bare values on stdout, diagnostics on stderr, no notes — for `-e`,
    /// where the output is probably feeding a pipe.
    Bare,
}

fn main() -> Result<()> {
    let args = Args::parse();
    // Auto-detect: no colors when redirected, explicitly opted out, or on a
    // terminal that declared itself unable (TERM=dumb).
    let color = !args.no_color
        && std::env::var_os("NO_COLOR").is_none()
        && std::io::stdout().is_terminal()
        && std::env::var("TERM").map_or(true, |t| t != "dumb");
    let ui = Ui { color };

    let tc = toolchain::Toolchain::detect(args.cc.as_deref(), args.std.as_deref())?;
    let mut ev = Evaluator::new(tc, Duration::from_secs(args.timeout))?;
    // At a real terminal, forward only the newest input's output as it arrives
    // so prompts before scanf are visible. Batch transcripts remain buffered.
    let stdin_is_terminal = std::io::stdin().is_terminal();
    ev.set_stream_output(stdin_is_terminal);
    // Piped REPL mode consumes stdin as C source. A TTY, `-e`, or a script
    // leaves stdin available as the evaluated program's input.
    ev.set_program_stdin(stdin_is_terminal || !args.eval.is_empty() || args.script.is_some());
    let mut session = Session::default();

    // ---- non-interactive modes -------------------------------------------

    if !args.eval.is_empty() {
        let mut ok = true;
        for code in &args.eval {
            let (input_ok, quit, _) = submit(code, &mut session, &mut ev, &ui, Style::Bare, false)?;
            ok &= input_ok;
            if quit {
                break;
            }
        }
        std::process::exit(if ok { 0 } else { 1 });
    }

    if let Some(path) = &args.script {
        let file = std::fs::File::open(path)
            .with_context(|| format!("cannot open script {}", path.display()))?;
        let ok = run_batch(std::io::BufReader::new(file), &mut session, &mut ev, &ui)?;
        std::process::exit(if ok { 0 } else { 1 });
    }

    if !std::io::stdin().is_terminal() {
        // Piped input: a transcript without prompts or banner. Tolerant exit
        // code, like the interactive REPL it substitutes for.
        let stdin = std::io::stdin();
        run_batch(stdin.lock(), &mut session, &mut ev, &ui)?;
        return Ok(());
    }

    // ---- interactive REPL ------------------------------------------------

    if !args.quiet && std::io::stdout().is_terminal() {
        print!("{}", ui.banner());
        println!(
            "{} {}  ·  {}",
            ui.bold("c-shell"),
            env!("CARGO_PKG_VERSION"),
            ev.tc.describe()
        );
        println!(
            "{}",
            ui.dim("Type C code to evaluate it · %help for commands · Ctrl-D to exit")
        );
    }

    // Completion vocabulary, refreshed after every input and shared with the
    // completer.
    let idents = Arc::new(Mutex::new(Vec::new()));

    // Shared editor state: the highlighter stores the current buffer and cursor
    // on every repaint, and the edit mode reads them to calculate indentation.
    let edit_state = Arc::new(Mutex::new(editor::EditState::default()));
    let completion_active = Arc::new(AtomicBool::new(false));

    // Tab opens the IPython-style dropdown, then steps through it. reedline
    // auto-saves submitted lines to an in-memory history (capacity 1000, no
    // file), and the validator drives multi-line continuation.
    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    let edit_mode = editor::CEditMode::new(
        Emacs::new(keybindings),
        Arc::clone(&edit_state),
        Arc::clone(&completion_active),
    );
    let menu = editor::TrackingMenu::new(
        IdeMenu::default().with_name("completion_menu"),
        completion_active,
    );
    let mut rl = Reedline::create()
        .with_completer(Box::new(editor::CCompleter::new(Arc::clone(&idents))))
        .with_highlighter(Box::new(editor::CHighlighter::new(
            color,
            Arc::clone(&edit_state),
        )))
        .with_validator(Box::new(editor::CValidator))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(Box::new(edit_mode))
        .with_history(Box::new(reedline::FileBackedHistory::new(1000)?))
        .with_ansi_colors(color);

    let mut edit_buffer: Option<String> = None;
    loop {
        let n = session.counter + 1;
        let prompt = editor::CPrompt { n };
        // `%edit` preloads the buffer: reedline clears it on submit, so the
        // insert lands in an empty buffer that read_line then edits in place.
        if let Some(initial) = edit_buffer.take() {
            rl.run_edit_commands(&[EditCommand::InsertString(initial)]);
        }
        let line = match rl.read_line(&prompt)? {
            Signal::Success(l) => l,
            Signal::CtrlC => continue,
            Signal::CtrlD => break,
            _ => continue,
        };
        let raw = line.trim();
        if raw.is_empty() {
            continue;
        }
        let (_, quit, next_edit) = submit(raw, &mut session, &mut ev, &ui, Style::Repl, true)?;
        // `%edit` itself consumes no input number. Its selected text becomes
        // the editable buffer at the same prompt on the next read_line call.
        edit_buffer = next_edit;
        *idents.lock().expect("ident vocabulary") = session.identifiers();
        if quit {
            break;
        }
    }

    println!("{}", ui.dim("bye"));
    Ok(())
}

/// Feed complete inputs from a reader, accumulating lines until the input is
/// syntactically finished — a function definition in a script spans many
/// lines. Returns whether every input succeeded.
fn run_batch<R: BufRead>(
    reader: R,
    session: &mut Session,
    ev: &mut Evaluator,
    ui: &Ui,
) -> Result<bool> {
    let mut all_ok = true;
    let mut pending = String::new();
    for line in reader.lines() {
        let line = line?;

        // A balanced `if` needs one physical line of lookahead: `else` and
        // intervening comment-only lines still belong to it. Any other line
        // confirms the if and starts a fresh REPL input.
        if !pending.is_empty() && editor::can_accept_else(&pending) {
            let comment = !line.trim().is_empty() && lex::is_blank(&line);
            if !editor::starts_with_else(&line) && !comment {
                let (ok, quit, _) = submit_batch_pending(&mut pending, session, ev, ui)?;
                all_ok &= ok;
                if quit {
                    return Ok(all_ok);
                }
            }
        }

        if pending.is_empty() && line.trim().is_empty() {
            continue;
        }
        if !pending.is_empty() {
            pending.push('\n');
        }
        pending.push_str(&line);
        if editor::is_structurally_incomplete(&pending) || editor::can_accept_else(&pending) {
            continue;
        }
        let (ok, quit, _) = submit_batch_pending(&mut pending, session, ev, ui)?;
        all_ok &= ok;
        if quit {
            return Ok(all_ok);
        }
    }
    // An unterminated or lookahead-delayed tail still gets evaluated: a
    // truncated construct earns the compiler's honest diagnostic.
    if !pending.trim().is_empty() {
        let (ok, _, _) = submit_batch_pending(&mut pending, session, ev, ui)?;
        all_ok &= ok;
    }
    Ok(all_ok)
}

fn submit_batch_pending(
    pending: &mut String,
    session: &mut Session,
    ev: &mut Evaluator,
    ui: &Ui,
) -> Result<(bool, bool, Option<String>)> {
    let input = std::mem::take(pending);
    submit(input.trim(), session, ev, ui, Style::Repl, false)
}

/// Run one complete input: magic command or C code. Returns (succeeded,
/// quit-requested, next interactive edit buffer).
fn submit(
    raw: &str,
    session: &mut Session,
    ev: &mut Evaluator,
    ui: &Ui,
    style: Style,
    interactive_edit: bool,
) -> Result<(bool, bool, Option<String>)> {
    let raw = raw.trim();
    // Comment-only input is legal to type but has nothing to evaluate.
    if raw.is_empty() || lex::is_blank(raw) {
        return Ok((true, false, None));
    }
    if raw.starts_with('%') {
        return match magic::handle(raw, session, ev, ui)? {
            magic::Action::Quit => Ok((true, true, None)),
            magic::Action::Continue => Ok((true, false, None)),
            magic::Action::Prefill(input) if interactive_edit => Ok((true, false, Some(input))),
            magic::Action::Prefill(_) => {
                let message = ui.err("%edit is available only in interactive REPL mode");
                if style == Style::Bare {
                    eprintln!("{message}");
                } else {
                    println!("{message}");
                }
                Ok((false, false, None))
            }
        };
    }

    let bare = style == Style::Bare;
    let n = session.counter + 1;
    session.counter = n;
    session.remember_input(n, raw);

    let external_calls = lex::external_replay_calls(raw)
        .into_iter()
        .filter(|name| name != "scanf")
        .collect::<Vec<_>>();
    let external_warning = !external_calls.is_empty() && !session.external_replay_warning_shown();
    if external_warning {
        let calls = external_calls
            .iter()
            .map(|name| format!("{name}()"))
            .collect::<Vec<_>>()
            .join(", ");
        let msg = ui.warn(&format!(
            "warning: external side-effect call detected ({calls}); retained inputs are replayed before every later evaluation, so input operations and file or process effects may happen repeatedly"
        ));
        if bare {
            eprintln!("{msg}");
        } else {
            println!("{msg}");
        }
    }

    match ev.eval(session, raw)? {
        Eval::CompileError(diag) => {
            let msg = ui.err(diag.trim_end());
            if bare {
                eprintln!("{msg}");
            } else {
                println!("{msg}");
            }
            Ok((false, false, None))
        }
        Eval::Done(o) => {
            // Live output has already been forwarded by Evaluator. If the C
            // program left a partial line, visibly mark the newline c-shell
            // inserts to protect the following warning, value or prompt.
            if o.streamed_output_needs_newline {
                println!("{}", ui.inserted_newline_marker());
            }
            let note = |s: &str| {
                if !bare {
                    println!("{}", ui.dim(s));
                }
            };
            if o.rewritten.is_some() {
                note("(missing semicolon added automatically)");
            }
            if o.unprintable {
                let msg = "valid expression, but this value category has no printer; evaluated without Out[n]";
                if bare {
                    eprintln!("{msg}");
                } else {
                    note(msg);
                }
            }
            if !o.warnings.trim().is_empty() {
                let msg = ui.warn(o.warnings.trim_end());
                if bare {
                    eprintln!("{msg}");
                } else {
                    println!("{msg}");
                }
            }
            print!("{}", o.output);
            // Programs rarely end their output with a newline, and without
            // this the next prompt would be glued onto the last line.
            if !o.output.is_empty() && !o.output.ends_with('\n') {
                println!();
            }
            if !o.errors.is_empty() {
                if bare {
                    eprint!("{}", o.errors);
                } else {
                    print!("{}", ui.err(&o.errors));
                }
            }
            if let Some(v) = &o.value {
                if bare {
                    println!("{v}");
                } else {
                    println!("{}{}", ui.out_label(n), v);
                }
            }
            let committed = o.rewritten.clone().unwrap_or_else(|| raw.to_string());
            match &o.abnormal {
                // A crashing input is not kept: every later evaluation
                // replays the whole session, so committing it would make
                // the prompt permanently unusable.
                Some(msg) => {
                    if bare {
                        eprintln!("{msg}");
                    } else {
                        println!("{}", ui.err(msg));
                    }
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
                    if external_warning {
                        session.mark_external_replay_warning_shown();
                    }
                    session.attach_stdin_events(o.stdin_events.clone());
                }
            }
            if o.abnormal.is_none() {
                if o.file_replacement.is_some() {
                    note("(replaced previous file-scope definition)");
                } else if o.slot == Slot::FileScope {
                    note("(added at file scope)");
                }
                if o.scoped_rebind {
                    note("(opened a nested scope to shadow an earlier declaration)");
                }
                if !o.stdin_events.is_empty() {
                    note(&format!(
                        "(captured {} stdin request(s) for replay; contents hidden)",
                        o.stdin_events.len()
                    ));
                }
            }
            Ok((o.abnormal.is_none(), false, None))
        }
    }
}
