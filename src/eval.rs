//! Compiling and running one input.
//!
//! A lexical heuristic identifies file-scope-shaped input; the compiler then
//! validates that choice and arbitrates expression versus statement.

use anyhow::{Context, Result};
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use crate::codegen::{
    self, M_DONE, M_NEW, M_STDIN, M_UNICODE, M_UTF8, M_VAL, MAX_ARRAY_DEPTH, Slot, UNPRINTABLE,
};
use crate::errmap;
use crate::lex;
use crate::proc;
use crate::session::Session;

pub struct Outcome {
    pub slot: Slot,
    /// What the new input printed itself.
    pub output: String,
    /// The `Out[n]` value, when the input was an expression.
    pub value: Option<String>,
    /// Anything the new input wrote to stderr.
    pub errors: String,
    /// Live program output already reached the terminal but did not end in
    /// `\n`; the renderer must separate subsequent diagnostics/value/prompt.
    pub streamed_output_needs_newline: bool,
    /// Fresh stdin request lines captured while executing the newest input.
    pub stdin_events: Vec<proc::StdinEvent>,
    /// Compiler warnings, already remapped to input-relative lines.
    pub warnings: String,
    /// Set when the program died abnormally or ran too long.
    pub abnormal: Option<String>,
    /// Set when the input had to be repaired to compile; this is what should
    /// be committed to the session instead of what was typed.
    pub rewritten: Option<String>,
    /// The normal block-scope assembly failed, but entering a nested block
    /// made the compiler accept this declaration as a shadowing rebind.
    pub scoped_rebind: bool,
    /// Existing file-scope item replaced in place, selected by successful
    /// whole-program compilation rather than by parsing a declaration name.
    pub file_replacement: Option<usize>,
    /// The input is a valid expression, but its value category has no printer;
    /// it was evaluated through the silent expression wrapper instead.
    pub unprintable: bool,
    /// Execution duration of the compiled program.
    pub duration: Duration,
}

pub enum Eval {
    Done(Outcome),
    /// Nothing compiled; the text is already remapped to input-relative lines.
    CompileError(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnicodeEncoding {
    Utf8,
    Utf16,
    Utf32,
}

impl UnicodeEncoding {
    fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16 => "UTF-16",
            Self::Utf32 => "UTF-32",
        }
    }

    fn literal_prefix(self) -> &'static str {
        match self {
            Self::Utf8 => "u8",
            Self::Utf16 => "u",
            Self::Utf32 => "U",
        }
    }

    fn unit_width(self) -> usize {
        match self {
            Self::Utf8 => 1,
            Self::Utf16 => 2,
            Self::Utf32 => 4,
        }
    }

    fn hex_digits(self) -> usize {
        self.unit_width() * 2
    }
}

pub struct Evaluator {
    pub tc: crate::toolchain::Toolchain,
    dir: tempfile::TempDir,
    pub timeout: Duration,
    stream_output: bool,
    allow_program_stdin: bool,
}

impl Evaluator {
    pub fn new(tc: crate::toolchain::Toolchain, timeout: Duration) -> Result<Self> {
        let dir = tempfile::tempdir().context("failed to create temporary directory")?;
        Ok(Evaluator {
            tc,
            dir,
            timeout,
            stream_output: false,
            allow_program_stdin: false,
        })
    }

    fn src_path(&self) -> PathBuf {
        self.dir.path().join("input.c")
    }

    fn exe_path(&self) -> PathBuf {
        self.dir
            .path()
            .join(format!("input{}", std::env::consts::EXE_SUFFIX))
    }

    /// Stream only the newest input's program output as it is produced. Batch
    /// mode leaves this off so transcript ordering remains deterministic.
    pub fn set_stream_output(&mut self, enabled: bool) {
        self.stream_output = enabled;
    }

    /// Whether the newest input may request real stdin. Disabled when stdin is
    /// itself carrying piped C source; enabled for a TTY, `-e`, and scripts.
    pub fn set_program_stdin(&mut self, enabled: bool) {
        self.allow_program_stdin = enabled;
    }

    /// Swap in a different compiler, re-probing what it supports.
    pub fn set_toolchain(&mut self, tc: crate::toolchain::Toolchain) {
        self.tc = tc;
    }

    /// Compile `text`. Either way the compiler's raw diagnostics come back —
    /// on success they are the warnings, which a beginner needs to see.
    ///
    /// The compiler runs under the same deadline as user programs: a hung
    /// compiler (pathological macro expansion, compiler bug) must not freeze
    /// the REPL any more than a hung program may.
    fn compile_text(&self, text: &str) -> std::result::Result<(PathBuf, String), String> {
        let src = self.src_path();
        let exe = self.exe_path();
        if let Err(e) = std::fs::write(&src, text) {
            return Err(format!("cannot write temporary source: {e}"));
        }
        let args = self.tc.compile_args(&src, &exe, self.dir.path());
        let mut cmd = Command::new(&self.tc.path);
        cmd.args(&args);
        let cap = match proc::run_captured(&mut cmd, self.timeout, false) {
            Ok(c) => c,
            Err(e) => return Err(format!("cannot run compiler: {e}")),
        };
        let mut d = String::from_utf8_lossy(&cap.stderr).into_owned();
        d.push_str(&String::from_utf8_lossy(&cap.stdout));
        if cap.stdout_truncated || cap.stderr_truncated {
            d.push_str(&format!(
                "\ncompiler output exceeded {} MiB per stream and was truncated\n",
                proc::MAX_CAPTURE_BYTES / (1024 * 1024)
            ));
        }
        match cap.status {
            Some(st) if st.success() => Ok((exe, d)),
            Some(_) => Err(d),
            None => Err(format!(
                "compiler timed out after {}s and was killed",
                self.timeout.as_secs()
            )),
        }
    }

    /// Query an expression's type through the generated `_Generic` table.
    /// The controlling expression is not evaluated, and the result is never
    /// committed to the replay journal.
    pub fn type_of(&self, session: &Session, input: &str) -> Result<Eval> {
        let prog = codegen::build_type_probe(session, input);
        self.run_probe(session, prog)
    }

    /// Inspect a scalar expression's object representation without retaining
    /// it in the replay journal. The generated `_Generic` dispatch evaluates
    /// the expression once, as the selected helper's function argument.
    pub fn bits_of(&self, session: &Session, input: &str, uppercase: bool) -> Result<Eval> {
        let prog = codegen::build_bits_probe(session, input, uppercase);
        self.run_probe(session, prog)
    }

    /// Explicitly interpret an integer pointer/array as Unicode code units.
    /// `_Generic` and `sizeof` inspect the expression without evaluating it;
    /// the selected helper receives the pointer exactly once.
    pub fn unicode_of(
        &self,
        session: &Session,
        input: &str,
        encoding: UnicodeEncoding,
        count: Option<usize>,
    ) -> Result<Eval> {
        const DEFAULT_LIMIT: usize = 100;

        let limit = count.unwrap_or(DEFAULT_LIMIT);
        let prog = codegen::build_unicode_probe(
            session,
            input,
            encoding.unit_width(),
            limit,
            count.is_some(),
        );
        let mut result = self.run_probe(session, prog)?;
        if let Eval::Done(outcome) = &mut result
            && outcome.abnormal.is_none()
            && let Some(rendered) = outcome
                .value
                .as_deref()
                .and_then(|value| render_unicode_payload(value, encoding, limit))
        {
            outcome.value = Some(rendered);
        }
        Ok(result)
    }

