//! Finding a C compiler and speaking its flag dialect.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

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
    pub extra: Vec<String>,
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

/// The one hard requirement on the language mode: without `_Generic` the
/// value printer cannot compile and expressions stop printing entirely.
const GENERIC_PROBE: &str =
    "int main(void){int v = _Generic(1, int: 1, default: 0); return v - 1;}";

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
    // Never trust the executable's name for gcc-vs-clang. On macOS both `cc`
    // and `gcc` are usually Apple clang, and the banner is the only honest
    // signal.
    if banner.to_ascii_lowercase().contains("clang") {
        return Family::Clang;
    }
    Family::Gnu
}

fn banner_of(path: &Path) -> Result<String> {
    // MSVC has no --version; invoked bare it prints its banner to stderr and
    // exits nonzero, so accept output from either stream and ignore status.
    let out = Command::new(path)
        .arg("--version")
        .output()
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
    if !s.is_empty() && out.status.success() {
        return Ok(s);
    }
    let bare = Command::new(path).output();
    if let Ok(bare) = bare {
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
    pub fn detect(explicit: Option<&str>, std: Option<&str>, extra: Vec<String>) -> Result<Self> {
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
                extra: extra.clone(),
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
                // Explicitly requested. If unsupported, fall through with the
                // compiler default; the banner makes the outcome visible.
                if tc.supports_std(s) {
                    tc.std = s.to_string();
                }
            }
            if tc.std.is_empty() && !tc.generic_ok() {
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
            if !tc.generic_ok() {
                tried.push(format!(
                    "{} (too old: no C11 _Generic in any mode)",
                    tc.path.display()
                ));
                continue;
            }
            return Ok(tc);
        }

        bail!(
            "no usable C compiler found (tried: {}).\n\
             c-shell requires C11: value printing depends on _Generic.\n\
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
            // REPL: C4101 unreferenced local, C4102 unreferenced label.
            a.push("/wd4101".into());
            a.push("/wd4102".into());
            // /TC forces C even when the temp file has an odd extension.
            a.push("/TC".into());
            a.extend(self.extra.clone());
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
            a.extend(self.extra.clone());
            a.push("-x".into());
            a.push("c".into());
            a.push(src.display().to_string());
            a.push("-o".into());
            a.push(exe.display().to_string());
            a.push("-lm".into());
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
        matches!(cmd.output(), Ok(o) if o.status.success())
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
        let ok = matches!(cmd.output(), Ok(o) if o.status.success());
        if !ok {
            return None;
        }
        let run = Command::new(&exe).output().ok()?;
        if !run.status.success() {
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
    fn generic_ok(&self) -> bool {
        let flag = if self.std.is_empty() {
            None
        } else if self.is_msvc() {
            Some(format!("/std:{}", self.std))
        } else {
            Some(format!("-std={}", self.std))
        };
        match &flag {
            Some(f) => self.probe(&[f], GENERIC_PROBE),
            None => self.probe(&[], GENERIC_PROBE),
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
}
