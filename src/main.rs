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
mod session;
mod toolchain;
mod ui;

use anyhow::Result;
use clap::Parser;
use rustyline::error::ReadlineError;
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

    /// Extra flags passed through to the compiler untouched
    #[arg(long, value_name = "FLAG", num_args = 1..)]
    flags: Vec<String>,

    /// Seconds a program may run before it is killed
    #[arg(long, value_name = "SECS", default_value_t = 10)]
    timeout: u64,

    /// Disable colored output
    #[arg(long)]
    no_color: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    // Respect the de facto standard opt-out as well as our own flag.
    let color = !args.no_color && std::env::var_os("NO_COLOR").is_none();
    let ui = Ui { color };

    let tc = toolchain::Toolchain::detect(args.cc.as_deref(), args.std.as_deref(), args.flags)?;
    let mut ev = Evaluator::new(tc, Duration::from_secs(args.timeout))?;

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

    let mut rl = rustyline::Editor::<editor::CHelper, _>::new()?;
    rl.set_helper(Some(editor::CHelper::new(color)));

    let mut session = Session::default();

    loop {
        let n = session.counter + 1;
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
        session.history.push(raw.to_string());

        if raw.starts_with('%') {
            match magic::handle(raw, &mut session, &mut ev, &ui)? {
                magic::Action::Quit => break,
                magic::Action::Continue => continue,
            }
        }

        let text = raw.to_string();
        session.counter = n;
        match ev.eval(&session, &text)? {
            Eval::CompileError(diag) => {
                println!("{}", ui.err(diag.trim_end()));
            }
            Eval::Done(o) => {
                if o.rewritten.is_some() {
                    println!("{}", ui.dim("(missing semicolon added automatically)"));
                }
                if !o.warnings.trim().is_empty() {
                    println!("{}", ui.warn(o.warnings.trim_end()));
                }
                print!("{}", o.output);
                // Programs rarely end their output with a newline, and without
                // this the next prompt would be glued onto the last line.
                if !o.output.is_empty() && !o.output.ends_with('\n') {
                    println!();
                }
                if !o.errors.is_empty() {
                    print!("{}", ui.err(&o.errors));
                }
                if let Some(v) = &o.value {
                    println!("{}{}", ui.out_label(n), v);
                }
                let committed = o.rewritten.clone().unwrap_or_else(|| text.clone());
                match &o.abnormal {
                    // A crashing input is not kept: every later evaluation
                    // replays the whole session, so committing it would make
                    // the prompt permanently unusable.
                    Some(msg) => {
                        println!("{}", ui.err(msg));
                        println!("{}", ui.dim("(input not kept in the session)"));
                    }
                    None => session.commit(&committed, o.slot),
                }
                if o.slot == Slot::FileScope && o.abnormal.is_none() {
                    println!("{}", ui.dim("(added at file scope)"));
                }
            }
        }
    }

    println!("{}", ui.dim("bye"));
    Ok(())
}
