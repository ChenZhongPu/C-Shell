//! Resident ("Step-DLL") evaluation backend.
//!
//! Instead of rebuilding and rerunning the whole session for every input, a
//! long-lived runner process keeps the session's state. Each input becomes one
//! small shared library exporting a single entry point, which the runner
//! `dlopen`s and calls with a table of heap slots:
//!
//! * a variable lives in a slot; later steps reach it through
//!   `#define x (*cs_pN)` where `cs_pN` is declared by [`VarDecl::pointer_decl`],
//!   so `&x`, `sizeof x` and assignment keep their C meaning;
//! * a function is compiled once under a unique symbol (`cs_fn_f_7`) that a
//!   macro maps `f` to, so redefining it simply binds the name to a new symbol;
//! * types, macros and includes are re-included in every step.
//!
//! The journal of successful steps is only a recovery aid: if the runner dies,
//! times out or is interrupted it is restarted and the journal is replayed
//! silently. Variable-length arrays are rejected (`-Werror=vla`) because their
//! size cannot be recovered from a declaration alone.

use super::{Eval, Evaluator, Outcome, describe_abnormal, split_new};
use crate::codegen::{self, M_DIM, M_DONE, M_NEW, Slot};
use crate::decl::{self, Analysis, FileItem, FuncDef, Plan, VarDecl};
use crate::session::Session;
use crate::{errmap, i18n, lex, proc};
use anyhow::{Context, Result, anyhow};
use std::ffi::{CStr, CString, c_void};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// Number of heap slots the runner can address.
const SLOT_CAP: usize = 1 << 16;

#[cfg(target_os = "macos")]
const SHARED_FLAGS: &[&str] = &[
    "-dynamiclib",
    "-fPIC",
    "-Werror=vla",
    "-Wl,-undefined,dynamic_lookup",
];
#[cfg(not(target_os = "macos"))]
const SHARED_FLAGS: &[&str] = &["-shared", "-fPIC", "-Werror=vla"];

#[cfg(target_os = "macos")]
const LIB_EXT: &str = "dylib";
#[cfg(not(target_os = "macos"))]
const LIB_EXT: &str = "so";

// ---------------------------------------------------------------------------
// Runner process (server side)
// ---------------------------------------------------------------------------

fn write_fd(fd: i32, bytes: &[u8]) {
    let mut rest = bytes;
    while !rest.is_empty() {
        let written = unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) };
        if written <= 0 {
            return;
        }
        rest = &rest[written as usize..];
    }
}

fn dl_error() -> String {
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        "unknown dynamic loader error".to_string()
    } else {
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .replace('\n', " ")
    }
}

/// # Safety
/// `slots` must point at `SLOT_CAP` writable pointers and the library must
/// export `symbol` with the signature `void (void **)`.
unsafe fn call_step(
    path: &str,
    symbol: &str,
    slots: *mut *mut c_void,
) -> std::result::Result<(), String> {
    let path = CString::new(path).map_err(|e| e.to_string())?;
    let symbol = CString::new(symbol).map_err(|e| e.to_string())?;
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        return Err(dl_error());
    }
    let entry = unsafe { libc::dlsym(handle, symbol.as_ptr()) };
    if entry.is_null() {
        return Err(dl_error());
    }
    let step: extern "C" fn(*mut *mut c_void) = unsafe { std::mem::transmute(entry) };
    step(slots);
    Ok(())
}

