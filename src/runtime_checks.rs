//! Optional runtime instrumentation, probed only when `%check` is requested.
//!
//! These messages deliberately remain English in every UI language. Compiler
//! families select argument syntax; actual compile/link/run probes select the
//! checks, including for clang-cl and installations missing sanitizer runtimes.

use std::io::Read as _;
use std::process::Command;
use std::time::Duration;

use crate::{proc, toolchain::Toolchain};

pub const USAGE: &str = "Usage: %check [--address | --undefined] <C code>";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Request {
    Auto,
    Address,
    Undefined,
}

pub fn parse(input: &str) -> Result<(Request, &str), &'static str> {
    let mut code = input.trim();
    let mut request = Request::Auto;
    loop {
        let end = code.find(char::is_whitespace).unwrap_or(code.len());
        let (first, rest) = code.split_at(end);
        match first {
            "--" => {
                code = rest.trim_start();
                break;
            }
            "--address" if request == Request::Auto => request = Request::Address,
            "--undefined" if request == Request::Auto => request = Request::Undefined,
            option if option.starts_with("--") => return Err(USAGE),
            _ => break,
        }
        code = rest.trim_start();
    }
    if code.is_empty() {
        Err(USAGE)
    } else {
        Ok((request, code))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checks {
    address: bool,
    undefined: bool,
}

impl Checks {
    const ADDRESS: Self = Self {
        address: true,
        undefined: false,
    };
    const UNDEFINED: Self = Self {
        address: false,
        undefined: true,
    };
    const BOTH: Self = Self {
        address: true,
        undefined: true,
    };

    pub fn detect(tc: &Toolchain, request: Request, timeout: Duration) -> Result<Self, String> {
        let candidates: &[Self] = match request {
            Request::Auto => &[Self::BOTH, Self::ADDRESS, Self::UNDEFINED],
            Request::Address => &[Self::ADDRESS],
            Request::Undefined => &[Self::UNDEFINED],
        };
        candidates.iter().copied().find(|checks| checks.probe(tc, timeout)).ok_or_else(|| {
            let kind = match request {
                Request::Auto => "Runtime checking",
                Request::Address => "AddressSanitizer",
                Request::Undefined => "UndefinedBehaviorSanitizer",
            };
            format!("{kind} is unavailable with the current compiler, target, or runtime installation.")
        })
    }

    pub fn compile_args(
        &self,
        tc: &Toolchain,
        src: &std::path::Path,
        exe: &std::path::Path,
        dir: &std::path::Path,
    ) -> Vec<String> {
        let mut args = tc.compile_args(src, exe, dir);
        let mut flags = Vec::new();
        if tc.is_msvc() {
            if self.address {
                flags.push("/fsanitize=address");
            }
            if self.undefined {
                flags.push("/fsanitize=undefined");
            }
            // /Z7 embeds debug information in the temporary object, avoiding
            // a compiler PDB in the user's current working directory.
            flags.extend(["/Z7", "/Od"]);
            args.extend([
                "/DEBUG".to_string(),
                "/INCREMENTAL:NO".to_string(),
                format!("/PDB:{}", exe.with_extension("pdb").display()),
            ]);
        } else {
            let sanitizer = match (self.address, self.undefined) {
                (true, true) => "-fsanitize=address,undefined",
                (true, false) => "-fsanitize=address",
                (false, true) => "-fsanitize=undefined",
                (false, false) => unreachable!("at least one check is required"),
            };
            flags.extend([
                sanitizer,
                "-g",
                "-O0",
                "-fno-omit-frame-pointer",
                "-fno-sanitize-recover=all",
            ]);
        }
        // Compiler switches must precede the MSVC /link delimiter.
        args.splice(0..0, flags.into_iter().map(str::to_string));
        args
    }

    pub fn capture(&self, command: &mut Command) -> std::io::Result<ReportCapture> {
        let report = ReportCapture {
            dir: tempfile::tempdir()?,
        };
        // Quoting keeps Windows drive-letter colons and spaces inside the
        // option value. Reports stay separate from program stderr and prompts.
        let logging = format!(
            "log_path=\"{}\":log_exe_name=0:log_to_syslog=0",
            report.dir.path().join("report").display()
        );
        if self.address {
            // A replay process intentionally loses all session allocations at
            // exit. Leak checking would diagnose unfinished exercises rather
            // than the operation the learner asked to check.
            // Summaries need no stack symbols; avoid starting an external
            // symbolizer (and possible symbol-server lookups) for each error.
            append_options(
                command,
                "ASAN_OPTIONS",
                &format!(
                    "detect_leaks=0:halt_on_error=1:abort_on_error=0:exitcode=1:symbolize=0:{logging}"
                ),
            );
        }
        if self.undefined {
            append_options(
                command,
                "UBSAN_OPTIONS",
                &format!("halt_on_error=1:print_stacktrace=0:symbolize=0:{logging}"),
            );
        }
        Ok(report)
    }

    fn probe(&self, tc: &Toolchain, timeout: Duration) -> bool {
        let Ok(dir) = tempfile::tempdir() else {
            return false;
        };
        let src = dir.path().join("check.c");
        let exe = dir
            .path()
            .join(format!("check{}", std::env::consts::EXE_SUFFIX));
        if std::fs::write(&src, PROBE).is_err() {
            return false;
        }
        let timeout = timeout.min(Duration::from_secs(10));
        let mut command = Command::new(&tc.path);
        command.args(self.compile_args(tc, &src, &exe, dir.path()));
        let Ok(compiled) = proc::run_captured(&mut command, timeout, false) else {
            return false;
        };
        let diagnostics = format!(
            "{}{}",
            String::from_utf8_lossy(&compiled.stdout),
            String::from_utf8_lossy(&compiled.stderr)
        )
        .to_ascii_lowercase();
        if !compiled.status.is_some_and(|s| s.success())
            || diagnostics.contains("d9002")
            || diagnostics.contains("ignoring unknown option")
            || diagnostics.contains("argument unused during compilation")
        {
            return false;
        }

        // A successful link alone does not establish that runtime DLLs can
        // load, or that a compiler silently accepting a flag instruments code.
        let run = |arg: Option<&str>| {
            let mut command = Command::new(&exe);
            if let Some(arg) = arg {
                command.arg(arg);
            }
            let report = self.capture(&mut command).ok()?;
            let captured = proc::run_captured(&mut command, timeout, false).ok()?;
            Some((captured, report.read().ok()?))
        };
        if !run(None).is_some_and(|(c, _)| c.status.is_some_and(|s| s.success())) {
            return false;
        }
        for (enabled, argument, report) in [
            (self.address, "address", "heap-buffer-overflow"),
            (self.undefined, "undefined", "signed integer overflow"),
        ] {
            if enabled
                && !run(Some(argument)).is_some_and(|(c, output)| {
                    let diagnostics = format!("{output}{}", String::from_utf8_lossy(&c.stderr));
                    c.status.is_some_and(|s| !s.success())
                        && has_report(&diagnostics)
                        && diagnostics.contains(report)
                })
            {
                return false;
            }
        }
        true
    }
}

/// A fresh directory per execution prevents stale reports from being reused
/// during array refinement or compiler trials. Reading stays capture-bounded.
pub struct ReportCapture {
    dir: tempfile::TempDir,
}

/// GCC's combined ASan/UBSan runtime can still send UBSan findings to stderr.
/// Separate that trailing report from ordinary program stderr as a fallback.
pub fn split_stderr<'a>(stderr: &'a str, source: &str) -> (&'a str, Option<&'a str>) {
    let Some(marker) = stderr
        .match_indices("runtime error:")
        .find_map(|(index, _)| has_location_suffix(&stderr[..index]).then_some(index))
    else {
        return (stderr, None);
    };
    let line_start = stderr[..marker].rfind('\n').map_or(0, |index| index + 1);
    let path = stderr[..marker]
        .rfind(source)
        .filter(|&index| index >= line_start)
        .or_else(|| {
            stderr[..marker]
                .rfind(&source.replace('\\', "/"))
                .filter(|&index| index >= line_start)
        });
    let start = path.unwrap_or_else(|| {
        stderr[..marker]
            .rfind(crate::codegen::M_NEW)
            .map_or(line_start, |index| {
                line_start.max(index + crate::codegen::M_NEW.len())
            })
    });
    (&stderr[..start], Some(&stderr[start..]))
}

