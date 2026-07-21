//! Finding a C compiler and speaking its flag dialect.

use anyhow::{Context, Result, bail};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use crate::proc;

/// Deadline for every probe: a compiler that needs longer than this to
/// answer `--version` or build ten lines is effectively hung, and a hung
/// probe would otherwise freeze startup before the prompt even appears.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Successful capability probes remain valid until the compiler, relevant
/// environment, requested standard, c-shell version, or this TTL changes.
const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const CACHE_SCHEMA: &str = "toolchain-v3";

/// `.output()` with a deadline; `None` covers spawn failure and timeout
/// alike — for a probe those are the same answer.
fn probe_output(cmd: &mut Command) -> Option<proc::Captured> {
    let cap = proc::run_captured(cmd, PROBE_TIMEOUT, false).ok()?;
    cap.status.is_some().then_some(cap)
}

/// cl.exe reports an unknown command-line option as warning D9002 and still
/// exits successfully. clang-cl can likewise warn that an option was unused.
/// A capability probe must treat both as rejection; exit status alone would
/// silently accept an unsupported `/std:` value.
fn msvc_ignored_option(cap: &proc::Captured) -> bool {
    ignored_msvc_option_text(&format!(
        "{}\n{}",
        String::from_utf8_lossy(&cap.stdout),
        String::from_utf8_lossy(&cap.stderr)
    ))
}

fn ignored_msvc_option_text(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("d9002")
        || text.contains("ignoring unknown option")
        || text.contains("argument unused during compilation")
}

/// Which flag dialect a compiler speaks. Version numbers are deliberately
/// not part of this: capabilities are probed by trial compile, never derived
/// from a version string.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    Gnu,
    Clang,
    /// `cl.exe`, and `clang-cl` which mimics its command line.
    Msvc,
}

#[derive(Clone, Debug)]
pub struct Toolchain {
    pub path: PathBuf,
    pub family: Family,
    /// First line of the compiler's own version banner.
    pub version: String,
    /// The `-std=` value to pass, or empty to leave the compiler on its own
    /// default. Empty is the normal state: what a plain `gcc foo.c` does on
    /// this machine is exactly what the tool exists to demonstrate.
    pub std: String,
    /// What a flag-less compile actually is (e.g. "gnu23"), reported by the
    /// compiler itself via `__STDC_VERSION__` / `__STRICT_ANSI__`.
    pub default_std: Option<String>,
    /// Set when `std` was forced by us rather than the user, because the
    /// compiler's default mode cannot host the `_Generic` value printer
    /// (MSVC without `/std:` is C89).
    pub auto_std: bool,
}

/// Compiled and *run* once per detection, with no `-std` flag: the compiler
/// reports what its default mode is. A successful run doubles as the
/// self-test that this toolchain can build a working executable at all.
const VERSION_PROBE: &str = r#"#include <stdio.h>
int main(void) {
    long v = 0;
#ifdef __STDC_VERSION__
    v = (long)__STDC_VERSION__;
#endif
    int gnu = 1;
#ifdef __STRICT_ANSI__
    gnu = 0;
#endif
    printf("%ld %d\n", v, gnu);
    return 0;
}
"#;

/// Exercise the C features used by the value-printer runtime rather than
/// `_Generic` alone: some compilers accept `_Generic` as an extension in a
/// mode that still rejects `inline` or `_Bool`.
const VALUE_PRINTER_PROBE: &str = r#"
static inline int cs_probe(_Bool v) { return v ? 1 : 0; }
int main(void) {
    int (*p)(_Bool) = _Generic(1, int: cs_probe, default: cs_probe);
    return p(1) - 1;
}
"#;

/// Turn the VERSION_PROBE's output into a human label like "gnu23".
fn parse_std_probe(out: &str, family: Family) -> Option<String> {
    let mut it = out.split_whitespace();
    let v: i64 = it.next()?.parse().ok()?;
    let gnu: i32 = it.next()?.parse().ok()?;
    let n = match v {
        202000.. => "23", // gcc used 202000L for pre-release C2x
        201710.. => "17",
        201112.. => "11",
        199901.. => "99",
        _ => "89",
    };
    // __STRICT_ANSI__ is a GNU-family signal; MSVC has no gnu dialect.
    let prefix = if gnu == 1 && family != Family::Msvc {
        "gnu"
    } else {
        "c"
    };
    Some(format!("{prefix}{n}"))
}