    fn run_probe(&self, session: &Session, prog: codegen::Program) -> Result<Eval> {
        let src = self.src_path().display().to_string();
        let (start, count) = (prog.new_start_line, prog.new_line_count);
        match self.compile_text(&prog.src) {
            Ok((exe, warns)) => {
                let warnings = errmap::only_new(&errmap::remap(
                    &warns,
                    &src,
                    &prog.src,
                    start,
                    count,
                    &prog.session_line_ranges,
                    prog.wrapped,
                ));
                Ok(Eval::Done(self.run(
                    &exe,
                    Slot::Expr,
                    warnings,
                    prog.uses_stdin_tape,
                    session.stdin_tape(),
                )?))
            }
            Err(diag) => Ok(Eval::CompileError(errmap::drop_stale_warnings(
                &errmap::remap(
                    &diag,
                    &src,
                    &prog.src,
                    start,
                    count,
                    &prog.session_line_ranges,
                    prog.wrapped,
                ),
            ))),
        }
    }

    pub fn eval(&self, session: &Session, input: &str) -> Result<Eval> {
        if function_definition_name(input).as_deref() == Some("main") {
            return Ok(Eval::CompileError(
                "c-shell already provides main(); enter the statements from its body directly and omit the final return"
                    .to_string(),
            ));
        }

        let diag = match self.attempt(session, input)? {
            Ok(o) => return Ok(Eval::Done(o)),
            Err(d) => d,
        };

        // Forgetting the semicolon is the single most common beginner
        // mistake, and the diagnostic for it points at generated scaffolding
        // rather than at anything the user wrote. Retrying costs one compile
        // and only ever on the path that already failed.
        //
        // A closing `}` does not exempt an input from this. A braced
        // initializer (`int a[3] = {1, 2, 3}`) ends in `}` yet is still a
        // declaration needing its semicolon, and the diagnostic it produces is
        // exactly the scaffolding `do` token that gets sanitized away — so
        // without the retry the input fails in complete silence. Genuinely
        // complete blocks never reach here, because they compiled already.
        let t = input.trim_end();
        let repairable = !t.ends_with(';') && !t.trim_start().starts_with('#');
        if repairable {
            let patched = format!("{t};");
            if let Ok(mut o) = self.attempt(session, &patched)? {
                o.rewritten = Some(patched);
                return Ok(Eval::Done(o));
            }
        }
        Ok(Eval::CompileError(diag))
    }

    /// Try compiler-validated assembly strategies. File-scope-shaped input is
    /// never demoted into `main`: GCC nested functions would make identical
    /// sessions diverge from Clang/MSVC. On a normal failure, candidate
    /// rebinding strategies are accepted only when the whole program compiles.
    fn attempt(
        &self,
        session: &Session,
        input: &str,
    ) -> Result<std::result::Result<Outcome, String>> {
        if looks_file_scope(input) {
            let normal = codegen::build(session, input, Slot::FileScope);
            let reported = match self.try_program(normal, Slot::FileScope, session.stdin_tape())? {
                Ok(o) => return Ok(Ok(o)),
                Err(diag) => diag,
            };

            // Syntax/type errors must not trigger O(session size) retries.
            // Only diagnostics in the compiler's redeclaration family enter
            // replacement arbitration; the candidates themselves are still
            // accepted solely by compiling the complete program.
            if !is_rebinding_diagnostic(&reported) {
                return Ok(Err(reported));
            }

            // Substitute each prior definition in place, newest first. An
            // unrelated removal cannot normally cure a redefinition, while
            // replacing the matching item does; the compiler remains the name
            // and compatibility oracle. Preprocessor directives are not
            // candidates because dropping an #include is not rebinding.
            for index in (0..session.file_items.len()).rev() {
                if session.file_items[index].trim_start().starts_with('#') {
                    continue;
                }
                let prog = codegen::build_file_replacement(session, input, index);
                if let Ok(mut o) = self.try_program(prog, Slot::FileScope, session.stdin_tape())? {
                    o.file_replacement = Some(index);
                    return Ok(Ok(o));
                }
            }
            return Ok(Err(reported));
        }

        let mut reported = String::new();
        let mut expr_reported = None;
        let try_expr = should_try_expr(input);
        let slots: &[Slot] = if try_expr {
            &[Slot::Expr, Slot::Stmt]
        } else {
            &[Slot::Stmt]
        };
        for &slot in slots {
            let normal = codegen::build(session, input, slot);
            match self.try_program(normal, slot, session.stdin_tape())? {
                Ok(o) if slot == Slot::Expr => {
                    return Ok(Ok(self.refine_array(session, input, o)?));
                }
                Ok(o) => return Ok(Ok(o)),
                Err(diag) if slot == Slot::Stmt => {
                    let retry_scoped = is_rebinding_diagnostic(&diag);
                    reported = diag;
                    if retry_scoped {
                        let scoped = codegen::build_scoped_stmt(session, input);
                        if let Ok(mut o) =
                            self.try_program(scoped, Slot::Stmt, session.stdin_tape())?
                        {
                            o.scoped_rebind = true;
                            return Ok(Ok(o));
                        }
                    }
                }
                Err(diag) => expr_reported = Some(diag),
            }
        }

        // The normal Expr wrapper can fail solely because CS_PRINT has no
        // association for this value category. Before missing-semicolon
        // repair turns it into a statement, compile and execute the same text
        // as a silent expression so the UI can explain why there is no value.
        if try_expr {
            let prog = codegen::build_expr_probe(session, input);
            if let Ok(mut o) = self.try_program(prog, Slot::Expr, session.stdin_tape())? {
                o.value = None;
                o.unprintable = true;
                return Ok(Ok(o));
            }
        }
        // Sanitizing statement-fallback fallout can remove its only block for
        // a genuinely incomplete expression (`1 +`). Never return a blank
        // error when the earlier expression compile has a real diagnostic.
        if reported.trim().is_empty()
            && let Some(expr) = expr_reported
        {
            reported = expr;
        }
        Ok(Err(reported))
    }

