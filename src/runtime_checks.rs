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
            append_options(
                command,
                "ASAN_OPTIONS",
                &format!("detect_leaks=0:halt_on_error=1:abort_on_error=0:exitcode=1:{logging}"),
            );
        }
        if self.undefined {
            append_options(
                command,
                "UBSAN_OPTIONS",
                &format!("halt_on_error=1:print_stacktrace=1:{logging}"),
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
                    c.status.is_some_and(|s| !s.success())
                        && (output.contains(report)
                            || String::from_utf8_lossy(&c.stderr).contains(report))
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
    let Some(marker) = stderr.find("runtime error:") else {
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

/// Withhold report prefixes across arbitrary chunks before the terminal sees
/// a generated path or trace. Program prompts still stream without a newline.
pub struct StderrFilter {
    markers: [Vec<u8>; 3],
    pending: Vec<u8>,
    reporting: bool,
}

impl StderrFilter {
    pub fn new(source: &str) -> Self {
        Self {
            markers: [
                source.as_bytes().to_vec(),
                source.replace('\\', "/").into_bytes(),
                b"runtime error:".to_vec(),
            ],
            pending: Vec::new(),
            reporting: false,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.reporting {
            return Vec::new();
        }
        self.pending.extend_from_slice(chunk);
        let location = self.markers[..2]
            .iter()
            .filter_map(|marker| {
                self.pending
                    .windows(marker.len())
                    .position(|window| window == marker)
            })
            .min();
        let header = self
            .pending
            .windows(self.markers[2].len())
            .position(|window| window == self.markers[2]);
        if let Some(header) = header {
            let start = location.filter(|&index| index <= header).unwrap_or(header);
            self.reporting = true;
            let visible = self.pending[..start].to_vec();
            self.pending.clear();
            return visible;
        }
        if let Some(location) = location {
            let candidate = &self.pending[location..];
            let protocol = [
                crate::codegen::M_NEW,
                crate::codegen::M_DONE,
                crate::codegen::M_STDIN,
            ];
            if candidate.len() < 4096
                && !candidate.contains(&b'\n')
                && !protocol.iter().any(|marker| {
                    candidate
                        .windows(marker.len())
                        .any(|bytes| bytes == marker.as_bytes())
                })
            {
                return self.pending.drain(..location).collect();
            }
        }
        let keep = self
            .markers
            .iter()
            .map(|marker| {
                (1..=self.pending.len().min(marker.len() - 1))
                    .rev()
                    .find(|&length| self.pending.ends_with(&marker[..length]))
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0);
        self.pending.drain(..self.pending.len() - keep).collect()
    }

    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
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
