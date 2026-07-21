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
use rustyline::error::ReadlineError;
use rustyline::{EventHandler, KeyCode, KeyEvent, Modifiers};
use std::io::{BufRead, IsTerminal};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::codegen::Slot;
use crate::eval::{Eval, Evaluator};
use crate::session::{HistoryEntry, Session};
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

    /// Seconds a program may run before it is killed
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
    let mut session = Session::default();

    // ---- non-interactive modes -------------------------------------------

    if !args.eval.is_empty() {
        let mut ok = true;
        for code in &args.eval {
            let (input_ok, quit) = submit(code, &mut session, &mut ev, &ui, Style::Bare)?;
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

    let mut rl = rustyline::Editor::<editor::CHelper, _>::with_config(
        rustyline::Config::builder()
            .max_history_size(1000)?
            .history_ignore_dups(true)?
            .build(),
    )?;
    // Completion vocabulary, refreshed after every input.
    let idents = Arc::new(std::sync::Mutex::new(Vec::new()));
    rl.set_helper(Some(editor::CHelper::new(color, Arc::clone(&idents))));

    let hist_path = history_path();
    if let Some(p) = &hist_path {
        let _ = rl.load_history(p); // absent on first run; fine
    }
    // Enter auto-indents continuation lines: padded to the prompt width so
    // code aligns under code, plus two spaces per open bracket. `}` on a
    // blank indent steps one level back out.
    let prompt_base = Arc::new(AtomicUsize::new(Ui::prompt_width(1)));
    rl.bind_sequence(
        KeyEvent(KeyCode::Enter, Modifiers::NONE),
        EventHandler::Conditional(Box::new(editor::EnterIndents(prompt_base.clone()))),
    );
    rl.bind_sequence(
        KeyEvent::from('}'),
        EventHandler::Conditional(Box::new(editor::BraceDedents)),
    );

    loop {
        let n = session.counter + 1;
        prompt_base.store(Ui::prompt_width(n), Ordering::Relaxed);
        let line = match rl.readline(&ui.prompt_in(n)) {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        let raw = line.trim();
        if raw.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(raw);
        let (_, quit) = submit(raw, &mut session, &mut ev, &ui, Style::Repl)?;
        *idents.lock().expect("ident vocabulary") = session.identifiers();
        if quit {
            break;
        }
    }

    if let Some(p) = &hist_path {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = rl.save_history(p); // best effort; losing history is not fatal
    }
    println!("{}", ui.dim("bye"));
    Ok(())
}

/// Where the persistent readline history lives:
/// `$XDG_DATA_HOME`/`~/.local/share` on Unix, `%APPDATA%` on Windows,
/// each with `c-shell/history` appended.
fn history_path() -> Option<std::path::PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(std::path::PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|h| std::path::PathBuf::from(h).join(".local").join("share"))
            })
    }?;
    Some(base.join("c-shell").join("history"))
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
        if pending.is_empty() && line.trim().is_empty() {
            continue;
        }
        if !pending.is_empty() {
            pending.push('\n');
        }
        pending.push_str(&line);
        if editor::is_incomplete(&pending) {
            continue;
        }
        let input = std::mem::take(&mut pending);
        let (ok, quit) = submit(input.trim(), session, ev, ui, Style::Repl)?;
        all_ok &= ok;
        if quit {
            return Ok(all_ok);
        }
    }
    // An unterminated tail still gets evaluated: the compile error it earns
    // is the honest report of a truncated script.
    if !pending.trim().is_empty() {
        let (ok, _) = submit(pending.trim(), session, ev, ui, Style::Repl)?;
        all_ok &= ok;
    }
    Ok(all_ok)
}

/// Run one complete input: magic command or C code. Returns (succeeded,
/// quit-requested).
fn submit(
    raw: &str,
    session: &mut Session,
    ev: &mut Evaluator,
    ui: &Ui,
    style: Style,
) -> Result<(bool, bool)> {
    let raw = raw.trim();
    // Comment-only input is legal to type but has nothing to evaluate.
    if raw.is_empty() || lex::is_blank(raw) {
        return Ok((true, false));
    }
    if raw.starts_with('%') {
        session.history.push(HistoryEntry {
            n: None,
            text: raw.to_string(),
        });
        return match magic::handle(raw, session, ev, ui)? {
            magic::Action::Quit => Ok((true, true)),
            magic::Action::Continue => Ok((true, false)),
        };
    }

    let bare = style == Style::Bare;
    let n = session.counter + 1;
    session.counter = n;
    session.history.push(HistoryEntry {
        n: Some(n),
        text: raw.to_string(),
    });
    match ev.eval(session, raw)? {
        Eval::CompileError(diag) => {
            let msg = ui.err(diag.trim_end());
            if bare {
                eprintln!("{msg}");
            } else {
                println!("{msg}");
            }
            Ok((false, false))
        }
        Eval::Done(o) => {
            let note = |s: &str| {
                if !bare {
                    println!("{}", ui.dim(s));
                }
            };
            if o.rewritten.is_some() {
                note("(missing semicolon added automatically)");
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
                None => session.commit(&committed, o.slot),
            }
            if o.slot == Slot::FileScope && o.abnormal.is_none() {
                note("(added at file scope)");
            }
            if o.demoted && o.abnormal.is_none() {
                note("(note: kept inside main — it did not compile at file scope)");
            }
            Ok((o.abnormal.is_none(), false))
        }
    }
}
