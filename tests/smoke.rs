//! End-to-end smoke tests: drive the real binary against the real C compiler.
//!
//! The unit tests are all pure functions, so this file is the only place the
//! toolchain detection, MSVC flag dialect, crash isolation and value printing
//! actually get exercised. In CI it runs on Linux (gcc), macOS (clang) and
//! Windows (MinGW and MSVC), which is precisely the coverage a dev machine
//! cannot provide.

use std::io::Write;
use std::process::{Command, Stdio};

/// Feed `lines` to c-shell on stdin, return everything it printed to stdout.
fn run(lines: &[&str]) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_c-shell"))
        .arg("--no-color")
        .arg("--timeout")
        .arg("15")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start c-shell");

    let mut stdin = child.stdin.take().expect("stdin piped");
    for l in lines {
        writeln!(stdin, "{l}").expect("write to c-shell stdin");
    }
    drop(stdin); // EOF ends the REPL loop

    let out = child.wait_with_output().expect("wait for c-shell");
    assert!(
        out.status.success(),
        "c-shell exited abnormally: {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn evaluates_expressions_against_session_state() {
    let out = run(&["int x = 41;", "x + 1"]);
    assert!(out.contains("Out[2]: 42"), "unexpected output:\n{out}");
}

#[test]
fn prints_floating_point_and_bool() {
    let out = run(&["3.0 / 2", "1 == 2"]);
    assert!(out.contains("Out[1]: 1.5"), "unexpected output:\n{out}");
    assert!(out.contains("Out[2]: 0"), "unexpected output:\n{out}");
}

#[test]
fn defines_and_calls_functions() {
    let out = run(&["int add(int a, int b) { return a + b; }", "add(3, 4)"]);
    assert!(out.contains("Out[2]: 7"), "unexpected output:\n{out}");
}

#[test]
fn statements_execute_and_mutate_state() {
    let out = run(&["int n = 1;", "n = n * 10;", "n"]);
    assert!(out.contains("Out[3]: 10"), "unexpected output:\n{out}");
}

#[test]
fn survives_compile_errors() {
    let out = run(&["int x = ;", "1 + 1"]);
    assert!(out.contains("error"), "no diagnostic shown:\n{out}");
    assert!(out.contains("Out[2]: 2"), "session did not survive:\n{out}");
}

#[test]
fn survives_a_crashing_input() {
    // The null write must crash on every platform, be reported, and be kept
    // out of the session so the next input still works.
    let out = run(&["int x = 7;", "char *p = 0; *p = 1;", "x"]);
    assert!(out.contains("Out[3]: 7"), "session did not survive:\n{out}");
}

#[test]
fn program_output_is_shown_once() {
    // Replay must not re-show output from earlier inputs.
    let out = run(&["puts(\"marker-once\");", "1 + 1"]);
    let hits = out.matches("marker-once").count();
    assert_eq!(hits, 1, "output replayed {hits} times:\n{out}");
}