/// A native UBSan header contains a source location before its message. The
/// words "runtime error:" alone are ordinary program output, not evidence.
fn has_location_suffix(prefix: &str) -> bool {
    let Some(location) = prefix.trim_end().strip_suffix(':') else {
        return false;
    };
    let Some((file_or_line, number)) = location.rsplit_once(':') else {
        return false;
    };
    !file_or_line.is_empty()
        && !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
}

/// Withhold report prefixes across arbitrary chunks before the terminal sees
/// a generated path or trace. Program prompts still stream without a newline.
pub struct StderrFilter {
    sources: [Vec<u8>; 2],
    pending: Vec<u8>,
    reporting: bool,
    suppressed: Vec<u8>,
}

impl StderrFilter {
    pub fn new(source: &str) -> Self {
        Self {
            sources: [
                source.as_bytes().to_vec(),
                source.replace('\\', "/").into_bytes(),
            ],
            pending: Vec::new(),
            reporting: false,
            suppressed: Vec::new(),
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.reporting {
            self.suppress(chunk);
            return Vec::new();
        }
        self.pending.extend_from_slice(chunk);
        let location = self
            .sources
            .iter()
            .filter_map(|marker| {
                self.pending
                    .windows(marker.len())
                    .position(|window| window == marker)
            })
            .min();
        let header = self
            .pending
            .windows(b"runtime error:".len())
            .enumerate()
            .find_map(|(index, window)| {
                (window == b"runtime error:"
                    && has_location_suffix(&String::from_utf8_lossy(&self.pending[..index])))
                .then_some(index)
            });
        if let Some(header) = header {
            let start = location
                .filter(|&index| index <= header)
                .unwrap_or_else(|| location_candidate_start(&self.pending[..header]));
            self.reporting = true;
            let visible = self.pending[..start].to_vec();
            let pending = std::mem::take(&mut self.pending);
            self.suppress(&pending[start..]);
            return visible;
        }
        // Retain both known generated paths and a possible foreign-file
        // location. A header's path/line/column can arrive before the words
        // "runtime error:" in separate reads, including one byte at a time.
        let known = location
            .filter(|&index| {
                !self.pending[index..]
                    .iter()
                    .any(|byte| byte.is_ascii_control() && *byte != b'\t')
            })
            .unwrap_or_else(|| {
                self.pending.len()
                    - self
                        .sources
                        .iter()
                        .map(|marker| {
                            (1..=self.pending.len().min(marker.len() - 1))
                                .rev()
                                .find(|&length| self.pending.ends_with(&marker[..length]))
                                .unwrap_or(0)
                        })
                        .max()
                        .unwrap_or(0)
            });
        let start = known.min(location_candidate_start(&self.pending));
        let start = start.max(self.pending.len().saturating_sub(4096));
        self.pending.drain(..start).collect()
    }

    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    fn suppress(&mut self, bytes: &[u8]) {
        let keep = bytes
            .len()
            .min(proc::MAX_CAPTURE_BYTES - self.suppressed.len());
        self.suppressed.extend_from_slice(&bytes[..keep]);
    }

    pub fn take_suppressed(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.suppressed)
    }
}