    /// Recover an array that `_Generic` could only report as an address.
    ///
    /// An array decays to a pointer before the value printer can see its type,
    /// so a bare address is the one clue that the input might have been an
    /// array. Re-printing it through the array-aware wrapper settles the
    /// question with the real compiler and the real object: the generated code
    /// falls back to the ordinary printer when the value turns out to be a
    /// genuine pointer, and it fails to compile outright for values that
    /// cannot be indexed at all (`void *`, function pointers). Either way the
    /// original result stands, so this can only add information.
    ///
    /// Depth cannot be decided at run time the way array-ness can, so depth 1
    /// is tried first — it is both the common case and the cheap one — and a
    /// deeper pass runs only when the elements themselves had no printer.
    fn refine_array(&self, session: &Session, input: &str, outcome: Outcome) -> Result<Outcome> {
        // Re-running the whole session must stay free of consequences.
        if lex::may_have_side_effects(input) {
            return Ok(outcome);
        }

        // An explicit u8 literal is an array regardless of the language mode,
        // but its element type changed from char before C23 to unsigned char
        // in C23. In the older modes the ordinary printer therefore renders
        // it as a C string instead of the address that normally triggers array
        // refinement. Source spelling gives us stronger evidence here, so run
        // the bounded raw-byte probe before applying the address heuristic.
        // The generated probe trusts this conservative source classification
        // instead of repeating a compiler-sensitive address comparison.
        if session.is_explicit_utf8_array_expr(input) {
            let prog = codegen::build_utf8_array_expr(session, input);
            if let Ok(mut candidate) = self.try_program(prog, Slot::Expr, session.stdin_tape())?
                && let Some(rendered) = candidate.value.as_deref().and_then(render_utf8_payload)
            {
                candidate.value = Some(rendered);
                return Ok(candidate);
            }
        }

        let addressed = outcome
            .value
            .as_deref()
            .is_some_and(|v| v.starts_with("0x"));
        if !addressed {
            return Ok(outcome);
        }

        let mut best = outcome;
        for depth in 1..=MAX_ARRAY_DEPTH {
            let prog = codegen::build_array_expr(session, input, depth);
            let Ok(candidate) = self.try_program(prog, Slot::Expr, session.stdin_tape())? else {
                break;
            };
            // No leading brace means the run-time check found a real pointer
            // object and the wrapper deferred to the ordinary printer.
            let Some(value) = candidate.value.as_deref().filter(|v| v.starts_with('{')) else {
                break;
            };
            let elements_unprintable = value.contains(UNPRINTABLE);
            best = candidate;
            if !elements_unprintable {
                break;
            }
        }
        Ok(best)
    }

    fn try_program(
        &self,
        prog: codegen::Program,
        slot: Slot,
        stdin_tape: &[proc::StdinEvent],
    ) -> Result<std::result::Result<Outcome, String>> {
        let src = self.src_path().display().to_string();
        let (start, count) = (prog.new_start_line, prog.new_line_count);
        match self.compile_text(&prog.src) {
            Ok((exe, warns)) => {
                let warnings = errmap::only_new(&errmap::remap(
                    &warns,
                    &src,
                    &prog.src,
                    start,
                    count,
                    &prog.session_line_ranges,
                    prog.wrapped,
                ));
                Ok(Ok(self.run(
                    &exe,
                    slot,
                    warnings,
                    prog.uses_stdin_tape,
                    stdin_tape,
                )?))
            }
            Err(diag) => Ok(Err(errmap::drop_stale_warnings(&errmap::remap(
                &diag,
                &src,
                &prog.src,
                start,
                count,
                &prog.session_line_ranges,
                prog.wrapped,
            )))),
        }
    }

    fn run(
        &self,
        exe: &PathBuf,
        slot: Slot,
        warnings: String,
        uses_stdin_tape: bool,
        stdin_tape: &[proc::StdinEvent],
    ) -> Result<Outcome> {
        let mut cmd = Command::new(exe);
        let taped = uses_stdin_tape || !stdin_tape.is_empty();
        let (request_tx, request_rx) = mpsc::channel();
        let live =
            (self.stream_output || taped).then(|| LiveStreams::new(self.stream_output, request_tx));
        let start_time = std::time::Instant::now();
        let cap = if taped {
            proc::run_observed_with_stdin_tape(
                &mut cmd,
                self.timeout,
                stdin_tape,
                self.allow_program_stdin,
                request_rx,
                live.as_ref().expect("taped observer").observers(),
            )
        } else if let Some(live) = &live {
            proc::run_observed(
                &mut cmd,
                self.timeout,
                self.allow_program_stdin,
                live.observers(),
            )
        } else {
            proc::run_captured(&mut cmd, self.timeout, self.allow_program_stdin)
        }
        .with_context(|| format!("failed to start {}", exe.display()))?;
        let duration = start_time.elapsed();
        if let Some(live) = &live {
            live.finish();
        }

        let out = String::from_utf8_lossy(&cap.stdout).into_owned();
        let err = String::from_utf8_lossy(&cap.stderr).into_owned();
        let out_parts = split_new(&out, true);
        let err_parts = split_new(&err, false);

        let abnormal = match cap.status {
            None => Some(format!(
                "killed after {}s (possible infinite loop)",
                self.timeout.as_secs()
            )),
            Some(st) => describe_abnormal(&st),
        }
        .or_else(|| {
            (cap.stdout_truncated || cap.stderr_truncated).then(|| {
                format!(
                    "program output exceeded {} MiB per stream and was truncated",
                    proc::MAX_CAPTURE_BYTES / (1024 * 1024)
                )
            })
        })
        .or_else(|| {
            cap.stdin_diverged.then(|| {
                "stdin tape diverged while replaying retained input; use %reset".to_string()
            })
        })
        .or_else(|| {
            (!out_parts.done || !err_parts.done)
                .then(|| "program exited before the input completed".to_string())
        });

        let streamed_output_needs_newline = live
            .as_ref()
            .is_some_and(|live| live.terminal_needs_newline());

        Ok(Outcome {
            slot,
            output: if self.stream_output {
                String::new()
            } else {
                out_parts.output
            },
            value: out_parts.value.filter(|_| {
                slot == Slot::Expr
                    && out_parts.done
                    && err_parts.done
                    && !cap.stdout_truncated
                    && !cap.stderr_truncated
            }),
            errors: if self.stream_output {
                String::new()
            } else {
                err_parts.output
            },
            streamed_output_needs_newline,
            stdin_events: cap.stdin_recorded,
            warnings,
            abnormal,
            rewritten: None,
            scoped_rebind: false,
            file_replacement: None,
            unprintable: false,
            duration,
        })
    }

    /// Benchmark an expression or statement without committing it to the session.
    pub fn timeit(&self, session: &Session, input: &str) -> Result<Eval> {
        let mut loop_input = input.to_string();
        let mut prog = codegen::build_timeit_probe(session, &loop_input, 1);
        let res = match self.try_program(prog, Slot::Expr, session.stdin_tape())? {
            Ok(o) => Ok(o),
            Err(_) if !input.trim_end().ends_with(';') => {
                loop_input = format!("{};", input.trim_end());
                prog = codegen::build_timeit_probe(session, &loop_input, 1);
                self.try_program(prog, Slot::Expr, session.stdin_tape())?
            }
            Err(e) => Err(e),
        };

        let outcome1 = match res {
            Ok(o) => o,
            Err(diag) => return Ok(Eval::CompileError(diag)),
        };

        if outcome1.abnormal.is_some() {
            return Ok(Eval::Done(outcome1));
        }

        let elapsed_ns_1: u64 = outcome1
            .value
            .as_deref()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let target_ns: u64 = 100_000_000;
        let ladder: &[u64] = &[
            1,
            2,
            5,
            10,
            20,
            50,
            100,
            200,
            500,
            1_000,
            2_000,
            5_000,
            10_000,
            20_000,
            50_000,
            100_000,
            200_000,
            500_000,
            1_000_000,
            2_000_000,
            5_000_000,
            10_000_000,
            20_000_000,
            50_000_000,
            100_000_000,
        ];

        let loops = if elapsed_ns_1 >= target_ns {
            1
        } else {
            let needed = target_ns / elapsed_ns_1.max(1);
            *ladder
                .iter()
                .find(|&&n| n >= needed)
                .unwrap_or(&100_000_000)
        };

        let rounds = 5;
        let mut samples_ns: Vec<f64> = Vec::with_capacity(rounds);
        let mut last_outcome = outcome1;

        for _ in 0..rounds {
            let p = codegen::build_timeit_probe(session, &loop_input, loops);
            if let Ok(Ok(o)) = self.try_program(p, Slot::Expr, session.stdin_tape()) {
                let ns_val = o.value.as_deref().and_then(|v| v.parse::<u64>().ok());
                if let Some(ns) = ns_val {
                    let per_loop_ns = ns as f64 / loops as f64;
                    samples_ns.push(per_loop_ns);
                    last_outcome = o;
                }
            }
        }

        if samples_ns.is_empty() {
            samples_ns.push(elapsed_ns_1 as f64);
        }

        let count = samples_ns.len() as f64;
        let mean = samples_ns.iter().sum::<f64>() / count;
        let variance = if samples_ns.len() > 1 {
            samples_ns.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / (count - 1.0)
        } else {
            0.0
        };
        let std_dev = variance.sqrt();

        let (mean_str, dev_str) = format_timeit_stats(mean, std_dev);
        let report = format!(
            "{} ± {} per loop (mean ± std. dev. of {} run{}, {} loop{} each)",
            mean_str,
            dev_str,
            samples_ns.len(),
            if samples_ns.len() == 1 { "" } else { "s" },
            format_integer(loops),
            if loops == 1 { "" } else { "s" }
        );

        Ok(Eval::Done(Outcome {
            slot: Slot::Expr,
            output: last_outcome.output,
            value: Some(report),
            errors: last_outcome.errors,
            streamed_output_needs_newline: last_outcome.streamed_output_needs_newline,
            stdin_events: Vec::new(),
            warnings: last_outcome.warnings,
            abnormal: None,
            rewritten: None,
            scoped_rebind: false,
            file_replacement: None,
            unprintable: false,
            duration: last_outcome.duration,
        }))
    }
}

