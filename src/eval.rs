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
    /// The input looked file-scope but only compiled inside `main`. Usually
    /// a function definition referencing session variables, which gcc
    /// quietly accepts as a GNU nested function and clang rejects — worth a
    /// visible note, or the session breaks the moment the compiler changes.
    pub demoted: bool,
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

    pub fn eval(&self, session: &Session, input: &str) -> Result<Eval> {
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

    /// Try each slot in turn; on total failure return the diagnostics from
    /// whichever slot the input most likely meant.
    fn attempt(
        &self,
        session: &Session,
        input: &str,
    ) -> Result<std::result::Result<Outcome, String>> {
        // A wrong guess here is not fatal: it only changes which attempt runs
        // first, and the remaining slots are still tried. It matters because
        // gcc accepts nested functions as a GNU extension, so a function
        // definition would otherwise be silently buried inside main, where
        // clang would later reject it.
        let file_first = looks_file_scope(input);
        // File scope is deliberately absent from the second list. It is a
        // wider scope than `main`, so offering it as a last resort turns a
        // redeclaration that ought to be an error into a silent global that
        // the local of the same name then shadows. Only the expression /
        // statement distinction is genuinely ambiguous; file-scope membership
        // is settled by the heuristic above.
        let order: &[Slot] = if file_first {
            &[Slot::FileScope, Slot::Stmt, Slot::Expr]
        } else {
            &[Slot::Expr, Slot::Stmt]
        };
        // Any expression is also a valid statement, so the statement attempt
        // reports the same underlying mistake in friendlier words. Report that
        // one unless the input was clearly meant to be file-scope.
        let report_slot = if file_first {
            Slot::FileScope
        } else {
            Slot::Stmt
        };
        let mut reported = String::new();

        for &slot in order {
            let prog = codegen::build(session, input, slot);
            let src = self.src_path().display().to_string();
            let (start, count) = (prog.new_start_line, prog.new_line_count);
            match self.compile_text(&prog.src) {
                Ok((exe, warns)) => {
                    let warnings =
                        errmap::only_new(&errmap::remap(&warns, &src, start, count, prog.wrapped));
                    let mut o = self.run(&exe, slot, warnings)?;
                    o.demoted = file_first && slot != Slot::FileScope;
                    return Ok(Ok(o));
                }
                Err(diag) => {
                    if slot == report_slot {
                        reported = errmap::drop_stale_warnings(&errmap::remap(
                            &diag,
                            &src,
                            start,
                            count,
                            prog.wrapped,
                        ));
                    }
                }
            }
        }

        // The normal Expr wrapper can fail solely because CS_PRINT has no
        // association for this value category. Before missing-semicolon
        // repair turns it into a statement, compile and execute the same text
        // as a silent expression so the UI can explain why there is no value.
        let t = input.trim_end();
        let bare_candidate =
            !t.ends_with(';') && !t.ends_with('}') && !t.trim_start().starts_with('#');
        if bare_candidate {
            let prog = codegen::build_expr_probe(session, input);
            let src = self.src_path().display().to_string();
            let (start, count) = (prog.new_start_line, prog.new_line_count);
            if let Ok((exe, warns)) = self.compile_text(&prog.src) {
                let warnings = errmap::only_new(&errmap::remap(&warns, &src, start, count, false));
                let mut o = self.run(&exe, Slot::Expr, warnings)?;
                o.value = None;
                o.unprintable = true;
                return Ok(Ok(o));
            }
        }
        Ok(Err(reported))
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
            demoted: false,
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
            value: Some(body[i + M_VAL.len()..].trim_end_matches('\n').to_string()),
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

/// Does this input have to live outside `main`?
///
/// Only a heuristic, and only used to pick which slot to try first.
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
    has_paren_then_brace(input)
}

/// True when a `)` is followed only by whitespace before the first `{`, the
/// shape of a function definition's parameter list meeting its body.
fn has_paren_then_brace(input: &str) -> bool {
    let sc = lex::scan(input);
    let b = input.as_bytes();
    let brace = match (0..b.len()).find(|&i| sc.code[i] && b[i] == b'{') {
        Some(i) => i,
        None => return false,
    };
    (0..brace)
        .rev()
        .filter(|&i| sc.code[i])
        .find(|&i| !b[i].is_ascii_whitespace())
        .map(|i| b[i] == b')')
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_definitions_to_file_scope() {
        assert!(looks_file_scope("int add(int a, int b) { return a + b; }"));
        assert!(looks_file_scope("#include <time.h>"));
        assert!(looks_file_scope("typedef unsigned long ulong;"));
        assert!(looks_file_scope("struct P { int x; int y; };"));
    }

    #[test]
    fn keeps_statements_in_main() {
        assert!(!looks_file_scope("if (x > 0) { puts(\"hi\"); }"));
        assert!(!looks_file_scope("for (int i = 0; i < 3; i++) { }"));
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