/// Candidate compiler names, most preferred first. On Windows `cc` is rarely
/// present but costs nothing to try; `cl` comes last because it only works
/// from a Developer Command Prompt.
#[cfg(windows)]
const CANDIDATES: &[&str] = &["gcc", "clang", "cc", "clang-cl", "cl"];
#[cfg(not(windows))]
const CANDIDATES: &[&str] = &["cc", "gcc", "clang", "tcc"];

fn family_of(path: &Path, banner: &str) -> Family {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // clang-cl must be checked before clang: it is clang, but takes MSVC flags.
    if stem == "cl" || stem.ends_with("clang-cl") {
        return Family::Msvc;
    }
    // Never trust the executable's name for gcc-vs-clang. Apple's Command
    // Line Tools may expose /usr/bin/gcc as an Apple Clang driver, while a
    // separately installed GNU GCC is genuine GCC; the banner distinguishes
    // them without making assumptions from the filename.
    if banner.to_ascii_lowercase().contains("clang") {
        return Family::Clang;
    }
    Family::Gnu
}

fn cache_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    }?;
    Some(base.join("c-shell"))
}

/// Fingerprint everything that can reasonably change a probe's answer.
/// Compiler metadata catches upgrades in place; the selected environment
/// variables cover driver lookup, headers, libraries and platform SDKs.
fn cache_key(path: &Path, requested_std: Option<&str>) -> Option<u64> {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let meta = std::fs::metadata(&path).ok()?;
    let modified = meta
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    CACHE_SCHEMA.hash(&mut h);
    env!("CARGO_PKG_VERSION").hash(&mut h);
    path.hash(&mut h);
    meta.len().hash(&mut h);
    modified.as_nanos().hash(&mut h);
    requested_std.hash(&mut h);
    for name in [
        "PATH",
        "INCLUDE",
        "LIB",
        "LIBPATH",
        "CPATH",
        "C_INCLUDE_PATH",
        "LIBRARY_PATH",
        "SDKROOT",
        "DEVELOPER_DIR",
        "GCC_EXEC_PREFIX",
        "COMPILER_PATH",
    ] {
        name.hash(&mut h);
        std::env::var_os(name).hash(&mut h);
    }
    Some(h.finish())
}

fn hex_encode(s: &str) -> String {
    s.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Option<String> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    String::from_utf8(bytes).ok()
}