fn render_utf8_payload(value: &str) -> Option<String> {
    const LIMIT: usize = 100;

    let payload = value.strip_prefix(M_UTF8)?;
    let (size, hex) = payload.split_once(':')?;
    let size = size.parse::<usize>().ok()?;
    if hex.len() % 2 != 0 || hex.len() / 2 != size.min(LIMIT) {
        return None;
    }
    let bytes = hex
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(text, 16).ok()
        })
        .collect::<Option<Vec<_>>>()?;

    if size > LIMIT {
        return Some(render_byte_array(&bytes, size));
    }
    let text_bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes.as_slice());
    let Ok(text) = std::str::from_utf8(text_bytes) else {
        return Some(render_byte_array(&bytes, size));
    };

    let escaped = text.chars().map(escape_utf8_char).collect::<String>();
    let code_units = bytes
        .iter()
        .map(|byte| format!("0x{byte:02x}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("u8\"{escaped}\"\ncode units: {{{code_units}}}"))
}

fn render_unicode_payload(value: &str, encoding: UnicodeEncoding, limit: usize) -> Option<String> {
    let payload = value.strip_prefix(M_UNICODE)?;
    let mut fields = payload.split(':');
    let actual_width = fields.next()?.parse::<usize>().ok()?;
    let address = fields.next()?;
    let encoded_units = fields.next()?;
    let status = fields.next()?;
    let count = fields.next()?.parse::<usize>().ok()?;
    if fields.next().is_some()
        || address.is_empty()
        || !address.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !matches!(status, "N" | "M" | "T" | "L" | "E")
    {
        return None;
    }

    let units = if encoded_units.is_empty() {
        Vec::new()
    } else {
        encoded_units
            .split(',')
            .map(|unit| {
                (unit.len() == 8)
                    .then(|| u32::from_str_radix(unit, 16).ok())
                    .flatten()
            })
            .collect::<Option<Vec<_>>>()?
    };
    if count != units.len() || matches!(status, "N" | "M") && count != 0 {
        return None;
    }

    let mut lines = vec![format!("encoding: {}", encoding.label())];
    if status == "N" {
        lines.push("address: NULL".to_string());
        return Some(lines.join("\n"));
    }

    lines.push(format!("address: 0x{address}"));
    if status == "M" {
        lines.push(format!(
            "error: expected {}-byte code units, but the expression points to {actual_width}-byte elements",
            encoding.unit_width()
        ));
        return Some(lines.join("\n"));
    }
    if actual_width != encoding.unit_width() {
        return None;
    }

    let text_units = units.strip_suffix(&[0]).unwrap_or(units.as_slice());
    let text_label = if status == "L" { "text prefix" } else { "text" };
    match decode_unicode(text_units, encoding) {
        Ok(text) => {
            let escaped = text.chars().map(escape_utf8_char).collect::<String>();
            lines.push(format!(
                "{text_label}: {}\"{escaped}\"",
                encoding.literal_prefix()
            ));
        }
        Err(error) => lines.push(format!("{text_label}: <{error}>")),
    }

    let code_units = units
        .iter()
        .map(|unit| format!("0x{unit:0width$x}", width = encoding.hex_digits()))
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(format!("code units: {{{code_units}}}"));
    if status == "L" {
        lines.push(format!(
            "note: no NUL terminator in the first {limit} code units"
        ));
    }
    Some(lines.join("\n"))
}

fn decode_unicode(units: &[u32], encoding: UnicodeEncoding) -> std::result::Result<String, String> {
    match encoding {
        UnicodeEncoding::Utf8 => {
            let bytes = units
                .iter()
                .enumerate()
                .map(|(index, &unit)| {
                    u8::try_from(unit)
                        .map_err(|_| format!("invalid UTF-8 code unit at index {index}"))
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            std::str::from_utf8(&bytes)
                .map(str::to_string)
                .map_err(|error| {
                    format!(
                        "invalid UTF-8 sequence at code unit {}",
                        error.valid_up_to()
                    )
                })
        }
        UnicodeEncoding::Utf16 => decode_utf16(units),
        UnicodeEncoding::Utf32 => units
            .iter()
            .enumerate()
            .map(|(index, &unit)| {
                char::from_u32(unit)
                    .ok_or_else(|| format!("invalid UTF-32 scalar value at index {index}"))
            })
            .collect(),
    }
}

fn decode_utf16(units: &[u32]) -> std::result::Result<String, String> {
    let mut text = String::new();
    let mut index = 0usize;
    while index < units.len() {
        let first = units[index];
        if first > u16::MAX as u32 {
            return Err(format!("invalid UTF-16 code unit at index {index}"));
        }
        if (0xd800..=0xdbff).contains(&first) {
            let Some(&second) = units.get(index + 1) else {
                return Err(format!("unpaired UTF-16 high surrogate at index {index}"));
            };
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(format!("unpaired UTF-16 high surrogate at index {index}"));
            }
            let scalar = 0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00);
            text.push(char::from_u32(scalar).expect("paired surrogates form a scalar"));
            index += 2;
        } else if (0xdc00..=0xdfff).contains(&first) {
            return Err(format!("unpaired UTF-16 low surrogate at index {index}"));
        } else {
            text.push(char::from_u32(first).expect("non-surrogate u16 is a scalar"));
            index += 1;
        }
    }
    Ok(text)
}

fn render_byte_array(bytes: &[u8], size: usize) -> String {
    let mut elements = bytes.iter().map(u8::to_string).collect::<Vec<_>>();
    if size > bytes.len() {
        elements.push(format!("... ({} more)", size - bytes.len()));
    }
    format!("{{{}}}", elements.join(", "))
}

