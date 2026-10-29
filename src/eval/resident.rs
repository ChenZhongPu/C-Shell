//! Resident ("Step-DLL") evaluation backend.
//!
//! Instead of rebuilding and rerunning the whole session for every input, a
//! long-lived runner process keeps the session's state. Each input becomes one
//! small shared library exporting an entry point, which the runner loads and
//! calls with a table of heap slots:
//!
//! * a variable lives in a slot; later steps reach it through `(*cs_pN)`.
//!   References are bound after preprocessing, respecting C scopes and members,
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
use anyhow::Result;
use std::ffi::c_void;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

mod bindings;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

/// Number of heap slots the runner can address. The final slot is reserved
/// for a per-run argument and never holds a variable.
const SLOT_CAP: usize = 1 << 16;

#[cfg(target_os = "macos")]
const SHARED_FLAGS: &[&str] = &[
    "-dynamiclib",
    "-fPIC",
    "-Werror=vla",
    "-Wl,-undefined,dynamic_lookup",
];
#[cfg(all(unix, not(target_os = "macos")))]
const SHARED_FLAGS: &[&str] = &["-shared", "-fPIC", "-Werror=vla"];

#[cfg(target_os = "macos")]
const LIB_EXT: &str = "dylib";
#[cfg(all(unix, not(target_os = "macos")))]
const LIB_EXT: &str = "so";

#[cfg(windows)]
const SHARED_FLAGS: &[&str] = &["-shared", "-Werror=vla", "-Wl,--exclude-all-symbols"];
#[cfg(windows)]
const LIB_EXT: &str = "dll";

// ---------------------------------------------------------------------------
// Runner process (server side)
// ---------------------------------------------------------------------------

/// Entry point of `c-shell` when started as a runner (`C_SHELL_RUNNER=1`).
/// The platform transport keeps commands and replies separate from the step's
/// stdout/stderr, which are bracketed by protocol markers.
pub(crate) fn runner_main() -> std::process::ExitCode {
    platform::runner_main()
}