/// Keep the current potential filename/location, but release ordinary prompts
/// at whitespace or punctuation. Paths with separators/extensions may contain
/// spaces. The caller bounds the lookahead to avoid buffering unbounded lines.
fn location_candidate_start(bytes: &[u8]) -> usize {
    let mut start = 0;
    let mut path = false;
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' || byte == b'\r' || byte.is_ascii_control() && byte != b'\t' {
            start = index + 1;
            path = false;
        } else if byte.is_ascii_whitespace() {
            if !path || bytes[..index].ends_with(b":") {
                if has_location_suffix(&String::from_utf8_lossy(&bytes[start..index])) {
                    path = true;
                } else {
                    start = index + 1;
                    path = false;
                }
            }
        } else if byte.is_ascii_alphanumeric() || byte >= 128 || b"_./\\:-~<>()[]+".contains(&byte)
        {
            path |= matches!(byte, b'.' | b'/' | b'\\');
        } else {
            start = index + 1;
            path = false;
        }
    }
    start
}

impl ReportCapture {
    pub fn read(&self) -> std::io::Result<String> {
        let mut files = std::fs::read_dir(self.dir.path())?.collect::<Result<Vec<_>, _>>()?;
        files.sort_by_key(|file| file.file_name());
        let mut bytes = Vec::new();
        for file in files {
            if file.file_type()?.is_file() {
                std::fs::File::open(file.path())?
                    .take((proc::MAX_CAPTURE_BYTES + 1 - bytes.len()) as u64)
                    .read_to_end(&mut bytes)?;
                if bytes.len() > proc::MAX_CAPTURE_BYTES {
                    break;
                }
            }
        }
        let truncated = bytes.len() > proc::MAX_CAPTURE_BYTES;
        bytes.truncate(proc::MAX_CAPTURE_BYTES);
        let mut report = String::from_utf8_lossy(&bytes).into_owned();
        if truncated {
            report.push_str("\n(Runtime report truncated.)\n");
        }
        Ok(report)
    }
}