fn escape_utf8_char(character: char) -> String {
    match character {
        '"' => "\\\"".to_string(),
        '\\' => "\\\\".to_string(),
        '\0' => "\\0".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        character
            if character.is_control()
                || matches!(
                    character,
                    '\u{00ad}'
                        | '\u{061c}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{2028}'..='\u{202e}'
                        | '\u{2060}'..='\u{206f}'
                        | '\u{feff}'
                        | '\u{fff9}'..='\u{fffb}'
                        | '\u{e0000}'..='\u{e007f}'
                ) =>
        {
            format!("\\u{{{:x}}}", character as u32)
        }
        character => character.to_string(),
    }
}

fn format_timeit_stats(mean_ns: f64, std_dev_ns: f64) -> (String, String) {
    if mean_ns < 1_000.0 {
        (
            format!("{:.1} ns", mean_ns),
            format!("{:.1} ns", std_dev_ns),
        )
    } else if mean_ns < 1_000_000.0 {
        (
            format!("{:.2} µs", mean_ns / 1000.0),
            format!("{:.2} µs", std_dev_ns / 1000.0),
        )
    } else if mean_ns < 1_000_000_000.0 {
        (
            format!("{:.2} ms", mean_ns / 1_000_000.0),
            format!("{:.2} ms", std_dev_ns / 1_000_000.0),
        )
    } else {
        (
            format!("{:.2} s", mean_ns / 1_000_000_000.0),
            format!("{:.2} s", std_dev_ns / 1_000_000_000.0),
        )
    }
}

fn format_integer(mut n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut s = String::new();
    let mut count = 0;
    while n > 0 {
        if count > 0 && count % 3 == 0 {
            s.insert(0, ',');
        }
        s.insert(0, (b'0' + (n % 10) as u8) as char);
        n /= 10;
        count += 1;
    }
    s
}

struct SplitOutput {
    output: String,
    value: Option<String>,
    done: bool,
}