/// Entry point of `c-shell` when started as a runner (`C_SHELL_RUNNER=1`).
/// Commands arrive on fd 3 and replies leave on fd 4; the step's own output
/// goes to the ordinary stdout/stderr pipes, bracketed by protocol markers.
pub(crate) fn runner_main() -> std::process::ExitCode {
    let mut commands = BufReader::new(unsafe { File::from_raw_fd(3) });
    let mut reply = unsafe { File::from_raw_fd(4) };
    let mut slots: Vec<*mut c_void> = vec![std::ptr::null_mut(); SLOT_CAP];
    let mut line = String::new();
    loop {
        line.clear();
        match commands.read_line(&mut line) {
            Ok(0) | Err(_) => return std::process::ExitCode::SUCCESS,
            Ok(_) => {}
        }
        let mut parts = line.trim_end_matches('\n').splitn(3, '\t');
        if parts.next() != Some("RUN") {
            continue;
        }
        let (Some(path), Some(symbol)) = (parts.next(), parts.next()) else {
            continue;
        };
        write_fd(1, M_NEW.as_bytes());
        write_fd(2, M_NEW.as_bytes());
        let status = unsafe { call_step(path, symbol, slots.as_mut_ptr()) };
        unsafe {
            libc::fflush(std::ptr::null_mut());
        }
        let _ = match status {
            Ok(()) => writeln!(reply, "OK"),
            Err(message) => writeln!(reply, "ERR {}", message.replace('\n', " ")),
        };
        let _ = reply.flush();
        write_fd(1, M_DONE.as_bytes());
        write_fd(2, M_DONE.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Runner process (client side)
// ---------------------------------------------------------------------------

enum Msg {
    Out(Vec<u8>),
    Err(Vec<u8>),
    Reply(Vec<u8>),
}

#[derive(Debug)]
enum End {
    Done,
    Exited(Option<ExitStatus>),
    Timeout,
    Interrupted,
}

struct RunResult {
    out: Vec<u8>,
    err: Vec<u8>,
    reply: String,
    truncated: bool,
    end: End,
}

struct Runner {
    child: Child,
    commands: File,
    rx: Receiver<Msg>,
}

/// A pipe whose descriptors are close-on-exec and well above the stdio range,
/// so duplicating them onto fixed numbers in the child cannot collide.
fn pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut moved = [0; 2];
    for (index, fd) in fds.iter().enumerate() {
        moved[index] = unsafe { libc::fcntl(*fd, libc::F_DUPFD_CLOEXEC, 10) };
        if moved[index] < 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe {
            libc::close(*fd);
        }
    }
    Ok((moved[0], moved[1]))
}

fn pump<R: Read + Send + 'static>(mut source: R, tx: Sender<Msg>, wrap: fn(Vec<u8>) -> Msg) {
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match source.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if tx.send(wrap(buffer[..read].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Append `chunk`, keeping at most `MAX_CAPTURE_BYTES`, and report whether the
/// done marker has now been seen. The marker is looked for across the seam.
fn absorb(buffer: &mut Vec<u8>, chunk: &[u8], seen: &mut bool, truncated: &mut bool) {
    let seam = buffer.len().saturating_sub(M_DONE.len());
    if buffer.len() < proc::MAX_CAPTURE_BYTES {
        buffer.extend_from_slice(chunk);
    } else {
        *truncated = true;
    }
    if !*seen {
        let tail_start = seam.min(buffer.len());
        *seen = contains(&buffer[tail_start..], M_DONE.as_bytes())
            || contains(chunk, M_DONE.as_bytes());
    }
}

impl Runner {
    fn spawn() -> Result<Self> {
        let exe = std::env::current_exe().context("cannot locate the c-shell executable")?;
        let (commands_read, commands_write) = pipe()?;
        let (reply_read, reply_write) = pipe()?;
        let mut command = Command::new(exe);
        command
            .env("C_SHELL_RUNNER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(commands_read, 3) < 0 || libc::dup2(reply_write, 4) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let spawned = command.spawn();
        unsafe {
            libc::close(commands_read);
            libc::close(reply_write);
        }
        let mut child = spawned.context("cannot start the resident runner")?;
        let (tx, rx) = mpsc::channel();
        pump(child.stdout.take().expect("piped"), tx.clone(), Msg::Out);
        pump(child.stderr.take().expect("piped"), tx.clone(), Msg::Err);
        pump(unsafe { File::from_raw_fd(reply_read) }, tx, Msg::Reply);
        Ok(Runner {
            child,
            commands: unsafe { File::from_raw_fd(commands_write) },
            rx,
        })
    }

    fn kill(&mut self) {
        unsafe {
            libc::killpg(self.child.id() as i32, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }

    fn run(&mut self, library: &Path, symbol: &str, timeout: Duration) -> RunResult {
        proc::install_interrupt_handler();
        let mut result = RunResult {
            out: Vec::new(),
            err: Vec::new(),
            reply: String::new(),
            truncated: false,
            end: End::Done,
        };
        let command = format!("RUN\t{}\t{symbol}\n", library.display());
        if self.commands.write_all(command.as_bytes()).is_err() {
            result.end = End::Exited(self.child.wait().ok());
            return result;
        }
        let (mut out_done, mut err_done) = (false, false);
        let start = Instant::now();
        loop {
            if proc::interrupted() {
                self.kill();
                result.end = End::Interrupted;
                return result;
            }
            if start.elapsed() >= timeout {
                self.kill();
                result.end = End::Timeout;
                return result;
            }
            match self.rx.recv_timeout(Duration::from_millis(10)) {
                Ok(Msg::Out(chunk)) => absorb(
                    &mut result.out,
                    &chunk,
                    &mut out_done,
                    &mut result.truncated,
                ),
                Ok(Msg::Err(chunk)) => absorb(
                    &mut result.err,
                    &chunk,
                    &mut err_done,
                    &mut result.truncated,
                ),
                Ok(Msg::Reply(chunk)) => result.reply.push_str(&String::from_utf8_lossy(&chunk)),
                Err(RecvTimeoutError::Timeout) => {
                    if let Ok(Some(status)) = self.child.try_wait() {
                        self.drain(&mut result);
                        result.end = End::Exited(Some(status));
                        return result;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    result.end = End::Exited(self.child.wait().ok());
                    return result;
                }
            }
            if out_done && err_done && result.reply.contains('\n') {
                return result;
            }
        }
    }

    /// Collect whatever a dead runner had already written.
    fn drain(&mut self, result: &mut RunResult) {
        while let Ok(message) = self.rx.recv_timeout(Duration::from_millis(50)) {
            match message {
                Msg::Out(chunk) => result.out.extend_from_slice(&chunk),
                Msg::Err(chunk) => result.err.extend_from_slice(&chunk),
                Msg::Reply(chunk) => result.reply.push_str(&String::from_utf8_lossy(&chunk)),
            }
        }
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.kill();
    }
}

// ---------------------------------------------------------------------------
// Session state and step generation
// ---------------------------------------------------------------------------

struct Var {
    decl: VarDecl,
    slot: usize,
    /// Element count of an inferred outermost array dimension.
    dim: Option<usize>,
}

struct Func {
    name: String,
    symbol: String,
    proto: String,
}

struct Library {
    path: PathBuf,
    symbol: String,
}

enum Wrapper {
    Print,
    Lvalue,
    Scalar,
    Array(usize),
}

enum Body<'a> {
    Empty,
    Vars(&'a [VarDecl]),
    Stmt(&'a str),
    Expr(&'a str, Wrapper),
}

struct Assembled {
    src: String,
    new_start_line: usize,
    new_line_count: usize,
    ranges: Vec<(usize, usize)>,
    wrapped: bool,
}

#[derive(Default)]
pub struct Resident {
    runner: Option<Runner>,
    items: Vec<FileItem>,
    vars: Vec<Var>,
    funcs: Vec<Func>,
    next_slot: usize,
    next_step: usize,
    journal: Vec<Library>,
}

fn line_of(src: &str) -> usize {
    src.bytes().filter(|byte| *byte == b'\n').count() + 1
}

/// Replacements and additions a successful step makes to the session.
struct Delta {
    remove_items: Vec<usize>,
    items: Vec<FileItem>,
    funcs: Vec<(FuncDef, String)>,
    vars: Vec<VarDecl>,
    first_slot: usize,
}

struct StepRun {
    outcome: Outcome,
    library: Library,
    dims: Vec<(usize, usize)>,
}

type Attempt = std::result::Result<StepRun, String>;

impl Resident {
    pub fn reset(&mut self) {
        self.runner = None;
        self.items.clear();
        self.vars.clear();
        self.funcs.clear();
        self.journal.clear();
        self.next_slot = 0;
    }

    fn type_in_use(&self, tag: &str, skip: usize) -> bool {
        let name = tag.rsplit(' ').next().unwrap_or(tag);
        let mentions = |text: &str| lex::identifiers(text).iter().any(|id| id == name);
        self.vars
            .iter()
            .any(|v| mentions(&v.decl.declspec) || mentions(&v.decl.declarator))
            || self.funcs.iter().any(|f| mentions(&f.proto))
            || self
                .items
                .iter()
                .enumerate()
                .any(|(index, item)| index != skip && mentions(&item.text))
    }

    /// Old items a new item replaces, or the type name that is still in use.
    fn replaced_items(&self, new_items: &[FileItem]) -> std::result::Result<Vec<usize>, String> {
        let mut remove = Vec::new();
        for item in new_items {
            for (index, old) in self.items.iter().enumerate() {
                let types: Vec<&String> = item
                    .types
                    .iter()
                    .filter(|t| old.types.contains(t))
                    .collect();
                let macros = item.defines.iter().any(|d| old.defines.contains(d));
                if types.is_empty() && !macros {
                    continue;
                }
                if let Some(tag) = types.first()
                    && self.type_in_use(tag, index)
                {
                    return Err((*tag).clone());
                }
                remove.push(index);
            }
        }
        remove.sort_unstable();
        remove.dedup();
        Ok(remove)
    }

    fn assemble(&self, step: usize, delta: &Delta, body: &Body, redefined: &[&str]) -> Assembled {
        let mut src = codegen::prelude(false, false);
        let mut ranges = Vec::new();
        let mut new_start_line = 1;
        let mut new_line_count = 1;
        let kept = self
            .items
            .iter()
            .enumerate()
            .filter(|(index, _)| !delta.remove_items.contains(index))
            .map(|(_, item)| item);
        for item in kept {
            ranges.push((line_of(&src), item.text.lines().count().max(1)));
            src.push_str(&item.text);
            src.push('\n');
        }
        for item in &delta.items {
            new_start_line = line_of(&src);
            new_line_count = item.text.lines().count().max(1);
            src.push_str(&item.text);
            src.push('\n');
        }
        for func in self
            .funcs
            .iter()
            .filter(|f| !redefined.contains(&f.name.as_str()))
        {
            src.push_str(&format!(
                "#define {} {}\n{};\n",
                func.name, func.symbol, func.proto
            ));
        }
        for (func, symbol) in &delta.funcs {
            src.push_str(&format!("#define {} {symbol}\n", func.name));
            new_start_line = line_of(&src);
            new_line_count = func.text.lines().count().max(1);
            src.push_str(&func.text);
            src.push('\n');
        }
        let printers = codegen::build_aggregate_printers(&src);
        src.push_str(&printers);
        src.push_str(&format!("\nvoid cs_step_{step}(void **cs_slot)\n{{\n"));
        for var in self
            .vars
            .iter()
            .filter(|v| !delta.vars.iter().any(|d| d.name == v.decl.name))
        {
            let pointer = format!("cs_p{}", var.slot);
            src.push_str(&format!(
                "    {} = cs_slot[{}];\n#define {} (*{pointer})\n",
                var.decl.pointer_decl(&pointer, var.dim),
                var.slot,
                var.decl.name
            ));
        }
        let mut wrapped = false;
        match body {
            Body::Empty => {}
            Body::Stmt(input) => {
                new_start_line = line_of(&src);
                new_line_count = input.lines().count().max(1);
                src.push_str(input);
                src.push('\n');
            }
            Body::Expr(input, wrapper) => {
                let open = match wrapper {
                    Wrapper::Print => "CS_PRINT".to_string(),
                    Wrapper::Lvalue => "CS_PRINT_LVALUE".to_string(),
                    Wrapper::Scalar => "CS_PRINT_SCALAR".to_string(),
                    Wrapper::Array(depth) => format!("CS_PRINT_ARRAY{depth}"),
                };
                src.push_str(&format!("    {open}((\n"));
                new_start_line = line_of(&src);
                new_line_count = input.lines().count().max(1);
                src.push_str(input);
                src.push_str("\n    ));\n");
                wrapped = true;
            }
            Body::Vars(vars) => {
                for var in vars.iter() {
                    src.push_str(&format!("#undef {}\n", var.name));
                }
                new_start_line = line_of(&src);
                // Declarations come first and contiguously: they are the only
                // lines that hold user text. Earlier declarators stay in scope
                // as locals for later ones, so no block is needed.
                for var in vars.iter() {
                    src.push_str(&format!("{} {};\n", var.declspec, var.init));
                }
                new_line_count = (line_of(&src) - new_start_line).max(1);
                for (offset, var) in vars.iter().enumerate() {
                    let slot = delta.first_slot + offset;
                    let name = &var.name;
                    src.push_str(&format!(
                        "    cs_slot[{slot}] = malloc(sizeof({name}));\n    if (!cs_slot[{slot}]) abort();\n    memcpy(cs_slot[{slot}], &{name}, sizeof({name}));\n"
                    ));
                    if var.dim_insert.is_some() {
                        src.push_str(&format!(
                            "    fputs(CS_M_DIM, stdout); printf(\"{slot}:%zu\\n\", sizeof({name}) / sizeof(({name})[0]));\n"
                        ));
                    }
                    let pointer = format!("cs_p{slot}");
                    src.push_str(&format!(
                        "    {} = cs_slot[{slot}];\n#define {name} (*{pointer})\n",
                        var.pointer_decl(&pointer, None)
                    ));
                }
            }
        }
        src.push_str("}\n");
        Assembled {
            src,
            new_start_line,
            new_line_count,
            ranges,
            wrapped,
        }
    }

    fn compile(
        &self,
        ev: &Evaluator,
        step: usize,
        assembled: &Assembled,
    ) -> std::result::Result<(PathBuf, String), String> {
        if proc::interrupted() {
            return Err(i18n::text("program-interrupted"));
        }
        let source = ev.dir.path().join(format!("step_{step}.c"));
        let library = ev.dir.path().join(format!("step_{step}.{LIB_EXT}"));
        if let Err(error) = std::fs::write(&source, &assembled.src) {
            return Err(i18n::text_with(
                "temp-source-write-error",
                &[("error", error.to_string())],
            ));
        }
        let mut args = ev.tc.compile_args(&source, &library);
        args.splice(0..0, SHARED_FLAGS.iter().map(|flag| flag.to_string()));
        let mut command = Command::new(&ev.tc.path);
        command.current_dir(ev.dir.path()).args(&args);
        let captured = proc::run_captured(&mut command, ev.timeout, false).map_err(|error| {
            i18n::text_with("compiler-run-error", &[("error", error.to_string())])
        })?;
        let mut diagnostics = String::from_utf8_lossy(&captured.stderr).into_owned();
        diagnostics.push_str(&String::from_utf8_lossy(&captured.stdout));
        if captured.interrupted {
            return Err(i18n::text("program-interrupted"));
        }
        let path = source.display().to_string();
        let remap = |text: &str| {
            errmap::remap(
                text,
                &path,
                &assembled.src,
                assembled.new_start_line,
                assembled.new_line_count,
                &assembled.ranges,
                assembled.wrapped,
            )
        };
        match captured.status {
            Some(status) if status.success() => {
                Ok((library, demangle(&errmap::only_new(&remap(&diagnostics)))))
            }
            Some(_) => Err(demangle(&errmap::drop_stale_warnings(&remap(&diagnostics)))),
            None => Err(i18n::text_with(
                "compiler-timeout",
                &[("seconds", ev.timeout.as_secs().to_string())],
            )),
        }
    }

    fn ensure_runner(&mut self) -> Result<&mut Runner> {
        if self.runner.is_none() {
            self.runner = Some(Runner::spawn()?);
        }
        Ok(self.runner.as_mut().expect("runner was just created"))
    }

    /// Restart the runner and silently replay every journaled step. Side
    /// effects of those steps repeat; this is recovery, not the normal path.
    fn recover(&mut self, timeout: Duration) {
        self.runner = None;
        let Ok(mut runner) = Runner::spawn() else {
            self.reset();
            return;
        };
        for library in &self.journal {
            let result = runner.run(&library.path, &library.symbol, timeout);
            if !matches!(result.end, End::Done) || !result.reply.starts_with("OK") {
                self.runner = None;
                self.reset();
                return;
            }
        }
        self.runner = Some(runner);
    }

    /// Compile one step, run it in the runner, and report the raw result.
    fn step(&mut self, ev: &Evaluator, delta: &Delta, body: &Body, slot: Slot) -> Result<Attempt> {
        let redefined: Vec<&str> = delta.funcs.iter().map(|(f, _)| f.name.as_str()).collect();
        let number = self.next_step;
        self.next_step += 1;
        let assembled = self.assemble(number, delta, body, &redefined);
        let (path, warnings) = match self.compile(ev, number, &assembled) {
            Ok(compiled) => compiled,
            Err(diagnostics) => return Ok(Err(diagnostics)),
        };
        let symbol = format!("cs_step_{number}");
        let started = Instant::now();
        let timeout = ev.timeout;
        let result = self.ensure_runner()?.run(&path, &symbol, timeout);
        let duration = started.elapsed();
        let library = Library { path, symbol };

        let out = String::from_utf8_lossy(&result.out).into_owned();
        let err = String::from_utf8_lossy(&result.err).into_owned();
        let out_parts = split_new(&out, true);
        let err_parts = split_new(&err, false);
        let (output, dims) = take_dims(&out_parts.output);
        let abnormal = match &result.end {
            End::Interrupted => Some(i18n::text("program-interrupted")),
            End::Timeout => Some(i18n::text_with(
                "program-killed",
                &[("seconds", ev.timeout.as_secs().to_string())],
            )),
            End::Exited(status) => status
                .as_ref()
                .and_then(describe_abnormal)
                .or_else(|| Some(i18n::text("program-exited-early"))),
            End::Done if result.reply.starts_with("ERR") => Some(i18n::text_with(
                "resident-runner-error",
                &[(
                    "error",
                    result.reply.trim_start_matches("ERR").trim().to_string(),
                )],
            )),
            End::Done if result.truncated => Some(i18n::text_with(
                "program-output-truncated",
                &[("mib", (proc::MAX_CAPTURE_BYTES / (1024 * 1024)).to_string())],
            )),
            End::Done => None,
        };
        if !matches!(result.end, End::Done) {
            self.recover(ev.timeout);
        }
        let outcome = Outcome {
            slot,
            output,
            value: out_parts
                .value
                .filter(|_| abnormal.is_none() && slot == Slot::Expr),
            errors: err_parts.output,
            runtime_report: None,
            streamed_output_needs_newline: false,
            stdin_events: Vec::new(),
            warnings,
            abnormal,
            rewritten: None,
            scoped_rebind: false,
            file_replacement: None,
            unprintable: false,
            duration,
            timed_duration: None,
        };
        Ok(Ok(StepRun {
            outcome,
            library,
            dims,
        }))
    }

    fn commit(&mut self, delta: Delta, run: &StepRun, journal: bool) {
        for index in delta.remove_items.iter().rev() {
            self.items.remove(*index);
        }
        self.items.extend(delta.items);
        for (func, symbol) in delta.funcs {
            self.funcs.retain(|f| f.name != func.name);
            self.funcs.push(Func {
                name: func.name,
                symbol,
                proto: func.proto,
            });
        }
        for (offset, decl) in delta.vars.into_iter().enumerate() {
            let slot = delta.first_slot + offset;
            self.vars.retain(|v| v.decl.name != decl.name);
            let dim = run
                .dims
                .iter()
                .find(|(s, _)| *s == slot)
                .map(|(_, count)| *count);
            self.vars.push(Var { decl, slot, dim });
        }
        if journal {
            self.journal.push(Library {
                path: run.library.path.clone(),
                symbol: run.library.symbol.clone(),
            });
        }
    }

    pub fn eval(&mut self, ev: &Evaluator, session: &Session, input: &str) -> Result<Eval> {
        let trimmed = input.trim();
        let mut plan_error = None;
        match decl::analyze(trimmed) {
            Analysis::Unsupported(reason) => {
                return Ok(Eval::CompileError(i18n::text_with(
                    "resident-unsupported",
                    &[("reason", reason)],
                )));
            }
            Analysis::Plan(plan) => {
                let has_vars = !plan.vars.is_empty();
                match self.eval_plan(ev, trimmed, plan)? {
                    Ok(outcome) => return Ok(Eval::Done(Box::new(outcome))),
                    Err(diagnostics) if has_vars => plan_error = Some(diagnostics),
                    Err(diagnostics) => return Ok(Eval::CompileError(diagnostics)),
                }
            }
            Analysis::Statement => {}
        }
        match self.eval_text(ev, session, trimmed)? {
            Ok(outcome) => Ok(Eval::Done(Box::new(outcome))),
            Err(diagnostics) => Ok(Eval::CompileError(plan_error.unwrap_or(diagnostics))),
        }
    }

    fn eval_plan(
        &mut self,
        ev: &Evaluator,
        input: &str,
        plan: Plan,
    ) -> Result<std::result::Result<Outcome, String>> {
        let remove_items = match self.replaced_items(&plan.items) {
            Ok(indices) => indices,
            Err(tag) => {
                let name = tag.rsplit(' ').next().unwrap_or(&tag).to_string();
                return Ok(Err(i18n::text_with(
                    "resident-type-in-use",
                    &[("name", name)],
                )));
            }
        };
        if self.next_slot + plan.vars.len() >= SLOT_CAP {
            return Ok(Err(i18n::text("resident-slots-exhausted")));
        }
        let number = self.next_step;
        let funcs: Vec<(FuncDef, String)> = plan
            .funcs
            .iter()
            .map(|f| (f.clone(), format!("cs_fn_{}_{number}", f.name)))
            .collect();
        let delta = Delta {
            remove_items,
            items: plan.items.clone(),
            funcs,
            vars: plan.vars.clone(),
            first_slot: self.next_slot,
        };
        let body = if plan.vars.is_empty() {
            Body::Empty
        } else {
            Body::Vars(&plan.vars)
        };
        let slot = if plan.vars.is_empty() {
            Slot::FileScope
        } else {
            Slot::Stmt
        };
        let mut run = match self.step(ev, &delta, &body, slot)? {
            Ok(run) => run,
            Err(diagnostics) => return Ok(Err(diagnostics)),
        };
        if run.outcome.abnormal.is_none() {
            self.next_slot += plan.vars.len();
            self.commit(delta, &run, true);
            if plan.rewritten {
                run.outcome.rewritten = Some(format!("{input};"));
            }
        }
        Ok(Ok(run.outcome))
    }

    /// Compile and run `body` against the current state, committing nothing.
    fn attempt(&mut self, ev: &Evaluator, body: &Body, slot: Slot) -> Result<Attempt> {
        let delta = Delta {
            remove_items: Vec::new(),
            items: Vec::new(),
            funcs: Vec::new(),
            vars: Vec::new(),
            first_slot: self.next_slot,
        };
        self.step(ev, &delta, body, slot)
    }

    fn eval_expression(
        &mut self,
        ev: &Evaluator,
        session: &Session,
        input: &str,
    ) -> Result<Attempt> {
        let impure = session.must_replay(input);
        let mut wrappers = Vec::new();
        if impure && lex::may_be_array_lvalue(input) {
            wrappers.push(Wrapper::Lvalue);
        }
        wrappers.push(Wrapper::Print);
        let mut failure = String::new();
        for wrapper in wrappers {
            match self.attempt(ev, &Body::Expr(input, wrapper), Slot::Expr)? {
                Ok(mut run) => {
                    // A bare address may be a decayed array. Only a pure
                    // expression can safely run again to find out.
                    let addressed = run
                        .outcome
                        .value
                        .as_deref()
                        .is_some_and(|value| value.starts_with("0x"));
                    if !impure && addressed {
                        for depth in 1..=codegen::MAX_ARRAY_DEPTH {
                            let Ok(candidate) = self.attempt(
                                ev,
                                &Body::Expr(input, Wrapper::Array(depth)),
                                Slot::Expr,
                            )?
                            else {
                                break;
                            };
                            let Some(value) = candidate
                                .outcome
                                .value
                                .clone()
                                .filter(|value| value.starts_with('{'))
                            else {
                                break;
                            };
                            let unprintable = value.contains(codegen::UNPRINTABLE);
                            run = candidate;
                            if !unprintable {
                                break;
                            }
                        }
                    }
                    return Ok(Ok(run));
                }
                Err(diagnostics) => failure = diagnostics,
            }
        }
        // GNU `typeof` rejects bit-fields; a numeric-only wrapper still works.
        if let Ok(run) = self.attempt(ev, &Body::Expr(input, Wrapper::Scalar), Slot::Expr)? {
            return Ok(Ok(run));
        }
        Ok(Err(failure))
    }

    fn eval_once(
        &mut self,
        ev: &Evaluator,
        session: &Session,
        input: &str,
    ) -> Result<std::result::Result<Outcome, String>> {
        let mut reported = String::new();
        if super::should_try_expr(input) {
            match self.eval_expression(ev, session, input)? {
                Ok(run) => return Ok(Ok(self.finish(session, input, run))),
                Err(diagnostics) => reported = diagnostics,
            }
        }
        match self.attempt(ev, &Body::Stmt(input), Slot::Stmt)? {
            Ok(run) => Ok(Ok(self.finish(session, input, run))),
            Err(diagnostics) => Ok(Err(if diagnostics.is_empty() {
                reported
            } else {
                diagnostics
            })),
        }
    }

    /// Statements always change state; an expression only when it might.
    fn finish(&mut self, session: &Session, input: &str, run: StepRun) -> Outcome {
        let journal = run.outcome.abnormal.is_none()
            && (run.outcome.slot == Slot::Stmt || session.must_replay(input));
        if journal {
            self.journal.push(Library {
                path: run.library.path.clone(),
                symbol: run.library.symbol.clone(),
            });
        }
        run.outcome
    }

    fn eval_text(
        &mut self,
        ev: &Evaluator,
        session: &Session,
        input: &str,
    ) -> Result<std::result::Result<Outcome, String>> {
        let first = match self.eval_once(ev, session, input)? {
            Ok(outcome) => return Ok(Ok(outcome)),
            Err(diagnostics) => diagnostics,
        };
        let repairable = !input.ends_with(';') && !input.trim_start().starts_with('#');
        if repairable {
            let patched = format!("{input};");
            if let Ok(mut outcome) = self.eval_once(ev, session, &patched)? {
                outcome.rewritten = Some(patched);
                return Ok(Ok(outcome));
            }
        }
        Ok(Err(first))
    }
}

/// Show the names the user typed instead of the generated step entry point
/// (`cs_step_7` reads as `main`, matching replay mode) and function symbols
/// (`cs_fn_add_3` reads as `add`).
fn demangle(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("cs_") {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(tail.len());
        let token = &tail[..end];
        let replacement = if token.starts_with("cs_step_")
            && token["cs_step_".len()..]
                .bytes()
                .all(|b| b.is_ascii_digit())
        {
            Some("main".to_string())
        } else {
            token
                .strip_prefix("cs_fn_")
                .and_then(|name| name.rsplit_once('_'))
                .filter(|(_, version)| version.bytes().all(|b| b.is_ascii_digit()))
                .map(|(name, _)| name.to_string())
        };
        out.push_str(replacement.as_deref().unwrap_or(token));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Split `slot:count` lines (introduced by [`M_DIM`]) out of program output.
fn take_dims(output: &str) -> (String, Vec<(usize, usize)>) {
    let mut dims = Vec::new();
    let mut clean = String::new();
    let mut rest = output;
    while let Some(at) = rest.find(M_DIM) {
        clean.push_str(&rest[..at]);
        let tail = &rest[at + M_DIM.len()..];
        let end = tail.find('\n').map_or(tail.len(), |i| i + 1);
        if let Some((slot, count)) = tail[..end].trim_end().split_once(':')
            && let (Ok(slot), Ok(count)) = (slot.parse(), count.parse())
        {
            dims.push((slot, count));
        }
        rest = &tail[end..];
    }
    clean.push_str(rest);
    (clean, dims)
}

impl Resident {
    pub fn new() -> Result<Self> {
        let mut resident = Self::default();
        // Fail early, with a useful message, if the runner cannot start.
        resident.ensure_runner().map(|_| ()).map_err(|error| {
            anyhow!(i18n::text_with(
                "resident-runner-error",
                &[("error", format!("{error:#}"))]
            ))
        })?;
        Ok(resident)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_names_are_replaced_by_what_the_user_typed() {
        assert_eq!(
            demangle("<input>: In function ‘cs_step_12’:\n"),
            "<input>: In function ‘main’:\n"
        );
        assert_eq!(
            demangle("conflicting types for ‘cs_fn_my_func_3’"),
            "conflicting types for ‘my_func’"
        );
        assert_eq!(demangle("cs_other and cs_step_x"), "cs_other and cs_step_x");
    }

    #[test]
    fn inferred_array_dimensions_are_split_out_of_program_output() {
        let text = format!("before\n{M_DIM}3:12\nafter {M_DIM}7:2\n");
        let (clean, dims) = take_dims(&text);
        assert_eq!(clean, "before\nafter ");
        assert_eq!(dims, vec![(3, 12), (7, 2)]);
    }
}