/// Explain common findings without native stack frames, addresses or allocator
/// internals. Unknown diagnostics receive a brief generic explanation.
pub fn summarize(report: &str) -> String {
    let undefined = report.lines().find_map(|line| {
        line.split_once("runtime error: ")
            .map(|(_, message)| message)
    });
    let (kind, detail) = if let Some(message) = undefined {
        if let Some(detail) = message.strip_prefix("signed integer overflow: ") {
            ("signed integer overflow", detail)
        } else if message.contains("division by zero") {
            ("integer division by zero", "The divisor evaluated to zero.")
        } else if message.starts_with("shift exponent") || message.starts_with("left shift") {
            ("invalid bit shift", message)
        } else if message.contains("null pointer") {
            (
                "null pointer access",
                "The operation tried to access an object through a null pointer.",
            )
        } else if message.contains("misaligned address") {
            (
                "misaligned pointer access",
                "The pointer does not meet the accessed type's alignment requirement.",
            )
        } else if message.starts_with("index ") && message.contains("out of bounds") {
            ("array index out of bounds", message)
        } else {
            (
                "undefined behavior detected",
                "The operation is not valid for these values.",
            )
        }
    } else if report.contains("heap-use-after-free") {
        ("use after free", "Memory was accessed after it was freed.")
    } else if report.contains("double-free") {
        (
            "double free",
            "The same allocation was freed more than once.",
        )
    } else if report.contains("heap-buffer-overflow") {
        (
            "memory access out of bounds",
            "The access went outside an allocated memory block.",
        )
    } else if report.contains("stack-buffer-overflow") || report.contains("global-buffer-overflow")
    {
        (
            "memory access out of bounds",
            "The access went outside an object's storage.",
        )
    } else if report.contains("stack-use-after-return") || report.contains("stack-use-after-scope")
    {
        (
            "access outside object lifetime",
            "A local object was accessed after its lifetime ended.",
        )
    } else {
        (
            "invalid operation detected",
            "The runtime checker reported an error.",
        )
    };
    format!("Runtime check: {kind}.\n{detail}")
}

fn append_options(command: &mut Command, name: &str, required: &str) {
    let mut options = std::env::var_os(name).unwrap_or_default();
    if !options.is_empty() {
        options.push(":");
    }
    options.push(required);
    command.env(name, options);
}

pub fn has_report(stderr: &str) -> bool {
    stderr.contains("ERROR: AddressSanitizer")
        || stderr.contains("AddressSanitizer:DEADLYSIGNAL")
        || stderr.contains("runtime error:")
        || stderr.contains("SUMMARY: UndefinedBehaviorSanitizer")
}