/// Keep only what the newest input produced, strip the completion marker and
/// separate the printed value. Missing M_DONE means even exit status 0 is not
/// enough to commit (`exit(0)` and a top-level `return` are the classic cases).
fn split_new(s: &str, has_value: bool) -> SplitOutput {
    let Some(start) = s.rfind(M_NEW) else {
        return SplitOutput {
            output: s.to_string(),
            value: None,
            done: false,
        };
    };
    let tail = &s[start + M_NEW.len()..];
    let (body, done) = match tail.rfind(M_DONE) {
        Some(i) => (&tail[..i], true),
        None => (tail, false),
    };
    let body = body.replace(M_STDIN, "");
    if has_value && let Some(i) = body.rfind(M_VAL) {
        return SplitOutput {
            output: body[..i].to_string(),
            value: Some(
                body[i + M_VAL.len()..]
                    .trim_end_matches(['\r', '\n'])
                    .to_string(),
            ),
            done,
        };
    }
    SplitOutput {
        output: body,
        value: None,
        done,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveState {
    BeforeInput,
    Output,
    Value,
    Done,
}

/// Incrementally strips replay/protocol bytes and returns only bytes that the
/// newest input itself wrote. Markers may straddle arbitrary pipe chunks.
struct LiveFilter {
    state: LiveState,
    stdout: bool,
    pending: Vec<u8>,
    visible_any: bool,
    visible_ends_in_newline: bool,
    stdin_requests: Vec<proc::StdinRequest>,
}

impl LiveFilter {
    fn new(stdout: bool) -> Self {
        Self {
            state: LiveState::BeforeInput,
            stdout,
            pending: Vec::new(),
            visible_any: false,
            visible_ends_in_newline: false,
            stdin_requests: Vec::new(),
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let mut visible = Vec::new();
        loop {
            match self.state {
                LiveState::BeforeInput => {
                    let new = find_bytes(&self.pending, M_NEW.as_bytes());
                    let request = (!self.stdout)
                        .then(|| find_bytes(&self.pending, M_STDIN.as_bytes()))
                        .flatten();
                    if request.is_some_and(|r| new.is_none_or(|n| r < n)) {
                        let i = request.expect("checked request");
                        self.pending.drain(..i + M_STDIN.len());
                        self.stdin_requests.push(proc::StdinRequest::Replay);
                    } else if let Some(i) = new {
                        self.pending.drain(..i + M_NEW.len());
                        self.state = LiveState::Output;
                    } else {
                        let markers: &[&[u8]] = if self.stdout {
                            &[M_NEW.as_bytes()]
                        } else {
                            &[M_NEW.as_bytes(), M_STDIN.as_bytes()]
                        };
                        retain_marker_prefix(&mut self.pending, markers);
                        break;
                    }
                }
                LiveState::Output => {
                    let value = self
                        .stdout
                        .then(|| find_bytes(&self.pending, M_VAL.as_bytes()))
                        .flatten();
                    let done = find_bytes(&self.pending, M_DONE.as_bytes());
                    let request = (!self.stdout)
                        .then(|| find_bytes(&self.pending, M_STDIN.as_bytes()))
                        .flatten();
                    let mut next = Vec::new();
                    if let Some(i) = value {
                        next.push((i, M_VAL.len(), Some(LiveState::Value)));
                    }
                    if let Some(i) = done {
                        next.push((i, M_DONE.len(), Some(LiveState::Done)));
                    }
                    if let Some(i) = request {
                        next.push((i, M_STDIN.len(), None));
                    }
                    if let Some((i, marker_len, state)) =
                        next.into_iter().min_by_key(|(i, _, _)| *i)
                    {
                        visible.extend_from_slice(&self.pending[..i]);
                        self.pending.drain(..i + marker_len);
                        if let Some(state) = state {
                            self.state = state;
                        } else {
                            self.stdin_requests.push(proc::StdinRequest::Current);
                        }
                    } else {
                        let markers: &[&[u8]] = if self.stdout {
                            &[M_VAL.as_bytes(), M_DONE.as_bytes()]
                        } else {
                            &[M_DONE.as_bytes(), M_STDIN.as_bytes()]
                        };
                        let keep = trailing_marker_prefix(&self.pending, markers);
                        let safe = self.pending.len() - keep;
                        visible.extend_from_slice(&self.pending[..safe]);
                        self.pending.drain(..safe);
                        break;
                    }
                }
                LiveState::Value => {
                    if let Some(i) = find_bytes(&self.pending, M_DONE.as_bytes()) {
                        self.pending.drain(..i + M_DONE.len());
                        self.state = LiveState::Done;
                    } else {
                        retain_marker_prefix(&mut self.pending, &[M_DONE.as_bytes()]);
                        break;
                    }
                }
                LiveState::Done => {
                    self.pending.clear();
                    break;
                }
            }
        }
        self.record_visible(&visible);
        visible
    }

    fn take_stdin_requests(&mut self) -> Vec<proc::StdinRequest> {
        std::mem::take(&mut self.stdin_requests)
    }

    fn finish(&mut self) -> Vec<u8> {
        let visible = if self.state == LiveState::Output {
            std::mem::take(&mut self.pending)
        } else {
            self.pending.clear();
            Vec::new()
        };
        self.record_visible(&visible);
        visible
    }

    fn record_visible(&mut self, visible: &[u8]) {
        if let Some(last) = visible.last() {
            self.visible_any = true;
            self.visible_ends_in_newline = *last == b'\n';
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn trailing_marker_prefix(bytes: &[u8], markers: &[&[u8]]) -> usize {
    markers
        .iter()
        .map(|marker| {
            let max = bytes.len().min(marker.len().saturating_sub(1));
            (1..=max)
                .rev()
                .find(|&n| bytes.ends_with(&marker[..n]))
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
}

fn retain_marker_prefix(bytes: &mut Vec<u8>, markers: &[&[u8]]) {
    let keep = trailing_marker_prefix(bytes, markers);
    bytes.drain(..bytes.len() - keep);
}

#[derive(Default)]
struct TerminalVisibility {
    visible_any: bool,
    visible_ends_in_newline: bool,
}

impl TerminalVisibility {
    fn record(&mut self, visible: &[u8]) {
        if let Some(last) = visible.last() {
            self.visible_any = true;
            self.visible_ends_in_newline = *last == b'\n';
        }
    }

    fn needs_newline(&self) -> bool {
        self.visible_any && !self.visible_ends_in_newline
    }
}

struct LiveStreams {
    stdout: Arc<Mutex<LiveFilter>>,
    stderr: Arc<Mutex<LiveFilter>>,
    terminal: Arc<Mutex<TerminalVisibility>>,
    stream_visible: bool,
    stdin_requests: mpsc::Sender<proc::StdinRequest>,
}

impl LiveStreams {
    fn new(stream_visible: bool, stdin_requests: mpsc::Sender<proc::StdinRequest>) -> Self {
        Self {
            stdout: Arc::new(Mutex::new(LiveFilter::new(true))),
            stderr: Arc::new(Mutex::new(LiveFilter::new(false))),
            terminal: Arc::new(Mutex::new(TerminalVisibility::default())),
            stream_visible,
            stdin_requests,
        }
    }

    fn observers(&self) -> proc::Observers {
        let stdout = Arc::clone(&self.stdout);
        let stderr = Arc::clone(&self.stderr);
        let stdout_terminal = Arc::clone(&self.terminal);
        let stderr_terminal = Arc::clone(&self.terminal);
        let stdout_requests = self.stdin_requests.clone();
        let stderr_requests = self.stdin_requests.clone();
        let stream_stdout = self.stream_visible;
        let stream_stderr = self.stream_visible;
        proc::Observers {
            stdout: Arc::new(move |chunk| {
                let (visible, requests) = {
                    let mut filter = stdout.lock().expect("stdout filter");
                    let visible = filter.feed(chunk);
                    (visible, filter.take_stdin_requests())
                };
                for request in requests {
                    let _ = stdout_requests.send(request);
                }
                if stream_stdout && !visible.is_empty() {
                    // Serialize the two stream writers so this state records
                    // the actual last byte placed on the shared terminal.
                    let mut terminal = stdout_terminal.lock().expect("terminal visibility");
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(&visible);
                    let _ = out.flush();
                    terminal.record(&visible);
                }
            }),
            stderr: Arc::new(move |chunk| {
                let (visible, requests) = {
                    let mut filter = stderr.lock().expect("stderr filter");
                    let visible = filter.feed(chunk);
                    (visible, filter.take_stdin_requests())
                };
                for request in requests {
                    let _ = stderr_requests.send(request);
                }
                if stream_stderr && !visible.is_empty() {
                    let mut terminal = stderr_terminal.lock().expect("terminal visibility");
                    let mut err = std::io::stderr().lock();
                    let _ = err.write_all(&visible);
                    let _ = err.flush();
                    terminal.record(&visible);
                }
            }),
        }
    }

    fn finish(&self) {
        let out_tail = self.stdout.lock().expect("stdout filter").finish();
        if self.stream_visible && !out_tail.is_empty() {
            let mut terminal = self.terminal.lock().expect("terminal visibility");
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(&out_tail);
            let _ = out.flush();
            terminal.record(&out_tail);
        }
        let err_tail = self.stderr.lock().expect("stderr filter").finish();
        if self.stream_visible && !err_tail.is_empty() {
            let mut terminal = self.terminal.lock().expect("terminal visibility");
            let mut err = std::io::stderr().lock();
            let _ = err.write_all(&err_tail);
            let _ = err.flush();
            terminal.record(&err_tail);
        }
    }

    fn terminal_needs_newline(&self) -> bool {
        self.terminal
            .lock()
            .expect("terminal visibility")
            .needs_newline()
    }
}

#[cfg(unix)]
fn describe_abnormal(st: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    let sig = st.signal()?;
    Some(match sig {
        11 => "program crashed: segmentation fault (SIGSEGV) — \
               usually a NULL/wild pointer dereference or an out-of-bounds index"
            .into(),
        6 => "program aborted (SIGABRT) — \
              usually a failed assert, or heap corruption caught by the C library"
            .into(),
        8 => "arithmetic error (SIGFPE) — usually integer division by zero".into(),
        4 => "illegal instruction (SIGILL)".into(),
        7 | 10 => "bus error (SIGBUS) — usually a misaligned memory access".into(),
        _ => format!("program terminated by signal {sig}"),
    })
}

#[cfg(windows)]
fn describe_abnormal(st: &std::process::ExitStatus) -> Option<String> {
    // Windows has no signals; a fatal fault surfaces as an NTSTATUS exit code.
    let code = st.code()? as u32;
    Some(match code {
        0xC0000005 => "program crashed: access violation — \
                       usually a NULL/wild pointer dereference or an out-of-bounds index"
            .into(),
        0xC0000094 => "arithmetic error: integer division by zero".into(),
        0xC000001D => "illegal instruction".into(),
        0xC00000FD => "stack overflow — usually runaway recursion or a huge stack array".into(),
        0xC0000409 => "stack buffer overrun detected".into(),
        _ => return None,
    })
}

/// Diagnostics that mean "this name already denotes something here".
/// GNU/Clang spellings are textual; MSVC diagnostic codes remain stable even
/// when its prose is localized. A positive match only enables a retry — the
/// alternate assembly still has to compile and run before it can be committed.
fn is_rebinding_diagnostic(diag: &str) -> bool {
    let lower = diag.to_ascii_lowercase();
    [
        "redefinition",
        "redeclaration",
        "redeclared",
        "conflicting types for",
        "already has a body",
        "c2011",
        "c2084",
        "c2086",
        "c2365",
        "c2371",
        "c2373",
        "c2374",
        "c2375",
        "c2556",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Whether expression classification can possibly succeed.
///
/// A top-level trailing semicolon makes the `CS_PRINT((input))` wrapper
/// syntactically impossible, so statement-shaped input takes one compiler
/// process instead of predictably failing an expression compile first. A
/// trailing `}` normally has the same property, except for C compound
/// literals such as `(int){1}` or `x = (Point){1, 2}`.
pub(crate) fn should_try_expr(input: &str) -> bool {
    let scan = lex::scan(input);
    let Some(last) = input
        .as_bytes()
        .iter()
        .enumerate()
        .rev()
        .find(|(i, byte)| scan.code[*i] && !byte.is_ascii_whitespace())
        .map(|(i, _)| i)
    else {
        return false;
    };
    match input.as_bytes()[last] {
        b';' => false,
        b'}' => ends_in_possible_compound_literal(input, &scan.code, last),
        _ => true,
    }
}

fn ends_in_possible_compound_literal(input: &str, code: &[bool], close: usize) -> bool {
    let bytes = input.as_bytes();
    let mut depth = 0usize;
    let mut open = None;
    for i in (0..=close).rev() {
        if !code[i] {
            continue;
        }
        match bytes[i] {
            b'}' => depth += 1,
            b'{' => {
                depth -= 1;
                if depth == 0 {
                    open = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(open) = open else { return true };
    let Some(before) = (0..open)
        .rev()
        .find(|&i| code[i] && !bytes[i].is_ascii_whitespace())
    else {
        return false;
    };
    if bytes[before] != b')' {
        return false;
    }

    // `if (x) { ... }` has the same `) {` boundary as a compound literal.
    // Clear control-flow prefixes are statements; ambiguous forms keep the
    // expression trial so classification can never reject valid C merely to
    // save a process launch.
    !lex::identifiers(input).first().is_some_and(|word| {
        matches!(
            word.as_str(),
            "if" | "for" | "while" | "switch" | "do" | "else" | "case"
        )
    })
}

/// Does this input have to live outside `main`?
///
/// This lexical routing is exclusive: once input has file-scope shape it is
/// never retried as a statement, because GCC might accept a nested function.
fn looks_file_scope(input: &str) -> bool {
    let t = input.trim_start();
    if t.starts_with('#') {
        return true;
    }
    let first: String = t
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    // `if (x) { ... }` also contains `) {`, so control flow is excluded before
    // the function-definition test below.
    if matches!(
        first.as_str(),
        "if" | "for"
            | "while"
            | "switch"
            | "do"
            | "else"
            | "return"
            | "case"
            | "goto"
            | "break"
            | "continue"
            | "sizeof"
    ) {
        return false;
    }
    if first == "typedef" {
        return true;
    }
    // A type definition, as opposed to a variable of that type: hoisting it
    // keeps the type usable from functions defined later. The two are told
    // apart by what follows the tag — `struct P {` defines, `struct P p`
    // declares.
    if matches!(first.as_str(), "struct" | "union" | "enum") {
        let after_kw = t[first.len()..].trim_start();
        let tag: String = after_kw
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if after_kw[tag.len()..].trim_start().starts_with('{') {
            return true;
        }
        return false;
    }
    !first.is_empty() && function_definition_name(input).is_some()
}

/// Return the simple function name whose parameter list meets a top-level
/// body brace. Merely spotting `) {` is insufficient: compound literals such
/// as `Point p = (Point){1, 2}` have the same byte pattern.
fn function_definition_name(input: &str) -> Option<String> {
    let sc = lex::scan(input);
    let b = input.as_bytes();
    let mut parens = 0i32;
    let mut brackets = 0i32;
    let mut brace = None;
    for (i, &byte) in b.iter().enumerate() {
        if !sc.code[i] {
            continue;
        }
        match byte {
            b'(' => parens += 1,
            b')' => parens -= 1,
            b'[' => brackets += 1,
            b']' => brackets -= 1,
            b'=' if parens == 0 && brackets == 0 => return None,
            b'{' if parens == 0 && brackets == 0 => {
                brace = Some(i);
                break;
            }
            // Braces while parentheses remain open can belong to a struct in
            // a parameter list (or to an expression); only a later top-level
            // brace can be the function body.
            b'{' => {}
            _ => {}
        }
    }
    let brace = brace?;
    let close = (0..brace)
        .rev()
        .find(|&i| sc.code[i] && !b[i].is_ascii_whitespace())?;
    if b[close] != b')' {
        return None;
    }

    // Find that closing paren's mate and require a declarator-like token just
    // before it. This rejects `(Type){...}` and `cond ? (Type){...}`.
    let mut depth = 0i32;
    let mut open = None;
    for i in (0..=close).rev() {
        if !sc.code[i] {
            continue;
        }
        match b[i] {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    open = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let open = open?;
    let before = (0..open)
        .rev()
        .find(|&i| sc.code[i] && !b[i].is_ascii_whitespace())?;
    if b[before] == b')' {
        // Parenthesized declarator: `int (main)(void) { ... }`.
        let mut depth = 0i32;
        let mut inner_open = None;
        for i in (0..=before).rev() {
            if !sc.code[i] {
                continue;
            }
            match b[i] {
                b')' => depth += 1,
                b'(' => {
                    depth -= 1;
                    if depth == 0 {
                        inner_open = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let inner_open = inner_open?;
        return identifier_between(input, &sc.code, inner_open + 1, before);
    }
    identifier_ending_at(input, &sc.code, before)
}

fn identifier_ending_at(input: &str, code: &[bool], end: usize) -> Option<String> {
    let b = input.as_bytes();
    if !(b[end] == b'_' || b[end].is_ascii_alphanumeric()) {
        return None;
    }
    let mut start = end;
    while start > 0
        && code[start - 1]
        && (b[start - 1] == b'_' || b[start - 1].is_ascii_alphanumeric())
    {
        start -= 1;
    }
    (!b[start].is_ascii_digit()).then(|| input[start..=end].to_string())
}

fn identifier_between(input: &str, code: &[bool], start: usize, end: usize) -> Option<String> {
    let b = input.as_bytes();
    let first = (start..end).find(|&i| code[i] && !b[i].is_ascii_whitespace())?;
    let last = (first..end)
        .rev()
        .find(|&i| code[i] && !b[i].is_ascii_whitespace())?;
    let ident = identifier_ending_at(input, code, last)?;
    let ident_start = last + 1 - ident.len();
    (ident_start == first).then_some(ident)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_impossible_expression_trials_without_losing_compound_literals() {
        assert!(!should_try_expr("int value = 1;"));
        assert!(!should_try_expr("value++; // retained statement"));
        assert!(!should_try_expr("if (value) { value--; }"));
        assert!(!should_try_expr("{ value++; }"));
        assert!(should_try_expr("value + 1"));
        assert!(should_try_expr("(int){ 7 }"));
        assert!(should_try_expr("value = (struct Box){ 7 }"));
    }

    #[test]
    fn recognizes_portable_rebinding_diagnostics() {
        assert!(is_rebinding_diagnostic("error: redefinition of 'f'"));
        assert!(is_rebinding_diagnostic("error: conflicting types for 'x'"));
        assert!(is_rebinding_diagnostic(
            "error C2084: function 'f' already has a body"
        ));
        assert!(!is_rebinding_diagnostic("error: expected expression"));
    }

    #[test]
    fn recognizes_main_definitions_without_matching_similar_expressions() {
        assert_eq!(
            function_definition_name("int main(void) { return 0; }").as_deref(),
            Some("main")
        );
        assert_eq!(
            function_definition_name("int (main)(int argc, char **argv) { return argc; }")
                .as_deref(),
            Some("main")
        );
        assert_eq!(
            function_definition_name("int domain(void) { return 1; }").as_deref(),
            Some("domain")
        );
        assert_eq!(function_definition_name("Point p = (Point){1, 2};"), None);
    }

    #[test]
    fn routes_definitions_to_file_scope() {
        assert!(looks_file_scope("int add(int a, int b) { return a + b; }"));
        assert!(looks_file_scope(
            "int get(struct Local { int x; } v) { return v.x; }"
        ));
        assert!(looks_file_scope("#include <time.h>"));
        assert!(looks_file_scope("typedef unsigned long ulong;"));
        assert!(looks_file_scope("struct P { int x; int y; };"));
    }

    #[test]
    fn keeps_statements_in_main() {
        assert!(!looks_file_scope("if (x > 0) { puts(\"hi\"); }"));
        assert!(!looks_file_scope("for (int i = 0; i < 3; i++) { }"));
        assert!(!looks_file_scope("(struct P){ 1, 2 }"));
        assert!(!looks_file_scope("Point p = (Point){ 1, 2 };"));
        assert!(!looks_file_scope("flag ? (Point){ 1, 2 } : other"));
        assert!(!looks_file_scope("int x = 41;"));
        assert!(!looks_file_scope("struct P p = {1, 2};"));
        assert!(!looks_file_scope("x + 1"));
    }

    #[test]
    fn splits_on_markers_and_requires_completion() {
        let s = format!("old{M_NEW}printed{M_VAL}42\n{M_DONE}");
        let got = split_new(&s, true);
        assert_eq!(got.output, "printed");
        assert_eq!(got.value.as_deref(), Some("42"));
        assert!(got.done);
    }

    #[test]
    fn renders_only_valid_complete_utf8_payloads_as_text() {
        let smiley = format!("{M_UTF8}5:f09f998200");
        assert_eq!(
            render_utf8_payload(&smiley).as_deref(),
            Some("u8\"🙂\"\ncode units: {0xf0, 0x9f, 0x99, 0x82, 0x00}")
        );

        let escaped = format!("{M_UTF8}8:61220a00e280ae00");
        assert_eq!(
            render_utf8_payload(&escaped).as_deref(),
            Some(
                "u8\"a\\\"\\n\\0\\u{202e}\"\ncode units: {0x61, 0x22, 0x0a, 0x00, 0xe2, 0x80, 0xae, 0x00}"
            )
        );

        let invalid = format!("{M_UTF8}2:ff00");
        assert_eq!(render_utf8_payload(&invalid).as_deref(), Some("{255, 0}"));
        assert!(render_utf8_payload("not an internal payload").is_none());
    }

    #[test]
    fn renders_utf8_utf16_and_utf32_probe_payloads_strictly() {
        let utf8 = format!("{M_UNICODE}1:1234:00000041,000000e5,000000a5,000000bd,00000000:T:5");
        assert_eq!(
            render_unicode_payload(&utf8, UnicodeEncoding::Utf8, 100).as_deref(),
            Some(
                "encoding: UTF-8\naddress: 0x1234\ntext: u8\"A好\"\ncode units: {0x41, 0xe5, 0xa5, 0xbd, 0x00}"
            )
        );

        let utf16 = format!("{M_UNICODE}2:abcd:00000041,0000597d,0000d83d,0000de00,00000000:T:5");
        assert_eq!(
            render_unicode_payload(&utf16, UnicodeEncoding::Utf16, 100).as_deref(),
            Some(
                "encoding: UTF-16\naddress: 0xabcd\ntext: u\"A好😀\"\ncode units: {0x0041, 0x597d, 0xd83d, 0xde00, 0x0000}"
            )
        );

        let utf32 = format!("{M_UNICODE}4:beef:00000041,0001f600,00000000:E:3");
        assert_eq!(
            render_unicode_payload(&utf32, UnicodeEncoding::Utf32, 3).as_deref(),
            Some(
                "encoding: UTF-32\naddress: 0xbeef\ntext: U\"A😀\"\ncode units: {0x00000041, 0x0001f600, 0x00000000}"
            )
        );
    }

    #[test]
    fn unicode_probe_reports_null_mismatch_invalid_and_unterminated_data() {
        let null = format!("{M_UNICODE}1:0::N:0");
        assert_eq!(
            render_unicode_payload(&null, UnicodeEncoding::Utf8, 100).as_deref(),
            Some("encoding: UTF-8\naddress: NULL")
        );

        let mismatch = format!("{M_UNICODE}4:1234::M:0");
        assert_eq!(
            render_unicode_payload(&mismatch, UnicodeEncoding::Utf16, 100).as_deref(),
            Some(
                "encoding: UTF-16\naddress: 0x1234\nerror: expected 2-byte code units, but the expression points to 4-byte elements"
            )
        );

        let invalid16 = format!("{M_UNICODE}2:1234:0000d800,00000000:T:2");
        assert!(
            render_unicode_payload(&invalid16, UnicodeEncoding::Utf16, 100)
                .unwrap()
                .contains("unpaired UTF-16 high surrogate at index 0")
        );
        let invalid32 = format!("{M_UNICODE}4:1234:00110000,00000000:T:2");
        assert!(
            render_unicode_payload(&invalid32, UnicodeEncoding::Utf32, 100)
                .unwrap()
                .contains("invalid UTF-32 scalar value at index 0")
        );

        let limited = format!("{M_UNICODE}1:1234:00000041,00000042:L:2");
        assert_eq!(
            render_unicode_payload(&limited, UnicodeEncoding::Utf8, 2).as_deref(),
            Some(
                "encoding: UTF-8\naddress: 0x1234\ntext prefix: u8\"AB\"\ncode units: {0x41, 0x42}\nnote: no NUL terminator in the first 2 code units"
            )
        );
    }

    #[test]
    fn missing_start_or_done_is_incomplete() {
        let no_start = split_new("crashed early", true);
        assert_eq!(no_start.output, "crashed early");
        assert!(!no_start.done);

        let no_done = split_new(&format!("{M_NEW}before exit"), true);
        assert_eq!(no_done.output, "before exit");
        assert!(!no_done.done);
    }

    #[test]
    fn live_filter_handles_markers_split_across_chunks() {
        let mut f = LiveFilter::new(true);
        let wire = format!("replayed{M_NEW}hello{M_VAL}42\n{M_DONE}");
        let mut visible = Vec::new();
        for chunk in wire.as_bytes().chunks(3) {
            visible.extend(f.feed(chunk));
        }
        visible.extend(f.finish());
        assert_eq!(visible, b"hello");
        assert_eq!(f.state, LiveState::Done);
        assert!(f.visible_any && !f.visible_ends_in_newline);
    }

    #[test]
    fn live_filter_reports_historical_and_current_stdin_requests() {
        let mut filter = LiveFilter::new(false);
        let wire = format!("old{M_STDIN}{M_NEW}prompt: {M_STDIN}{M_DONE}");
        let mut visible = Vec::new();
        let mut requests = Vec::new();
        for chunk in wire.as_bytes().chunks(3) {
            visible.extend(filter.feed(chunk));
            requests.extend(filter.take_stdin_requests());
        }
        assert_eq!(visible, b"prompt: ");
        assert_eq!(
            requests,
            [proc::StdinRequest::Replay, proc::StdinRequest::Current]
        );
    }

    #[test]
    fn terminal_newline_state_follows_the_last_interleaved_stream_write() {
        let mut terminal = TerminalVisibility::default();
        terminal.record(b"stdout without newline");
        assert!(terminal.needs_newline());
        terminal.record(b"stderr finishes the terminal line\n");
        assert!(!terminal.needs_newline());
        terminal.record(b"partial again");
        assert!(terminal.needs_newline());
    }

    #[test]
    fn stderr_live_filter_streams_until_done() {
        let mut f = LiveFilter::new(false);
        let wire = format!("old{M_NEW}warning text\n{M_DONE}");
        assert_eq!(f.feed(wire.as_bytes()), b"warning text\n");
        assert_eq!(f.state, LiveState::Done);
        assert!(f.visible_any && f.visible_ends_in_newline);
    }
}