fn serve(mut commands: impl BufRead, mut reply: impl Write) -> std::process::ExitCode {
    let mut slots: Vec<*mut c_void> = vec![std::ptr::null_mut(); SLOT_CAP];
    let mut line = String::new();
    loop {
        line.clear();
        match commands.read_line(&mut line) {
            Ok(0) | Err(_) => return std::process::ExitCode::SUCCESS,
            Ok(_) => {}
        }
        let mut parts = line.trim_end_matches('\n').splitn(4, '\t');
        if parts.next() != Some("RUN") {
            continue;
        }
        let (Some(path), Some(symbol)) = (parts.next(), parts.next()) else {
            continue;
        };
        // The last slot carries a per-run argument (the `%timeit` loop count).
        slots[SLOT_CAP - 1] = parts
            .next()
            .and_then(|arg| arg.parse::<usize>().ok())
            .unwrap_or(0) as *mut c_void;
        platform::write_output(1, M_NEW.as_bytes());
        platform::write_output(2, M_NEW.as_bytes());
        let status = unsafe { platform::call_step(path, symbol, slots.as_mut_ptr()) };
        let _ = match status {
            Ok(()) => writeln!(reply, "OK"),
            Err(message) => writeln!(reply, "ERR {}", message.replace('\n', " ")),
        };
        let _ = reply.flush();
        platform::write_output(1, M_DONE.as_bytes());
        platform::write_output(2, M_DONE.as_bytes());
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
    commands: platform::Commands,
    #[cfg(windows)]
    job: crate::proc::windows::Job,
    rx: Receiver<Msg>,
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
fn absorb(
    buffer: &mut Vec<u8>,
    chunk: &[u8],
    seen: &mut bool,
    truncated: &mut bool,
    tail: &mut Vec<u8>,
) {
    let left = proc::MAX_CAPTURE_BYTES.saturating_sub(buffer.len());
    buffer.extend_from_slice(&chunk[..chunk.len().min(left)]);
    *truncated |= chunk.len() > left;
    if !*seen {
        tail.extend_from_slice(chunk);
        *seen = contains(tail, M_DONE.as_bytes());
        let keep = tail.len().saturating_sub(M_DONE.len() - 1);
        tail.drain(..keep);
    }
}

impl Runner {
    fn spawn(ev: &Evaluator) -> Result<Self> {
        platform::spawn(&ev.tc, ev.timeout)
    }

    fn kill(&mut self) {
        platform::kill(self);
    }

    fn run(&mut self, library: &Path, symbol: &str, arg: u64, timeout: Duration) -> RunResult {
        proc::install_interrupt_handler();
        let mut result = RunResult {
            out: Vec::new(),
            err: Vec::new(),
            reply: String::new(),
            truncated: false,
            end: End::Done,
        };
        let command = format!("RUN\t{}\t{symbol}\t{arg}\n", library.display());
        if self.commands.write_all(command.as_bytes()).is_err() {
            result.end = End::Exited(self.child.wait().ok());
            return result;
        }
        let (mut out_done, mut err_done) = (false, false);
        let (mut out_tail, mut err_tail) = (Vec::new(), Vec::new());
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
                    &mut out_tail,
                ),
                Ok(Msg::Err(chunk)) => absorb(
                    &mut result.err,
                    &chunk,
                    &mut err_done,
                    &mut result.truncated,
                    &mut err_tail,
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
        let mut done = false;
        let mut tail = Vec::new();
        while let Ok(message) = self.rx.recv_timeout(Duration::from_millis(50)) {
            match message {
                Msg::Out(chunk) => absorb(
                    &mut result.out,
                    &chunk,
                    &mut done,
                    &mut result.truncated,
                    &mut tail,
                ),
                Msg::Err(chunk) => absorb(
                    &mut result.err,
                    &chunk,
                    &mut done,
                    &mut result.truncated,
                    &mut tail,
                ),
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
    /// Import library for the DLL that owns this function version.
    #[cfg(windows)]
    import_library: PathBuf,
}

struct Library {
    path: PathBuf,
    symbol: String,
}

struct Compiled {
    library: Library,
    warnings: String,
}

enum Wrapper {
    Print,
    Lvalue,
    Scalar,
    Array(usize),
}

enum Body<'a> {
    Empty,
    /// A probe: `before`, the user's expression on its own line, `after`.
    Wrapped {
        before: &'a str,
        input: &'a str,
        after: &'a str,
        bits: bool,
    },
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
    bindings: Vec<(String, String)>,
    #[cfg(windows)]
    exports: Vec<String>,
    #[cfg(windows)]
    imports: Vec<PathBuf>,
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
    /// `%time` is running: wrap the input in the single-input timer.
    timed: bool,
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

    /// Stop the single-input timer and report it, when `%time` is active.
    fn timer_end(&self) -> String {
        if self.timed {
            "    cs_time_once_stop();\n    printf(\"%s%llu\\n\", CS_M_TIME, (unsigned long long)cs_time_once_elapsed_ns);\n".to_string()
        } else {
            String::new()
        }
    }

    fn assemble(&self, step: usize, delta: &Delta, body: &Body, redefined: &[&str]) -> Assembled {
        let bits = matches!(body, Body::Wrapped { bits: true, .. });
        let mut src = codegen::prelude(bits, self.timed);
        let mut ranges = Vec::new();
        let mut new_start_line = 1;
        let mut new_line_count = 1;
        let mut bindings = Vec::new();
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
            #[cfg(unix)]
            src.push_str(&format!(
                "#define {} {}\n{};\n",
                func.name, func.symbol, func.proto
            ));
            #[cfg(windows)]
            src.push_str(&format!(
                "#define {} {}\n__declspec(dllimport) {};\n",
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
        #[cfg(windows)]
        src.push_str(&format!(
            "\nvoid cs_flush_{step}(void) {{ (fflush)(NULL); }}\n"
        ));
        src.push_str(&format!("\nvoid cs_step_{step}(void **cs_slot)\n{{\n"));
        for var in &self.vars {
            let pointer = format!("cs_p{}", var.slot);
            src.push_str(&format!(
                "    {} = cs_slot[{}];\n",
                var.decl.pointer_decl(&pointer, var.dim),
                var.slot,
            ));
            bindings.push((pointer, var.decl.name.clone()));
        }
        let mut wrapped = false;
        match body {
            Body::Empty => {}
            Body::Wrapped {
                before,
                input,
                after,
                ..
            } => {
                src.push_str(before);
                new_start_line = line_of(&src);
                new_line_count = input.lines().count().max(1);
                src.push_str(input);
                src.push_str(after);
                wrapped = true;
            }
            Body::Stmt(input) => {
                if self.timed {
                    src.push_str("    cs_time_once_begin();\n");
                }
                new_start_line = line_of(&src);
                new_line_count = input.lines().count().max(1);
                src.push_str(input);
                src.push('\n');
                src.push_str(&self.timer_end());
            }
            Body::Expr(input, wrapper) => {
                let open = match wrapper {
                    Wrapper::Print => "CS_PRINT".to_string(),
                    Wrapper::Lvalue => "CS_PRINT_LVALUE".to_string(),
                    Wrapper::Scalar => "CS_PRINT_SCALAR".to_string(),
                    Wrapper::Array(depth) => format!("CS_PRINT_ARRAY{depth}"),
                };
                if self.timed {
                    src.push_str("    cs_time_once_begin();\n");
                }
                src.push_str(&format!("    {open}((\n"));
                new_start_line = line_of(&src);
                new_line_count = input.lines().count().max(1);
                src.push_str(input);
                src.push_str("\n    ));\n");
                src.push_str(&self.timer_end());
                wrapped = true;
            }
            Body::Vars(vars) => {
                new_start_line = line_of(&src);
                if self.timed {
                    src.push_str("    cs_time_once_begin();\n");
                }
                for (offset, var) in vars.iter().enumerate() {
                    let slot = delta.first_slot + offset;
                    let pointer = format!("cs_p{slot}");
                    // Allocate before evaluating the initializer: references
                    // to this object (including &self) must use its final address.
                    src.push_str(&format!(
                        "    {};\n    cs_slot[{slot}] = malloc(sizeof(*{pointer}));\n    if (!cs_slot[{slot}]) abort();\n    {pointer} = cs_slot[{slot}];\n",
                        var.storage_pointer_decl(&pointer)
                    ));
                    bindings.push((pointer.clone(), var.name.clone()));
                    if var.initializer().is_some() {
                        let init = format!("cs_init{slot}");
                        src.push_str(&format!(
                            "    {};\n    memcpy((void *){pointer}, &{init}, sizeof({init}));\n",
                            var.init_decl(&init)
                        ));
                    } else {
                        src.push_str(&format!(
                            "    memset((void *){pointer}, 0, sizeof(*{pointer}));\n"
                        ));
                    }
                    if var.dim_insert.is_some() {
                        src.push_str(&format!(
                            "    fputs(CS_M_DIM, stdout); printf(\"{slot}:%zu\\n\", sizeof(*{pointer}) / sizeof((*{pointer})[0]));\n"
                        ));
                    }
                }
                new_line_count = (line_of(&src) - new_start_line).max(1);
                src.push_str(&self.timer_end());
            }
        }
        src.push_str("}\n");
        Assembled {
            src,
            new_start_line,
            new_line_count,
            ranges,
            wrapped,
            bindings,
            #[cfg(windows)]
            exports: delta
                .funcs
                .iter()
                .map(|(_, symbol)| symbol.clone())
                .collect(),
            #[cfg(windows)]
            imports: self
                .funcs
                .iter()
                .filter(|func| !redefined.contains(&func.name.as_str()))
                .map(|func| func.import_library.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
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
        let preprocessed = ev.dir.path().join(format!("step_{step}.i"));
        let library = ev.dir.path().join(format!("step_{step}.{LIB_EXT}"));
        if let Err(error) = std::fs::write(&source, &assembled.src) {
            return Err(i18n::text_with(
                "temp-source-write-error",
                &[("error", error.to_string())],
            ));
        }
        // Bind after the real preprocessor has expanded user/header macros.
        // Preserve line directives so compiler diagnostics still map to input.
        let mut pp_args = ev.tc.compile_args(&source, &preprocessed);
        pp_args.retain(|arg| arg != "-lm");
        pp_args.insert(0, "-E".to_string());
        #[cfg(unix)]
        pp_args.insert(0, "-fPIC".to_string());
        let mut command = Command::new(&ev.tc.path);
        command.current_dir(ev.dir.path()).args(&pp_args);
        let pp = proc::run_captured(&mut command, ev.timeout, false).map_err(|error| {
            i18n::text_with("compiler-run-error", &[("error", error.to_string())])
        })?;
        let pp_diagnostics = String::from_utf8_lossy(&pp.stderr).into_owned();
        let path = source.display().to_string();
        let remap = |text: &str| {
            let text = errmap::remap(
                text,
                &path,
                &assembled.src,
                assembled.new_start_line,
                assembled.new_line_count,
                &assembled.ranges,
                assembled.wrapped,
            );
            // Pointer and initializer temporaries are implementation details.
            let aliases: std::collections::HashMap<_, _> = assembled
                .bindings
                .iter()
                .flat_map(|(pointer, name)| {
                    [
                        (pointer.clone(), name.as_str()),
                        (pointer.replacen("cs_p", "cs_init", 1), name.as_str()),
                    ]
                })
                .collect();
            text.split_inclusive(|c: char| !c.is_alphanumeric() && c != '_')
                .map(|part| {
                    let end = part
                        .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_')
                        .len();
                    let (word, punctuation) = part.split_at(end);
                    format!(
                        "{}{punctuation}",
                        aliases.get(word).copied().unwrap_or(word)
                    )
                })
                .collect::<String>()
        };
        if pp.interrupted {
            return Err(i18n::text("program-interrupted"));
        }
        match pp.status {
            Some(status) if status.success() => {}
            Some(_) => return Err(demangle(&remap(&pp_diagnostics))),
            None => {
                return Err(i18n::text_with(
                    "compiler-timeout",
                    &[("seconds", ev.timeout.as_secs().to_string())],
                ));
            }
        }
        let prepared = std::fs::read_to_string(&preprocessed).and_then(|src| {
            std::fs::write(
                &preprocessed,
                bindings::rewrite(&src, step, &assembled.bindings),
            )
        });
        prepared.map_err(|error| {
            i18n::text_with("temp-source-write-error", &[("error", error.to_string())])
        })?;
        let mut args = ev.tc.compile_args(&preprocessed, &library);
        let language = args
            .iter()
            .position(|arg| arg == "-x")
            .expect("language flag");
        args[language + 1] = "cpp-output".to_string();
        args.splice(0..0, SHARED_FLAGS.iter().map(|flag| flag.to_string()));
        #[cfg(windows)]
        {
            // `compile_args` selects C with `-x c`; reset it before adding
            // the definition file and import archives as linker inputs.
            let definition = ev.dir.path().join(format!("step_{step}.def"));
            let import_library = library.with_extension("dll.a");
            let mut exports = format!("EXPORTS\n    cs_step_{step}\n    cs_flush_{step}\n");
            for symbol in &assembled.exports {
                exports.push_str(&format!("    {symbol}\n"));
            }
            std::fs::write(&definition, exports).map_err(|error| {
                i18n::text_with("temp-source-write-error", &[("error", error.to_string())])
            })?;
            args.extend([
                "-x".to_string(),
                "none".to_string(),
                definition.display().to_string(),
                "-Xlinker".to_string(),
                "--out-implib".to_string(),
                "-Xlinker".to_string(),
                import_library.display().to_string(),
            ]);
            args.extend(
                assembled
                    .imports
                    .iter()
                    .map(|path| path.display().to_string()),
            );
        }
        let mut command = Command::new(&ev.tc.path);
        command.current_dir(ev.dir.path()).args(&args);
        let captured = proc::run_captured(&mut command, ev.timeout, false).map_err(|error| {
            i18n::text_with("compiler-run-error", &[("error", error.to_string())])
        })?;
        let mut diagnostics = pp_diagnostics;
        diagnostics.push_str(&String::from_utf8_lossy(&captured.stderr));
        diagnostics.push_str(&String::from_utf8_lossy(&captured.stdout));
        if captured.interrupted {
            return Err(i18n::text("program-interrupted"));
        }
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

    fn ensure_runner(&mut self, ev: &Evaluator) -> Result<&mut Runner> {
        if self.runner.is_none() {
            self.runner = Some(Runner::spawn(ev)?);
        }
        Ok(self.runner.as_mut().expect("runner was just created"))
    }

    /// Restart the runner and silently replay every journaled step. Side
    /// effects of those steps repeat; this is recovery, not the normal path.
    fn recover(&mut self, ev: &Evaluator) {
        self.runner = None;
        let Ok(mut runner) = Runner::spawn(ev) else {
            self.reset();
            return;
        };
        for library in &self.journal {
            let result = runner.run(&library.path, &library.symbol, 0, ev.timeout);
            if !matches!(result.end, End::Done) || !result.reply.starts_with("OK") {
                self.runner = None;
                self.reset();
                return;
            }
        }
        self.runner = Some(runner);
    }

    /// Assemble and compile one step into a shared library.
    fn compile_step(
        &mut self,
        ev: &Evaluator,
        delta: &Delta,
        body: &Body,
    ) -> std::result::Result<Compiled, String> {
        let redefined: Vec<&str> = delta.funcs.iter().map(|(f, _)| f.name.as_str()).collect();
        let number = self.next_step;
        self.next_step += 1;
        let assembled = self.assemble(number, delta, body, &redefined);
        let (path, warnings) = self.compile(ev, number, &assembled)?;
        Ok(Compiled {
            library: Library {
                path,
                symbol: format!("cs_step_{number}"),
            },
            warnings,
        })
    }

    /// Compile one step, run it in the runner, and report the raw result.
    fn step(&mut self, ev: &Evaluator, delta: &Delta, body: &Body, slot: Slot) -> Result<Attempt> {
        match self.compile_step(ev, delta, body) {
            Ok(compiled) => Ok(Ok(self.execute(ev, compiled, slot, 0)?)),
            Err(diagnostics) => Ok(Err(diagnostics)),
        }
    }

    /// Run an already compiled step. `arg` reaches it through the reserved
    /// last slot; the same library may be run repeatedly (`%timeit`).
    fn execute(
        &mut self,
        ev: &Evaluator,
        compiled: Compiled,
        slot: Slot,
        arg: u64,
    ) -> Result<StepRun> {
        let Compiled { library, warnings } = compiled;
        let started = Instant::now();
        let timeout = ev.timeout;
        let result = self
            .ensure_runner(ev)?
            .run(&library.path, &library.symbol, arg, timeout);
        let duration = started.elapsed();

        let out = String::from_utf8_lossy(&result.out).into_owned();
        let err = String::from_utf8_lossy(&result.err).into_owned();
        let out_parts = split_new(&out, true);
        let err_parts = split_new(&err, false);
        let (output, dims) = take_dims(&out_parts.output);
        let timed_duration = out_parts.time_ns.map(Duration::from_nanos);
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
        if abnormal.is_some() {
            self.recover(ev);
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
            source_scoped_rebind: false,
            file_replacement: None,
            unprintable: false,
            duration: timed_duration.unwrap_or(duration),
            timed_duration,
        };
        Ok(StepRun {
            outcome,
            library,
            dims,
        })
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
                #[cfg(windows)]
                import_library: run.library.path.with_extension("dll.a"),
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

    /// Resident mode only allows output: calls that read input or change
    /// files, processes or the environment are refused before compiling, so
    /// replaying the journal after a crash can only repeat (hidden) output.
    fn forbidden(input: &str) -> Option<String> {
        let calls = lex::blocked_resident_calls(input);
        (!calls.is_empty()).then(|| {
            let calls = calls
                .iter()
                .map(|name| format!("{name}()"))
                .collect::<Vec<_>>()
                .join(", ");
            i18n::text_with("resident-side-effects", &[("calls", calls)])
        })
    }

    fn empty_delta(&self) -> Delta {
        Delta {
            remove_items: Vec::new(),
            items: Vec::new(),
            funcs: Vec::new(),
            vars: Vec::new(),
            first_slot: self.next_slot,
        }
    }

    /// `%type`, `%bits`, `%utf*`: evaluate a read-only probe against the live
    /// state. Nothing is committed and nothing is replayed.
    pub fn probe(
        &mut self,
        ev: &Evaluator,
        session: &Session,
        input: &str,
        probe: &codegen::Probe,
    ) -> Result<Eval> {
        let input = input.trim();
        if let Some(message) = Self::forbidden(input) {
            return Ok(Eval::CompileError(message));
        }
        let (before, after) = codegen::probe_parts(session, input, probe);
        let body = Body::Wrapped {
            before: &before,
            input,
            after: &after,
            bits: matches!(probe, codegen::Probe::Bits(_)),
        };
        match self.attempt(ev, &body, Slot::Expr)? {
            Ok(run) => Ok(Eval::Done(Box::new(run.outcome))),
            Err(diagnostics) => Ok(Eval::CompileError(diagnostics)),
        }
    }

    /// `%time`: an ordinary evaluation whose newest input is timed in-process.
    pub fn time(&mut self, ev: &Evaluator, session: &Session, input: &str) -> Result<Eval> {
        self.timed = true;
        let result = self.eval(ev, session, input);
        self.timed = false;
        result
    }

    /// `%timeit`: compile the benchmark once and run it in the runner with
    /// growing loop counts. Like IPython, it executes against the live state,
    /// so a statement such as `n++` really does change `n`.
    pub fn timeit(&mut self, ev: &Evaluator, _session: &Session, input: &str) -> Result<Eval> {
        if let Some(message) = Self::forbidden(input) {
            return Ok(Eval::CompileError(message));
        }
        let loops = format!("(uint64_t)(uintptr_t)cs_slot[{}]", SLOT_CAP - 1);
        let mut text = input.trim().to_string();
        let mut repaired = false;
        let compiled = loop {
            let (before, after) = codegen::timeit_parts(&text, &loops);
            let body = Body::Wrapped {
                before: &before,
                input: &text,
                after: &after,
                bits: false,
            };
            let delta = self.empty_delta();
            match self.compile_step(ev, &delta, &body) {
                Ok(compiled) => break compiled,
                Err(_) if !repaired && !text.ends_with(';') => {
                    text.push(';');
                    repaired = true;
                }
                Err(diagnostics) => return Ok(Eval::CompileError(diagnostics)),
            }
        };
        let library = Library {
            path: compiled.library.path.clone(),
            symbol: compiled.library.symbol.clone(),
        };
        let warnings = compiled.warnings.clone();
        ev.timeit_report(|count| {
            let compiled = Compiled {
                library: Library {
                    path: library.path.clone(),
                    symbol: library.symbol.clone(),
                },
                warnings: warnings.clone(),
            };
            Ok(self.execute(ev, compiled, Slot::Expr, count)?.outcome)
        })
    }

    pub fn eval(&mut self, ev: &Evaluator, session: &Session, input: &str) -> Result<Eval> {
        let trimmed = input.trim();
        if let Some(message) = Self::forbidden(trimmed) {
            return Ok(Eval::CompileError(message));
        }
        match decl::analyze(trimmed) {
            Analysis::Unsupported(reason) => {
                return Ok(Eval::CompileError(i18n::text_with(
                    "resident-unsupported",
                    &[("reason", reason)],
                )));
            }
            Analysis::Plan(plan) => {
                // The syntax parser has no typedef table: `x * y;` can look
                // like a declaration when x is actually a live variable.
                let expression = !plan.vars.is_empty()
                    && plan
                        .vars
                        .iter()
                        .all(|v| self.vars.iter().any(|old| old.decl.name == v.declspec));
                if !expression {
                    return match self.eval_plan(ev, trimmed, plan)? {
                        Ok(outcome) => Ok(Eval::Done(Box::new(outcome))),
                        Err(diagnostics) => Ok(Eval::CompileError(diagnostics)),
                    };
                }
            }
            Analysis::Statement => {}
        }
        match self.eval_text(ev, session, trimmed)? {
            Ok(outcome) => Ok(Eval::Done(Box::new(outcome))),
            Err(diagnostics) => Ok(Eval::CompileError(diagnostics)),
        }
    }

    fn eval_plan(
        &mut self,
        ev: &Evaluator,
        input: &str,
        plan: Plan,
    ) -> Result<std::result::Result<Outcome, String>> {
        // Resident rebindings allocate a fresh slot. In the source view,
        // a nested C scope represents that new binding and preserves pointers
        // to earlier objects.
        let source_scoped_rebind = plan
            .vars
            .iter()
            .any(|new| self.vars.iter().any(|old| old.decl.name == new.name));
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
        if self.next_slot + plan.vars.len() >= SLOT_CAP - 1 {
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
            run.outcome.source_scoped_rebind = source_scoped_rebind;
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
        // The runner starts lazily: its stdin depends on how c-shell is used.
        Ok(Self::default())
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

    #[test]
    fn output_limit_still_detects_a_split_completion_marker() {
        let mut bytes = vec![b'x'; proc::MAX_CAPTURE_BYTES - 2];
        let (mut done, mut truncated) = (false, false);
        let mut tail = Vec::new();
        let mut chunk = vec![b'x'; 16];
        chunk.extend_from_slice(&M_DONE.as_bytes()[..5]);
        absorb(&mut bytes, &chunk, &mut done, &mut truncated, &mut tail);
        assert!(truncated);
        assert!(!done);
        absorb(
            &mut bytes,
            &M_DONE.as_bytes()[5..],
            &mut done,
            &mut truncated,
            &mut tail,
        );
        assert!(done);
        assert_eq!(bytes.len(), proc::MAX_CAPTURE_BYTES);
    }
}
