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
    run_with_timeout(15, lines)
}

fn run_with_timeout(secs: u32, lines: &[&str]) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_c-shell"))
        .arg("--no-color")
        .arg("--timeout")
        .arg(secs.to_string())
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
fn defining_things_without_using_them_warns_nothing() {
    // At a REPL everything is unused the moment it is defined; the -Wunused
    // family must stay suppressed or every function definition and every
    // deliberate `expr;` drowns the prompt in noise.
    let out = run(&[
        "int foo(int x) { return x; }",
        "int unused_yet = 1;",
        "unused_yet + 1;",
    ]);
    assert!(
        !out.contains("warning"),
        "unused-style warning leaked:\n{out}"
    );
}

#[test]
fn real_warnings_still_show() {
    // Suppressing REPL noise must not take genuine diagnostics with it.
    //
    // The comparison must go through *variables*: on a constant expression
    // like `-1 > 0u` clang folds and deliberately stays silent, and this
    // test's job is to pass on every compiler family. gcc and clang warn
    // via -Wsign-compare, MSVC via C4018 — all within default levels.
    let out = run(&["int neg = -1;", "unsigned pos = 0;", "neg > pos"]);
    assert!(
        out.contains("warning") || out.contains("C4018"),
        "sign-compare warning disappeared:\n{out}"
    );
}

#[test]
fn disguised_calls_commit_their_side_effects() {
    // P0 regression: `f/**/()` evaluated correctly but was misjudged as a
    // pure expression and never committed, silently losing session state.
    // The counter lives inside f to keep the scenario portable: a
    // file-scope function cannot see main's locals (gcc would quietly
    // accept that as a nested function; clang rejects it outright).
    let out = run(&[
        "int f(void) { static int n; return ++n; }",
        "int (*fp)(void) = f;",
        "f/**/()",
        "f /* comment */ ()",
        "(f)()",
        "(*fp)()",
        "f()",
    ]);
    assert!(out.contains("Out[3]: 1"), "first call:\n{out}");
    // The last call sees all four disguised calls replayed before it.
    assert!(
        out.contains("Out[7]: 5"),
        "a disguised call was dropped:\n{out}"
    );
}

#[test]
fn eval_flag_prints_bare_value_and_exits_clean() {
    let out = Command::new(env!("CARGO_BIN_EXE_c-shell"))
        .args(["-e", "1 + 1"])
        .output()
        .expect("run c-shell -e");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "2");
}

#[test]
fn eval_flag_failure_sets_exit_code_and_uses_stderr() {
    let out = Command::new(env!("CARGO_BIN_EXE_c-shell"))
        .args(["-e", "no_such_variable"])
        .output()
        .expect("run c-shell -e");
    assert!(!out.status.success(), "failure must be visible to scripts");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("error"),
        "diagnostic missing from stderr"
    );
}

#[test]
fn script_mode_accumulates_multi_line_definitions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("t.csh");
    std::fs::write(
        &path,
        "// a comment-only line is skipped
int mul(int a, int b)
{
    return a * b;
}
mul(6, 7)
",
    )
    .expect("write script");
    let out = Command::new(env!("CARGO_BIN_EXE_c-shell"))
        .arg("--script")
        .arg(&path)
        .output()
        .expect("run c-shell --script");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Out[2]: 42"),
        "unexpected output:\n{stdout}"
    );
}

#[test]
fn piped_stdin_keeps_else_with_preceding_if() {
    let out = run(&[
        "int x = 0;",
        "if (x) {",
        "    puts(\"wrong\");",
        "}",
        "else {",
        "    puts(\"else-ok\");",
        "}",
        "1 + 1",
    ]);
    assert!(out.contains("else-ok"), "else was detached from if:\n{out}");
    assert!(
        out.contains("Out[3]: 2"),
        "unexpected input numbering:\n{out}"
    );
}

#[test]
fn piped_stdin_accumulates_conditional_preprocessor_group() {
    let out = run(&[
        "#if 1",
        "#define CS_TEST_ANSWER 42",
        "#else",
        "#define CS_TEST_ANSWER 0",
        "#endif",
        "CS_TEST_ANSWER",
    ]);
    assert!(out.contains("Out[2]: 42"), "unexpected output:\n{out}");
}

#[test]
fn piped_stdin_accumulates_control_body_and_do_while() {
    let out = run(&[
        "int n = 0;",
        "if (1)",
        "    n++;",
        "do {",
        "    n++;",
        "} while (0);",
        "n",
    ]);
    assert!(out.contains("Out[4]: 2"), "unexpected output:\n{out}");
}

#[test]
fn piped_stdin_accumulates_multi_line_definitions() {
    // The batch reader gives pipes the same multi-line handling the
    // interactive validator provides at a terminal.
    let out = run(&["int sq(int a)", "{", "    return a * a;", "}", "sq(9)"]);
    assert!(out.contains("Out[2]: 81"), "unexpected output:\n{out}");
}