fn load_cached(path: &Path, requested_std: Option<&str>) -> Option<Toolchain> {
    let key = cache_key(path, requested_std)?;
    let file = cache_dir()?.join(format!("{key:016x}.cache"));
    let age = std::fs::metadata(&file)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()?;
    if age > CACHE_TTL {
        return None;
    }
    let text = std::fs::read_to_string(file).ok()?;
    let mut lines = text.lines();
    if lines.next()? != CACHE_SCHEMA || lines.next()? != format!("{key:016x}") {
        return None;
    }
    let family = match lines.next()? {
        "gnu" => Family::Gnu,
        "clang" => Family::Clang,
        "msvc" => Family::Msvc,
        _ => return None,
    };
    let version = hex_decode(lines.next()?)?;
    let std = hex_decode(lines.next()?)?;
    let default_std = match lines.next()? {
        "-" => None,
        encoded => Some(hex_decode(encoded)?),
    };
    let auto_std = match lines.next()? {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    Some(Toolchain {
        path: path.to_path_buf(),
        family,
        version,
        std,
        default_std,
        auto_std,
    })
}

fn store_cached(tc: &Toolchain, requested_std: Option<&str>) {
    let Some(key) = cache_key(&tc.path, requested_std) else {
        return;
    };
    let Some(dir) = cache_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let family = match tc.family {
        Family::Gnu => "gnu",
        Family::Clang => "clang",
        Family::Msvc => "msvc",
    };
    let default_std = tc
        .default_std
        .as_deref()
        .map(hex_encode)
        .unwrap_or_else(|| "-".to_string());
    let text = format!(
        "{CACHE_SCHEMA}\n{key:016x}\n{family}\n{}\n{}\n{default_std}\n{}\n",
        hex_encode(&tc.version),
        hex_encode(&tc.std),
        u8::from(tc.auto_std)
    );
    // Cache corruption is harmless (the next launch probes again), so cache
    // I/O remains best effort and never prevents the prompt from opening.
    let _ = std::fs::write(dir.join(format!("{key:016x}.cache")), text);
}

fn banner_of(path: &Path) -> Result<String> {
    // MSVC has no --version; invoked bare it prints its banner to stderr and
    // exits nonzero, so accept output from either stream and ignore status.
    let out = probe_output(Command::new(path).arg("--version"))
        .with_context(|| format!("failed to run {}", path.display()))?;
    let pick = |b: &[u8]| {
        String::from_utf8_lossy(b)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let s = pick(&out.stdout);
    if !s.is_empty() && out.status.is_some_and(|st| st.success()) {
        return Ok(s);
    }
    if let Some(bare) = probe_output(&mut Command::new(path)) {
        let e = pick(&bare.stderr);
        if !e.is_empty() {
            return Ok(e);
        }
    }
    let e = pick(&out.stderr);
    if e.is_empty() {
        bail!("{} produced no version output", path.display());
    }
    Ok(e)
}

impl Toolchain {
    /// Resolve a compiler: explicit choice, then `$CC`, then PATH.
    pub fn detect(explicit: Option<&str>, std: Option<&str>) -> Result<Self> {
        let mut tried: Vec<String> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        if let Some(e) = explicit {
            names.push(e.to_string());
        } else {
            if let Some(cc) = std::env::var("CC").ok().filter(|c| !c.trim().is_empty()) {
                names.push(cc);
            }
            names.extend(CANDIDATES.iter().map(|s| s.to_string()));
        }

        for name in names {
            let path = match which::which(&name) {
                Ok(p) => p,
                Err(_) => {
                    tried.push(name);
                    continue;
                }
            };
            if let Some(tc) = load_cached(&path, std) {
                return Ok(tc);
            }
            let banner = match banner_of(&path) {
                Ok(b) => b,
                Err(_) => {
                    tried.push(name);
                    continue;
                }
            };
            let family = family_of(&path, &banner);
            let mut tc = Toolchain {
                path,
                family,
                version: banner,
                std: String::new(),
                default_std: None,
                auto_std: false,
            };
            // One compile + run answers two questions at once: does this
            // toolchain produce working executables (MSVC outside a Developer
            // Command Prompt fails here), and what its default mode is.
            match tc.probe_run(&[], VERSION_PROBE) {
                Some(out) => tc.default_std = parse_std_probe(&out, family),
                None => {
                    tried.push(format!(
                        "{} (cannot build a working program)",
                        tc.path.display()
                    ));
                    continue;
                }
            }
            if let Some(s) = std {
                if tc.supports_std(s) {
                    tc.std = s.to_string();
                } else {
                    tried.push(format!(
                        "{} (does not support requested standard {s})",
                        tc.path.display()
                    ));
                    continue;
                }
            }
            if tc.std.is_empty() && !tc.value_printer_ok() {
                // The default mode cannot host the value printer. Force the
                // oldest std that can, and own up to it via `auto_std`.
                for cand in ["c17", "c11"] {
                    if tc.supports_std(cand) {
                        tc.std = cand.to_string();
                        tc.auto_std = true;
                        break;
                    }
                }
            }
            // C11 `_Generic` is this tool's hard floor: without it no
            // expression ever prints a value, which reads as "the tool is
            // broken" with no hint why. A compiler that cannot reach it in
            // any mode is disqualified outright, not limped along with.
            if !tc.value_printer_ok() {
                tried.push(format!(
                    "{} (selected mode cannot compile the value printer)",
                    tc.path.display()
                ));
                continue;
            }
            store_cached(&tc, std);
            return Ok(tc);
        }

        bail!(
            "no usable C compiler found (tried: {}).\n\
             c-shell requires a mode capable of its C11-style value printer.\n\
             Install gcc or clang, or point c-shell at one with --cc <path>.{}",
            tried.join(", "),
            if cfg!(windows) {
                "\nOn Windows, MSVC (cl.exe) only works from a Developer Command Prompt."
            } else {
                ""
            }
        )
    }

    pub fn is_msvc(&self) -> bool {
        self.family == Family::Msvc
    }

    /// Render the argv for compiling `src` to `exe`.
    pub fn compile_args(&self, src: &Path, exe: &Path, obj_dir: &Path) -> Vec<String> {
        let mut a: Vec<String> = Vec::new();
        if self.is_msvc() {
            a.push("/nologo".into());
            if !self.std.is_empty() {
                a.push(format!("/std:{}", self.std));
            }
            a.push("/W3".into());
            // The MSVC spellings of "defined but not used yet", normal at a
            // REPL: C4101 unreferenced local, C4102 unreferenced label,
            // C4552/C4553 expression result not used (a deliberate `expr;`).
            a.push("/wd4101".into());
            a.push("/wd4102".into());
            a.push("/wd4552".into());
            a.push("/wd4553".into());
            // /TC forces C even when the temp file has an odd extension.
            a.push("/TC".into());
            a.push(src.display().to_string());
            a.push(format!("/Fe:{}", exe.display()));
            // Keep the intermediate .obj out of the working directory. The
            // trailing separator is what tells cl this is a directory.
            a.push(format!(
                "/Fo:{}{}",
                obj_dir.display(),
                std::path::MAIN_SEPARATOR
            ));
            a.push("/link".into());
        } else {
            if !self.std.is_empty() {
                a.push(format!("-std={}", self.std));
            }
            // Beginners benefit far more from seeing warnings than from a
            // quiet prompt, so these are on by default.
            a.push("-Wall".into());
            a.push("-Wextra".into());
            // ...except the -Wunused umbrella. At a REPL everything is
            // unused the moment it is defined: a new function has no caller
            // yet, and a deliberate `expr;` is an unused value by design.
            a.push("-Wno-unused".into());
            // Enabled by -Wextra and not part of the umbrella.
            a.push("-Wno-unused-parameter".into());
            a.push("-fno-diagnostics-color".into());
            a.push("-x".into());
            a.push("c".into());
            a.push(src.display().to_string());
            a.push("-o".into());
            a.push(exe.display().to_string());
            // Math lives in libm only on Unix. On Windows it is part of
            // every C runtime, and worse: clang.exe defaults to the MSVC
            // target there, where `-lm` becomes a link against a
            // nonexistent m.lib. The probes never pass -lm, so that
            // failure would surface not at detection but at the user's
            // first evaluation.
            if !cfg!(windows) {
                a.push("-lm".into());
            }
        }
        a
    }

    fn probe(&self, args: &[&str], body: &str) -> bool {
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(_) => return false,
        };
        let src = dir.path().join("probe.c");
        if std::fs::write(&src, body).is_err() {
            return false;
        }
        let exe = dir
            .path()
            .join(format!("probe{}", std::env::consts::EXE_SUFFIX));
        let mut cmd = Command::new(&self.path);
        if self.is_msvc() {
            cmd.arg("/nologo").arg("/TC");
            cmd.args(args);
            cmd.arg(&src).arg(format!("/Fe:{}", exe.display()));
            cmd.arg(format!(
                "/Fo:{}{}",
                dir.path().display(),
                std::path::MAIN_SEPARATOR
            ));
        } else {
            cmd.args(args);
            cmd.arg("-x").arg("c").arg(&src).arg("-o").arg(&exe);
        }
        probe_output(&mut cmd).is_some_and(|c| {
            c.status.is_some_and(|st| st.success()) && !(self.is_msvc() && msvc_ignored_option(&c))
        })
    }

    /// Like `probe`, but also runs the produced executable and returns its
    /// stdout. `None` means compile, link or run failed.
    fn probe_run(&self, args: &[&str], body: &str) -> Option<String> {
        let dir = tempfile::tempdir().ok()?;
        let src = dir.path().join("probe.c");
        std::fs::write(&src, body).ok()?;
        let exe = dir
            .path()
            .join(format!("probe{}", std::env::consts::EXE_SUFFIX));
        let mut cmd = Command::new(&self.path);
        if self.is_msvc() {
            cmd.arg("/nologo").arg("/TC");
            cmd.args(args);
            cmd.arg(&src).arg(format!("/Fe:{}", exe.display()));
            cmd.arg(format!(
                "/Fo:{}{}",
                dir.path().display(),
                std::path::MAIN_SEPARATOR
            ));
        } else {
            cmd.args(args);
            cmd.arg("-x").arg("c").arg(&src).arg("-o").arg(&exe);
        }
        let ok = probe_output(&mut cmd).is_some_and(|c| c.status.is_some_and(|st| st.success()));
        if !ok {
            return None;
        }
        let run = probe_output(&mut Command::new(&exe))?;
        if !run.status.is_some_and(|st| st.success()) {
            return None;
        }
        Some(String::from_utf8_lossy(&run.stdout).into_owned())
    }

    fn supports_std(&self, std: &str) -> bool {
        let flag = if self.is_msvc() {
            format!("/std:{std}")
        } else {
            format!("-std={std}")
        };
        self.probe(&[&flag], "int main(void){return 0;}")
    }

    /// Can the value-printing runtime compile under the currently chosen
    /// mode? Probed with the actual flag that will be used, so a forced
    /// `--std` is judged as-is rather than assumed fine.
    fn value_printer_ok(&self) -> bool {
        let flag = if self.std.is_empty() {
            None
        } else if self.is_msvc() {
            Some(format!("/std:{}", self.std))
        } else {
            Some(format!("-std={}", self.std))
        };
        match &flag {
            Some(f) => self.probe(&[f], VALUE_PRINTER_PROBE),
            None => self.probe(&[], VALUE_PRINTER_PROBE),
        }
    }

    pub fn describe(&self) -> String {
        let std = match (&self.std, &self.default_std) {
            (s, _) if s.is_empty() => match &self.default_std {
                Some(d) => format!("default std {d}"),
                None => "default std".to_string(),
            },
            (s, Some(d)) if self.auto_std => {
                format!("-std={s}, auto-raised: default {d} lacks _Generic")
            }
            (s, _) => format!("-std={s}"),
        };
        format!("{} ({})", self.version, std)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_text_round_trips_utf8_and_empty_strings() {
        for value in ["", "gcc (GCC) 16.1.1", "编译器"] {
            assert_eq!(hex_decode(&hex_encode(value)).as_deref(), Some(value));
        }
        assert!(hex_decode("0").is_none());
        assert!(hex_decode("zz").is_none());
    }

    #[test]
    fn recognizes_msvc_ignored_option_diagnostics() {
        assert!(ignored_msvc_option_text(
            "cl : Command line warning D9002 : ignoring unknown option '/std:bogus'"
        ));
        assert!(ignored_msvc_option_text(
            "warning: argument unused during compilation: '/std:bogus'"
        ));
        assert!(!ignored_msvc_option_text(
            "warning C4101: unreferenced local variable"
        ));
    }

    #[test]
    fn requested_standard_is_part_of_cache_identity() {
        let exe = std::env::current_exe().expect("current test executable");
        assert_ne!(cache_key(&exe, None), cache_key(&exe, Some("c17")));
        assert_ne!(cache_key(&exe, Some("c17")), cache_key(&exe, Some("c23")));
    }

    #[test]
    fn parses_modern_gcc_default() {
        assert_eq!(
            parse_std_probe("202311 1\n", Family::Gnu).as_deref(),
            Some("gnu23")
        );
        // gcc's pre-release C2x value maps to the same label.
        assert_eq!(
            parse_std_probe("202000 1\n", Family::Gnu).as_deref(),
            Some("gnu23")
        );
    }

    #[test]
    fn distinguishes_strict_from_gnu_dialect() {
        assert_eq!(
            parse_std_probe("201710 0\n", Family::Clang).as_deref(),
            Some("c17")
        );
        assert_eq!(
            parse_std_probe("201710 1\n", Family::Clang).as_deref(),
            Some("gnu17")
        );
    }

    #[test]
    fn msvc_legacy_mode_has_no_gnu_dialect() {
        // MSVC never defines __STRICT_ANSI__; the gnu bit is meaningless there.
        assert_eq!(
            parse_std_probe("0 1\n", Family::Msvc).as_deref(),
            Some("c89")
        );
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_std_probe("", Family::Gnu), None);
        assert_eq!(parse_std_probe("not numbers\n", Family::Gnu), None);
    }

    #[test]
    fn family_detection_covers_windows_compilers() {
        // clang-cl IS clang, but speaks the MSVC dialect: the name must win
        // over the banner.
        assert_eq!(
            family_of(
                Path::new("C:/LLVM/bin/clang-cl.exe"),
                "clang version 22.1.6"
            ),
            Family::Msvc
        );
        assert_eq!(
            family_of(Path::new("cl.exe"), "Microsoft (R) C/C++"),
            Family::Msvc
        );
        assert_eq!(
            family_of(Path::new("/usr/bin/clang"), "clang version 22.1.6"),
            Family::Clang
        );
        assert_eq!(
            family_of(Path::new("/mingw64/bin/gcc.exe"), "gcc (GCC) 14.2"),
            Family::Gnu
        );
        // Apple Command Line Tools may expose a gcc-named Clang driver.
        assert_eq!(
            family_of(Path::new("/usr/bin/gcc"), "Apple clang version 21.0.0"),
            Family::Clang
        );
    }

    #[test]
    fn libm_is_linked_only_on_unix() {
        // On Windows, clang defaults to the MSVC target where `-lm` becomes
        // a link against a nonexistent m.lib; MinGW keeps math in the CRT.
        let tc = Toolchain {
            path: "gcc".into(),
            family: Family::Gnu,
            version: String::new(),
            std: "c17".into(),
            default_std: None,
            auto_std: false,
        };
        let args = tc.compile_args(Path::new("in.c"), Path::new("out"), Path::new("."));
        assert_eq!(args.contains(&"-lm".to_string()), cfg!(unix));
    }
}
