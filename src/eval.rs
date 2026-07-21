//! Compiling and running one input.
//!
//! Classification is done by trial compilation rather than by parsing C: the
//! input is wrapped as an expression, then a statement, then a file-scope
//! item, and whichever the compiler accepts first is what it was. The
//! compiler is the only oracle that agrees with the compiler.

use anyhow::{Context, Result};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

use crate::codegen::{self, M_NEW, M_VAL, Slot};
use crate::errmap;
use crate::lex;
use crate::session::Session;

pub struct Outcome {
    pub slot: Slot,
    /// What the new input printed itself.
    pub output: String,
    /// The `Out[n]` value, when the input was an expression.
    pub value: Option<String>,
    /// Anything the new input wrote to stderr.
    pub errors: String,
    /// Compiler warnings, already remapped to input-relative lines.
    pub warnings: String,
    /// Set when the program died abnormally or ran too long.
    pub abnormal: Option<String>,
    /// Set when the input had to be repaired to compile; this is what should
    /// be committed to the session instead of what was typed.
    pub rewritten: Option<String>,
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
}

impl Evaluator {
    pub fn new(tc: crate::toolchain::Toolchain, timeout: Duration) -> Result<Self> {
        let dir = tempfile::tempdir().context("failed to create temporary directory")?;
        Ok(Evaluator { tc, dir, timeout })
    }

    fn src_path(&self) -> PathBuf {
        self.dir.path().join("input.c")
    }

    fn exe_path(&self) -> PathBuf {
        self.dir
            .path()
            .join(format!("input{}", std::env::consts::EXE_SUFFIX))
    }

    /// Swap in a different compiler, re-probing what it supports.
    pub fn set_toolchain(&mut self, tc: crate::toolchain::Toolchain) {
        self.tc = tc;
    }

    /// Compile `text`. Either way the compiler's raw diagnostics come back —
    /// on success they are the warnings, which a beginner needs to see.
    fn compile_text(&self, text: &str) -> std::result::Result<(PathBuf, String), String> {
        let src = self.src_path();
        let exe = self.exe_path();
        if let Err(e) = std::fs::write(&src, text) {
            return Err(format!("cannot write temporary source: {e}"));
        }
        let args = self.tc.compile_args(&src, &exe, self.dir.path());
        let out = match Command::new(&self.tc.path).args(&args).output() {
            Ok(o) => o,
            Err(e) => return Err(format!("cannot run compiler: {e}")),
        };
        let mut d = String::from_utf8_lossy(&out.stderr).into_owned();
        d.push_str(&String::from_utf8_lossy(&out.stdout));
        if out.status.success() {
            Ok((exe, d))
        } else {
            Err(d)
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
                    return Ok(Ok(self.run(&exe, slot, warnings)?));
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
        Ok(Err(reported))
    }

    fn run(&self, exe: &PathBuf, slot: Slot, warnings: String) -> Result<Outcome> {
        let mut child = Command::new(exe)
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start {}", exe.display()))?;

        // Drained on threads so a chatty program cannot fill a pipe buffer and
        // deadlock against our own timeout wait.
        let mut so = child.stdout.take().expect("stdout piped");
        let mut se = child.stderr.take().expect("stderr piped");
        let t_out = std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = so.read_to_end(&mut v);
            v
        });
        let t_err = std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = se.read_to_end(&mut v);
            v
        });

        let status = child.wait_timeout(self.timeout)?;
        let mut abnormal = None;
        match status {
            None => {
                let _ = child.kill();
                let _ = child.wait();
                abnormal = Some(format!(
                    "killed after {}s (possible infinite loop)",
                    self.timeout.as_secs()
                ));
            }
            Some(st) => {
                if let Some(msg) = describe_abnormal(&st) {
                    abnormal = Some(msg);
                }
            }
        }

        let out = String::from_utf8_lossy(&t_out.join().unwrap_or_default()).into_owned();
        let err = String::from_utf8_lossy(&t_err.join().unwrap_or_default()).into_owned();

        let (output, value) = split_new(&out);
        let (errors, _) = split_new(&err);

        Ok(Outcome {
            slot,
            output,
            value: value.filter(|_| slot == Slot::Expr),
            errors,
            warnings,
            abnormal,
            rewritten: None,
        })
    }
}

/// Keep only what the newest input produced, and separate the printed value.
///
/// If the marker is missing the program died before reaching the new input,
/// so everything is shown rather than nothing.
fn split_new(s: &str) -> (String, Option<String>) {
    let tail = match s.rfind(M_NEW) {
        Some(i) => &s[i + M_NEW.len()..],
        None => s,
    };
    match tail.rfind(M_VAL) {
        Some(i) => (
            tail[..i].to_string(),
            Some(tail[i + M_VAL.len()..].trim_end_matches('\n').to_string()),
        ),
        None => (tail.to_string(), None),
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
    fn splits_on_markers() {
        let s = format!("old{M_NEW}printed{M_VAL}42\n");
        let (out, val) = split_new(&s);
        assert_eq!(out, "printed");
        assert_eq!(val.as_deref(), Some("42"));
    }

    #[test]
    fn shows_everything_when_marker_never_reached() {
        let (out, val) = split_new("crashed early");
        assert_eq!(out, "crashed early");
        assert!(val.is_none());
    }
}