#[test]
fn type_magic_reports_expression_types_without_evaluating_them() {
    let out = run(&[
        "int n = 0;",
        "const char *message = \"hello\";",
        "%type n",
        "%type message",
        "%type n + 0.5",
        "%type n++",
        "n",
    ]);
    let lines: Vec<_> = out.lines().collect();
    assert!(lines.contains(&"int"), "integer type missing:\n{out}");
    assert!(
        lines.contains(&"const char *"),
        "qualified pointer type missing:\n{out}"
    );
    assert!(lines.contains(&"double"), "expression type missing:\n{out}");
    assert!(
        out.contains("Out[3]: 0"),
        "%type evaluated its controlling expression or consumed an input number:\n{out}"
    );
}

#[test]
fn type_magic_recognizes_named_and_typedef_aggregates() {
    let out = run(&[
        "struct Pair { int x, y; };",
        "union Number { int i; double d; };",
        "typedef struct { int code; } Result;",
        "typedef union Number Numeric;",
        "struct Pair pair = { 1, 2 };",
        "Numeric number = { 3 };",
        "Result result = { 0 };",
        "%type pair",
        "%type number",
        "%type result",
    ]);
    let lines: Vec<_> = out.lines().collect();
    assert!(
        lines.contains(&"Struct Pair"),
        "named struct type missing:\n{out}"
    );
    assert!(
        lines.contains(&"Union Number"),
        "named union type missing:\n{out}"
    );
    assert!(
        lines.contains(&"Struct Result"),
        "anonymous typedef name missing:\n{out}"
    );
}

#[test]
fn clear_erases_screen_without_resetting_session() {
    let out = run(&["int kept = 42;", "%clear", "kept"]);
    assert!(
        out.contains("\x1b[2J\x1b[H"),
        "clear sequence missing:\n{out:?}"
    );
    assert!(
        out.contains("Out[2]: 42"),
        "clear changed session state or input numbering:\n{out:?}"
    );
}

#[test]
fn history_separates_input_numbers_from_magic_commands() {
    let out = run(&["int h1 = 41;", "%src", "h1 + 1", "%history"]);
    assert!(out.contains("In[  1] int h1 = 41;"), "numbered row:\n{out}");
    assert!(out.contains("In[  2] h1 + 1"), "numbered row:\n{out}");
    // Magic commands are in the history but never consumed an input number.
    assert!(out.contains("--    %src"), "magic row:\n{out}");
}

#[test]
fn piped_stdin_shows_no_banner_or_prompt_noise() {
    let out = run(&["1 + 1"]);
    assert!(
        !out.contains("____"),
        "ASCII banner leaked into pipe:\n{out}"
    );
    assert!(!out.contains("bye"), "interactive farewell leaked:\n{out}");
    assert!(out.contains("Out[1]: 2"), "unexpected output:\n{out}");
}

#[cfg(unix)]
#[test]
fn timeout_kills_forked_descendants_too() {
    // The program forks; parent and child both hang forever, and the child
    // keeps the stdout pipe open. Killing only the direct child would leave
    // the REPL blocked on its reader thread — the whole process group must
    // die. The final input proves the prompt survived.
    let out = run_with_timeout(
        2,
        &[
            "#include <unistd.h>",
            "if (fork() == 0) { for (;;) pause(); } for (;;) pause();",
            "1 + 1",
        ],
    );
    assert!(out.contains("killed after 2s"), "no timeout report:\n{out}");
    assert!(out.contains("Out[3]: 2"), "REPL did not survive:\n{out}");
}

#[test]
fn successful_early_exit_is_not_committed() {
    // Exit status 0 is not enough: the generated completion marker must be
    // reached, or replaying this statement would wedge every later input.
    let out = run(&["exit(0);", "1 + 1"]);
    assert!(
        out.contains("exited before the input completed"),
        "missing protocol failure:\n{out}"
    );
    assert!(out.contains("Out[2]: 2"), "session stayed wedged:\n{out}");
}

#[test]
fn unsupported_value_category_is_explained_and_not_misclassified() {
    let out = run(&["((struct { int x; }){ 1 })", "1 + 1"]);
    assert!(
        out.contains("value category has no printer"),
        "missing printer explanation:\n{out}"
    );
    assert!(
        out.contains("Out[2]: 2"),
        "session did not continue:\n{out}"
    );
}

#[test]
fn unsupported_explicit_standard_is_an_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_c-shell"))
        .args(["--std", "definitely-not-a-c-standard", "-e", "1"])
        .output()
        .expect("run c-shell with invalid standard");
    assert!(!out.status.success(), "invalid --std was silently ignored");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("does not support requested standard"),
        "unexpected diagnostic:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn program_output_is_shown_once() {
    // Replay must not re-show output from earlier inputs.
    let out = run(&["puts(\"marker-once\");", "1 + 1"]);
    let hits = out.matches("marker-once").count();
    assert_eq!(hits, 1, "output replayed {hits} times:\n{out}");
}
