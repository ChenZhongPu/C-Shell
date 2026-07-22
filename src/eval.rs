//! Compiling and running one input.
//!
//! A lexical heuristic identifies file-scope-shaped input; the compiler then
//! validates that choice and arbitrates expression versus statement.

use anyhow::{Context, Result};
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::codegen::{self, M_DONE, M_NEW, M_VAL, Slot};
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
}

pub enum Eval {
    Done(Outcome),
    /// Nothing compiled; the text is already remapped to input-relative lines.
    CompileError(String),
}

pub struct Evaluator {
    pub tc: crate::toolchain::Toolchain,
    dir: tempfile::TempDir,
    pub timeout: Duration,
    stream_output: bool,
}

impl Evaluator {
    pub fn new(tc: crate::toolchain::Toolchain, timeout: Duration) -> Result<Self> {
        let dir = tempfile::tempdir().context("failed to create temporary directory")?;
        Ok(Evaluator {
            tc,
            dir,
            timeout,
            stream_output: false,
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
                Ok(Eval::Done(self.run(&exe, Slot::Expr, warnings)?))
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
        let t = input.trim_end();
        let repairable = !t.ends_with(';') && !t.ends_with('}') && !t.trim_start().starts_with('#');
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
            let reported = match self.try_program(normal, Slot::FileScope)? {
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
                if let Ok(mut o) = self.try_program(prog, Slot::FileScope)? {
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
            match self.try_program(normal, slot)? {
                Ok(o) => return Ok(Ok(o)),
                Err(diag) if slot == Slot::Stmt => {
                    let retry_scoped = is_rebinding_diagnostic(&diag);
                    reported = diag;
                    if retry_scoped {
                        let scoped = codegen::build_scoped_stmt(session, input);
                        if let Ok(mut o) = self.try_program(scoped, Slot::Stmt)? {
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
            if let Ok(mut o) = self.try_program(prog, Slot::Expr)? {
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

    fn try_program(
        &self,
        prog: codegen::Program,
        slot: Slot,
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
                Ok(Ok(self.run(&exe, slot, warnings)?))
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

    fn run(&self, exe: &PathBuf, slot: Slot, warnings: String) -> Result<Outcome> {
        let mut cmd = Command::new(exe);
        let live = self.stream_output.then(LiveStreams::new);
        let cap = match &live {
            Some(live) => proc::run_observed(&mut cmd, self.timeout, true, live.observers()),
            None => proc::run_captured(&mut cmd, self.timeout, true),
        }
        .with_context(|| format!("failed to start {}", exe.display()))?;
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
            warnings,
            abnormal,
            rewritten: None,
            scoped_rebind: false,
            file_replacement: None,
            unprintable: false,
        })
    }
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
        output: body.to_string(),
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
}

impl LiveFilter {
    fn new(stdout: bool) -> Self {
        Self {
            state: LiveState::BeforeInput,
            stdout,
            pending: Vec::new(),
            visible_any: false,
            visible_ends_in_newline: false,
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let mut visible = Vec::new();
        loop {
            match self.state {
                LiveState::BeforeInput => {
                    if let Some(i) = find_bytes(&self.pending, M_NEW.as_bytes()) {
                        self.pending.drain(..i + M_NEW.len());
                        self.state = LiveState::Output;
                    } else {
                        retain_marker_prefix(&mut self.pending, &[M_NEW.as_bytes()]);
                        break;
                    }
                }
                LiveState::Output => {
                    let value = self
                        .stdout
                        .then(|| find_bytes(&self.pending, M_VAL.as_bytes()))
                        .flatten();
                    let done = find_bytes(&self.pending, M_DONE.as_bytes());
                    let next = match (value, done) {
                        (Some(v), Some(d)) if v < d => Some((v, M_VAL.len(), LiveState::Value)),
                        (_, Some(d)) => Some((d, M_DONE.len(), LiveState::Done)),
                        (Some(v), None) => Some((v, M_VAL.len(), LiveState::Value)),
                        (None, None) => None,
                    };
                    if let Some((i, marker_len, state)) = next {
                        visible.extend_from_slice(&self.pending[..i]);
                        self.pending.drain(..i + marker_len);
                        self.state = state;
                    } else {
                        let markers: &[&[u8]] = if self.stdout {
                            &[M_VAL.as_bytes(), M_DONE.as_bytes()]
                        } else {
                            &[M_DONE.as_bytes()]
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

    fn needs_newline(&self) -> bool {
        self.visible_any && !self.visible_ends_in_newline
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

struct LiveStreams {
    stdout: Arc<Mutex<LiveFilter>>,
    stderr: Arc<Mutex<LiveFilter>>,
}

impl LiveStreams {
    fn new() -> Self {
        Self {
            stdout: Arc::new(Mutex::new(LiveFilter::new(true))),
            stderr: Arc::new(Mutex::new(LiveFilter::new(false))),
        }
    }

    fn observers(&self) -> proc::Observers {
        let stdout = Arc::clone(&self.stdout);
        let stderr = Arc::clone(&self.stderr);
        proc::Observers {
            stdout: Arc::new(move |chunk| {
                let visible = stdout.lock().expect("stdout filter").feed(chunk);
                if !visible.is_empty() {
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(&visible);
                    let _ = out.flush();
                }
            }),
            stderr: Arc::new(move |chunk| {
                let visible = stderr.lock().expect("stderr filter").feed(chunk);
                if !visible.is_empty() {
                    let mut err = std::io::stderr().lock();
                    let _ = err.write_all(&visible);
                    let _ = err.flush();
                }
            }),
        }
    }

    fn finish(&self) {
        let out_tail = self.stdout.lock().expect("stdout filter").finish();
        if !out_tail.is_empty() {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(&out_tail);
            let _ = out.flush();
        }
        let err_tail = self.stderr.lock().expect("stderr filter").finish();
        if !err_tail.is_empty() {
            let mut err = std::io::stderr().lock();
            let _ = err.write_all(&err_tail);
            let _ = err.flush();
        }
    }

    fn terminal_needs_newline(&self) -> bool {
        self.stdout.lock().expect("stdout filter").needs_newline()
            || self.stderr.lock().expect("stderr filter").needs_newline()
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
fn should_try_expr(input: &str) -> bool {
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
        assert!(f.needs_newline());
    }

    #[test]
    fn stderr_live_filter_streams_until_done() {
        let mut f = LiveFilter::new(false);
        let wire = format!("old{M_NEW}warning text\n{M_DONE}");
        assert_eq!(f.feed(wire.as_bytes()), b"warning text\n");
        assert_eq!(f.state, LiveState::Done);
        assert!(!f.needs_newline());
    }
}