// Volatile accesses prevent the compiler from erasing the test operations.
// argc chooses the bad operation at runtime; the no-argument run is valid.
const PROBE: &str = r#"#include <stdlib.h>
#include <limits.h>
int main(int argc, char **argv) {
    if (argc > 1 && argv[1][0] == 'a') {
        volatile unsigned char *p = (unsigned char *)malloc(1);
        if (!p) return 2;
        p[argc] = 1;
        free((void *)p);
    } else if (argc > 1) {
        volatile int x = INT_MAX;
        x += argc - 1;
        return x == 0;
    }
    return 0;
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_check_options_without_rewriting_code() {
        assert_eq!(
            parse(" --address a[i++] "),
            Ok((Request::Address, "a[i++]"))
        );
        assert_eq!(
            parse("--undefined x + 1"),
            Ok((Request::Undefined, "x + 1"))
        );
        assert_eq!(parse("x +\n 1"), Ok((Request::Auto, "x +\n 1")));
        assert_eq!(parse("-- --x"), Ok((Request::Auto, "--x")));
        for input in [
            "",
            "--address",
            "--bogus x",
            "--undefined",
            "--verbose",
            "--verbose x + 1",
            "--address --verbose x",
            "--address --undefined x",
        ] {
            assert_eq!(parse(input), Err(USAGE));
        }
    }

    #[test]
    fn runtime_reports_are_recognized_across_backends() {
        assert!(has_report(
            "==12==ERROR: AddressSanitizer: heap-use-after-free"
        ));
        assert!(has_report(
            "input.c:3: runtime error: signed integer overflow"
        ));
        assert!(!has_report("program exited with code 1"));
    }

    #[test]
    fn summaries_explain_errors_without_native_runtime_details() {
        let report = "<input>:1:5: runtime error: signed integer overflow: 2147483647 + 1 cannot be represented in type 'int'\n    #0 0x12345 in main <input>:1\n    #1 0x67890 /usr/lib/libc.so.6 (BuildId: abc)\n";
        assert_eq!(
            summarize(report),
            "Runtime check: signed integer overflow.\n2147483647 + 1 cannot be represented in type 'int'"
        );
        let address = "==42==ERROR: AddressSanitizer: heap-use-after-free on address 0x12345\n#0 main <session>:3:7\nShadow bytes ...";
        assert_eq!(
            summarize(address),
            "Runtime check: use after free.\nMemory was accessed after it was freed."
        );
        let unknown = summarize("AddressSanitizer: unexpected diagnostic at 0x12345");
        assert!(unknown.contains("invalid operation detected"));
        assert!(!unknown.contains("0x12345"));
    }

    #[test]
    fn live_fallback_withholds_chunked_reports_but_preserves_prompts_and_files() {
        let source = "/tmp/test/input.c";
        let mut filter = StderrFilter::new(source);
        assert_eq!(filter.feed(b"value? "), b"value? ");
        assert_eq!(filter.feed(b"log.txt: "), b"log.txt: ");
        let report =
            format!("{source}:642:5: runtime error: signed integer overflow\n#0 0x123 main\n");
        let visible = report
            .bytes()
            .flat_map(|byte| filter.feed(&[byte]))
            .collect::<Vec<_>>();
        assert!(visible.is_empty(), "{}", String::from_utf8_lossy(&visible));
        assert!(filter.finish().is_empty());
        let mut plain = StderrFilter::new(source);
        let mut visible = plain.feed(source.as_bytes());
        visible.extend(plain.finish());
        assert_eq!(visible, source.as_bytes());
        let marker = format!("{source}{}", crate::codegen::M_STDIN);
        let mut prompt = StderrFilter::new(source);
        assert_eq!(prompt.feed(marker.as_bytes()), marker.as_bytes());
        let stderr = format!("{}prompt? {report}", crate::codegen::M_NEW);
        let (program, diagnostic) = split_stderr(&stderr, source);
        assert!(program.ends_with("prompt? "));
        assert_eq!(diagnostic, Some(report.as_str()));
        let ordinary = "runtime error: demo\nERROR: AddressSanitizer is just some text\n";
        assert_eq!(split_stderr(ordinary, source), (ordinary, None));
        let mut plain = StderrFilter::new(source);
        let mut visible = ordinary
            .bytes()
            .flat_map(|byte| plain.feed(&[byte]))
            .collect::<Vec<_>>();
        visible.extend(plain.finish());
        assert_eq!(visible, ordinary.as_bytes());
    }

    #[test]
    fn live_fallback_withholds_foreign_locations_at_every_chunk_boundary() {
        for location in [
            "/tmp/helper.h:7:3:",
            "C:\\SDK Files\\helper.h:7:3:",
            "helper.h:7:3:",
            "<header>:7:3:",
            "helper:7:",
        ] {
            let report = format!("{location} runtime error: signed integer overflow\n#0 trace\n");
            let wire = format!("value? {report}");
            for split in 0..=wire.len() {
                let mut filter = StderrFilter::new("/tmp/input.c");
                let mut visible = filter.feed(&wire.as_bytes()[..split]);
                visible.extend(filter.feed(&wire.as_bytes()[split..]));
                visible.extend(filter.finish());
                assert_eq!(visible, b"value? ", "{location}, split {split}");
                assert_eq!(filter.take_suppressed(), report.as_bytes());
            }
            let mut filter = StderrFilter::new("/tmp/input.c");
            let mut visible = Vec::new();
            for byte in wire.bytes() {
                visible.extend(filter.feed(&[byte]));
            }
            visible.extend(filter.finish());
            assert_eq!(visible, b"value? ", "{location}, byte chunks");
            assert_eq!(filter.take_suppressed(), report.as_bytes());
        }
    }

    #[test]
    fn report_files_obey_the_capture_limit() {
        let mut command = Command::new("unused");
        let capture = Checks::ADDRESS.capture(&mut command).unwrap();
        let file = std::fs::File::create(capture.dir.path().join("report.1")).unwrap();
        file.set_len((proc::MAX_CAPTURE_BYTES + 100) as u64)
            .unwrap();
        let report = capture.read().unwrap();
        assert!(report.ends_with("(Runtime report truncated.)\n"));
        assert!(report.len() < proc::MAX_CAPTURE_BYTES + 100);

        let mut filter = StderrFilter::new("/tmp/input.c");
        assert!(
            filter
                .feed(b"helper.h:7:3: runtime error: demo\n")
                .is_empty()
        );
        assert!(
            filter
                .feed(&vec![b'x'; proc::MAX_CAPTURE_BYTES + 100])
                .is_empty()
        );
        assert_eq!(filter.take_suppressed().len(), proc::MAX_CAPTURE_BYTES);
        let mut plain = StderrFilter::new("/tmp/input.c");
        plain.feed(&vec![b'x'; 8192]);
        assert!(plain.pending.len() <= 4096);
    }

    #[test]
    fn msvc_instrumentation_stays_before_the_linker_delimiter() {
        let tc = Toolchain {
            path: "cl.exe".into(),
            family: crate::toolchain::Family::Msvc,
            version: String::new(),
            std: "c17".into(),
            default_std: None,
            auto_std: false,
        };
        let args = Checks::ADDRESS.compile_args(
            &tc,
            std::path::Path::new("temp/input.c"),
            std::path::Path::new("temp/input.exe"),
            std::path::Path::new("temp"),
        );
        let linker = args.iter().position(|arg| arg == "/link").unwrap();
        for option in ["/fsanitize=address", "/Z7", "/Od"] {
            assert!(args[..linker].iter().any(|arg| arg == option));
        }
        assert!(
            args[linker + 1..]
                .iter()
                .any(|arg| arg == "/INCREMENTAL:NO")
        );
        assert!(!args.iter().any(|arg| arg == "/Zi"));
        assert!(
            args[linker + 1..]
                .iter()
                .any(|arg| arg == "/PDB:temp/input.pdb")
        );
    }
}
