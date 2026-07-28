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

fn run_evals_with_stdin(codes: &[&str], input: &str) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_c-shell"));
    command.arg("--no-color");
    for code in codes {
        command.args(["-e", code]);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start c-shell -e with program stdin");
    child
        .stdin
        .take()
        .expect("program stdin")
        .write_all(input.as_bytes())
        .expect("write program stdin");
    child.wait_with_output().expect("wait for c-shell -e")
}

#[test]
fn scanf_stdin_tape_replays_functions_and_loops_without_reading_again() {
    let output = run_evals_with_stdin(
        &[
            "int read_number(void) { int value; scanf(\"%d\", &value); return value; }",
            "int first = read_number();",
            "int second = 0; for (int k = 0; k < 2; ++k) { second += read_number(); }",
            "first + second",
        ],
        "3\n4\n5\n",
    );
    assert!(
        output.status.success(),
        "stdin tape session failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "12");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("external side-effect"),
        "supported scanf still used the generic replay warning"
    );

    let divergent = run_evals_with_stdin(
        &[
            "int read_once(void) { int value; scanf(\"%d\", &value); return value; }",
            "int kept = read_once();",
            "int read_once(void) { return 99; }",
            "kept",
        ],
        "3\n",
    );
    assert!(!divergent.status.success(), "divergent tape was accepted");
    assert!(
        String::from_utf8_lossy(&divergent.stderr).contains("stdin tape diverged"),
        "missing divergence diagnostic:\n{}",
        String::from_utf8_lossy(&divergent.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&divergent.stdout).trim(),
        "3",
        "rejected replacement corrupted the retained tape/session"
    );
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
fn explicit_u8_literals_get_a_safe_utf8_preview_only() {
    // Before C23 a u8 literal has char elements and initially goes through the
    // ordinary string printer. The explicit-source probe must still refine it
    // into validated UTF-8 rather than depending on an address-shaped result.
    // Invalid UTF-8 payload fallback is tested below the C compiler boundary:
    // Clang/MSVC may reject deliberately malformed u8 literals as source.
    // A universal character name asks every compiler to encode the character;
    // spelling its UTF-8 bytes as \xF0... is not portable before C23.
    let out = run(&[
        r#"u8"\U0001F642""#,
        "unsigned char bytes[] = { 240, 159, 153, 130, 0 };",
        "bytes",
    ]);
    assert!(
        out.contains("Out[1]: u8\"🙂\"\ncode units: {0xf0, 0x9f, 0x99, 0x82, 0x00}"),
        "valid explicit UTF-8 literal was not rendered as text:\n{out}"
    );
    assert!(
        out.contains("Out[3]: {240, 159, 153, 130, 0}"),
        "ordinary unsigned char array must stay numeric:\n{out}"
    );
    assert_eq!(
        out.matches("u8\"🙂\"").count(),
        1,
        "numeric byte array was also treated as explicit UTF-8:\n{out}"
    );
}

#[test]
fn unicode_magics_decode_explicit_code_units_without_changing_session() {
    let out = run(&[
        "int untouched = 0;",
        r#"%utf8 ((++untouched == 1) ? u8"A\u597D\U0001F600" : u8"twice")"#,
        r#"%utf16 u"A\u597D\U0001F600""#,
        r#"%utf32 U"A\u597D\U0001F600""#,
        "%utf8 -n 3 (const unsigned char[]){0x41, 0, 0x42}",
        "%utf16 (const unsigned short[]){0xd800, 0}",
        "%utf32 (const unsigned int[]){0x110000, 0}",
        "%utf8 (const unsigned int[]){0x41, 0}",
        "%utf8 42",
        r#"%utf8 -n 4097 u8"x""#,
        "untouched",
    ]);
    assert!(
        out.contains("text: u8\"A好😀\"")
            && out.contains("code units: {0x41, 0xe5, 0xa5, 0xbd, 0xf0, 0x9f, 0x98, 0x80, 0x00}"),
        "UTF-8 magic failed:\n{out}"
    );
    assert!(
        out.contains("text: u\"A好😀\"") && out.contains("0x0041, 0x597d, 0xd83d, 0xde00, 0x0000"),
        "UTF-16 magic failed:\n{out}"
    );
    assert!(
        out.contains("text: U\"A好😀\"")
            && out.contains("0x00000041, 0x0000597d, 0x0001f600, 0x00000000"),
        "UTF-32 magic failed:\n{out}"
    );
    assert!(
        out.contains("text: u8\"A\\0B\""),
        "-n did not preserve an embedded NUL:\n{out}"
    );
    assert!(
        out.contains("unpaired UTF-16 high surrogate at index 0")
            && out.contains("invalid UTF-32 scalar value at index 0"),
        "invalid Unicode was silently replaced:\n{out}"
    );
    assert!(
        out.contains("expected 1-byte code units")
            && out.contains("%utf8 supports pointers and arrays of integer code-unit types")
            && out.contains("-n is limited to 4096 code units"),
        "Unicode magic error paths were not actionable:\n{out}"
    );
    assert!(
        out.contains("Out[2]: 0"),
        "Unicode magic evaluated more than once, retained its expression, or consumed an input number:\n{out}"
    );
}

#[cfg(not(windows))]
#[test]
fn strict_iso_mode_keeps_the_runtime_timer_available() {
    let out = Command::new(env!("CARGO_BIN_EXE_c-shell"))
        .args(["--no-color", "--std", "c11", "-e", "1"])
        .output()
        .expect("run c-shell in strict C11 mode");
    assert!(
        out.status.success(),
        "strict ISO mode broke generated runtime:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1");
}

#[test]
fn complete_main_definition_gets_actionable_guidance() {
    let out = run(&[
        "int main(void) { printf(\"hello-main\\n\"); return 0; }",
        "1 + 1",
    ]);
    assert!(
        out.contains("already provides main()")
            && out.contains("enter the statements from its body directly"),
        "missing main guidance:\n{out}"
    );
    assert!(
        !out.contains("warning"),
        "-Wmain-style noise leaked:\n{out}"
    );
    assert!(
        !out.lines().any(|line| line == "hello-main"),
        "user main was unexpectedly executed:\n{out}"
    );
    assert!(
        out.contains("Out[2]: 2"),
        "session did not continue after guidance:\n{out}"
    );
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
fn local_redeclaration_opens_a_shadowing_scope_and_undo_restores_it() {
    let out = run(&["int x = 1;", "x = 5;", "int x = 2;", "x", "%undo", "x"]);
    assert!(
        out.contains("opened a nested scope"),
        "shadowing retry was not reported:\n{out}"
    );
    assert!(out.contains("Out[4]: 2"), "new binding not visible:\n{out}");
    assert!(
        out.contains("Out[5]: 5"),
        "undo did not restore the old binding:\n{out}"
    );
}

#[test]
fn file_scope_redefinition_replaces_in_place_portably() {
    let out = run(&[
        "int f(int n) { return n * 2; }",
        "f(3)",
        "int f(int n) { return n * 3; }",
        "f(3)",
    ]);
    assert!(
        out.contains("Out[2]: 6"),
        "old function result missing:\n{out}"
    );
    assert!(
        out.contains("replaced previous file-scope definition"),
        "replacement was not reported:\n{out}"
    );
    assert!(out.contains("Out[4]: 9"), "new function not used:\n{out}");
    assert!(
        !out.contains("kept inside main"),
        "function was silently demoted:\n{out}"
    );
}

#[test]
fn tag_redefinition_replaces_the_old_type_when_retained_session_still_compiles() {
    let out = run(&[
        "struct Box { int old_value; };",
        "struct Box box = { 3 };",
        "struct Box { int new_value; };",
        "box.new_value",
    ]);
    assert!(
        out.contains("replaced previous file-scope definition"),
        "tag definition was not replaced:\n{out}"
    );
    assert!(
        out.contains("Out[4]: 3"),
        "retained declaration did not use the new type:\n{out}"
    );
}

#[test]
fn file_scope_replacement_is_undoable_and_functions_never_demote() {
    let out = run(&[
        "int g(int n) { return n * 2; }",
        "int g(int n) { return n * 3; }",
        "%undo",
        "g(3)",
        "int local = 7;",
        "int illegal(void) { return local; }",
        "local",
    ]);
    assert!(
        out.contains("Out[3]: 6"),
        "undo did not restore the old function:\n{out}"
    );
    assert!(
        out.contains("error") && out.contains("local"),
        "file-scope failure was hidden by nested-function demotion:\n{out}"
    );
    assert!(
        out.contains("Out[6]: 7"),
        "session did not survive the rejected function:\n{out}"
    );
}

#[test]
fn compound_literal_initializer_stays_at_block_scope() {
    let out = run(&[
        "typedef struct { int x, y; } Point;",
        "(int){ 7 }",
        "Point p = (Point){ 1, 2 };",
        "p.x + p.y",
    ]);
    assert_eq!(
        out.matches("added at file scope").count(),
        1,
        "compound literal declaration was mistaken for a function:\n{out}"
    );
    assert!(
        out.contains("Out[2]: 7"),
        "scalar compound literal stopped being an expression:\n{out}"
    );
    assert!(out.contains("Out[4]: 3"), "unexpected result:\n{out}");
}

#[test]
fn struct_printing_never_implicitly_dereferences_pointer_members() {
    let out = run(&[
        "struct Person { char *name; int age; int scores[3]; struct Person *next; };",
        "struct Person p = { (char *)(uintptr_t)0x1234, 30, { 0, 7, 0 }, NULL };",
        "p",
        "p.name = \"Alice\";",
        "p.name",
        "struct Person *person_ptr = &p;",
        "person_ptr",
        "*person_ptr",
        "struct Pair { int x, y; };",
        "struct Pair pair = { 1, 2 };",
        "pair",
        "1 + 1",
    ]);
    assert!(
        out.contains(".name = (void *)") && out.contains("1234"),
        "pointer member was not rendered as an address:\n{out}"
    );
    assert!(
        out.contains("Out[5]: \"Alice\""),
        "an explicit char-pointer member did not use string formatting:\n{out}"
    );
    assert_eq!(
        out.matches(".age = 30").count(),
        2,
        "a struct value and explicit *ptr should both expand:\n{out}"
    );
    assert!(
        out.contains(".scores = {0, 7, 0}"),
        "array member missing:\n{out}"
    );
    assert!(out.contains(".next = NULL"), "NULL member missing:\n{out}");
    assert!(
        out.contains("<struct Pair raw bytes:"),
        "unsupported multi-declarator did not fall back safely:\n{out}"
    );
    assert!(
        out.contains("Out[12]: 2"),
        "printing the invalid pointer dereferenced or crashed:\n{out}"
    );
}

#[test]
fn survives_compile_errors() {
    let out = run(&["int x = ;", "1 + 1"]);
    assert!(out.contains("error"), "no diagnostic shown:\n{out}");
    assert!(out.contains("Out[2]: 2"), "session did not survive:\n{out}");
}

#[test]
fn diagnostics_hide_generated_wrappers_and_marker_tokens() {
    let out = run(&["int number = 1;", "numbr + 1", "1 + 1"]);
    assert!(out.contains("numbr"), "real diagnostic missing:\n{out}");
    assert!(
        !out.contains("CS_PRINT") && !out.contains("CS_MARK"),
        "generated source leaked:\n{out}"
    );
    assert!(
        !out.contains("'do'") && !out.contains("‘do’") && !out.contains("`do'"),
        "generated marker expansion leaked:\n{out}"
    );
    assert!(
        !out.contains("expected ';'") && !out.contains("expected ‘;’"),
        "synthetic statement-fallback diagnostic leaked:\n{out}"
    );
    assert!(
        out.contains("Out[3]: 2"),
        "session did not survive cleaned diagnostic:\n{out}"
    );

    let incomplete = run(&["1 +", "1 + 1"]);
    assert!(
        incomplete.contains("expected expression at end of input"),
        "incomplete expression lost its diagnostic:\n{incomplete}"
    );
    assert!(
        !incomplete.contains("before ‘)’") && !incomplete.contains("before ')'"),
        "closing expression wrapper leaked:\n{incomplete}"
    );
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
    // file-scope function cannot see main's locals, and c-shell deliberately
    // never demotes function-shaped input into GCC's nested-function mode.
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

    // A tag's closing brace does not complete its declaration: the following
    // semicolon must stay in the same input rather than becoming a new In[n].
    let tag = run(&[
        "struct P { int x; int y; }",
        ";",
        "struct P point = { 3, 4 };",
        "point",
    ]);
    assert!(
        tag.contains("Out[3]: { .x = 3, .y = 4 }"),
        "tag definition was submitted before its semicolon:\n{tag}"
    );
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
fn bits_magic_inspects_object_representations_and_evaluates_once() {
    let out = run(&[
        "int n = 0;",
        "%bits ++n",
        "%bits 0.1f",
        "%bits (unsigned char)255",
        "%Bits 0.1f",
        "%Bits (unsigned char)255",
        "%BITS 1",
        "%bits (struct { int x; }){1}",
        "n",
    ])
    .replace("\r\n", "\n");
    assert!(
        out.contains("type: int\nsize: 4 bytes\nvalue: 1\nhex: 0x00000001\nbinary: 00000000"),
        "integer representation missing:\n{out}"
    );
    assert!(
        out.contains("type: float")
            && out.contains("hex: 0x3dcccccd")
            && out.contains("sign: 0")
            && out.contains("exponent: 123 (-4)")
            && out.contains("fraction: 0x4ccccd"),
        "IEEE-754 fields missing:\n{out}"
    );
    assert!(
        out.contains("type: unsigned char")
            && out.contains("binary: 11111111")
            && out.contains("memory: ff"),
        "byte representation missing:\n{out}"
    );
    assert!(
        out.contains("hex: 0X3DCCCCCD")
            && out.contains("memory: CD CC CC 3D")
            && out.contains("fraction: 0X4CCCCD")
            && out.contains("hex: 0XFF"),
        "uppercase hexadecimal representation missing:\n{out}"
    );
    assert!(
        out.contains("unknown command %BITS"),
        "unexpected case-insensitive magic matching:\n{out}"
    );
    assert!(
        out.contains("byte order: ") && out.contains("Out[2]: 0"),
        "%bits evaluated its expression more than once, retained it, or consumed an input number:\n{out}"
    );
    assert!(
        out.contains("%bits supports standard scalar values and pointers to scalar types"),
        "unsupported type diagnostic missing:\n{out}"
    );
}

#[test]
fn where_magic_uses_the_iso_index_without_consuming_input_number() {
    let out = run(&[
        "int marker = 7;",
        "%where printf",
        "%where sqrt",
        "%where gets",
        "%where ckd_add",
        "%where getline",
        "%where",
        "marker",
    ])
    .replace("\r\n", "\n");
    assert!(
        out.contains("name: printf")
            && out.contains("| header    | documentation")
            && out.contains("| <stdio.h> | https://en.cppreference.com/c/header/stdio")
            && out.contains("signature: int printf("),
        "stdio lookup missing:\n{out}"
    );
    assert!(
        out.contains("name: sqrt")
            && out.contains("| <math.h>   | https://en.cppreference.com/c/header/math")
            && out.contains("| <tgmath.h> | https://en.cppreference.com/c/header/tgmath")
            && out.contains("kind: function / type-generic macro"),
        "multi-header lookup missing:\n{out}"
    );
    assert!(
        out.contains("name: gets")
            && out.contains("removed in C11")
            && out.contains("note: deprecated;"),
        "removed/deprecated metadata missing:\n{out}"
    );
    assert!(
        out.contains("name: ckd_add")
            && out.contains("| <stdckdint.h> | https://en.cppreference.com/c/header/stdckdint")
            && out.contains("ISO C availability: C23 and later"),
        "C23 lookup missing:\n{out}"
    );
    assert!(
        out.contains("getline was not found in c-shell's ISO C standard library index"),
        "POSIX identifier was not excluded:\n{out}"
    );
    assert!(
        out.contains("usage: %where <identifier>"),
        "bare %where did not show usage:\n{out}"
    );
    assert!(
        out.contains("Out[2]: 7"),
        "%where changed session state or consumed an input number:\n{out}"
    );
}

#[test]
fn src_defaults_to_user_view_and_raw_keeps_scaffolding() {
    let user = run(&["int source_value = 7;", "%src"]);
    assert!(user.contains("int main(void)"), "main missing:\n{user}");
    assert!(
        user.contains("int source_value = 7;"),
        "retained statement missing:\n{user}"
    );
    assert!(
        !user.contains("CS_PRINT") && !user.contains("CS_MARK"),
        "runtime scaffolding leaked into default source view:\n{user}"
    );

    let raw = run(&["int source_value = 7;", "%src --raw"]);
    assert!(
        raw.contains("CS_PRINT") && raw.contains("CS_MARK"),
        "raw source omitted runtime scaffolding:\n{raw}"
    );
}

#[test]
fn edit_without_previous_or_matching_c_input_is_safe() {
    let out = run(&["%edit", "1 + 1"]);
    assert!(
        out.contains("nothing to edit"),
        "unexpected edit result:\n{out}"
    );
    assert!(
        out.contains("Out[1]: 2"),
        "edit consumed an input number:\n{out}"
    );

    let missing = run(&["1 + 1", "%edit 7"]);
    assert!(
        missing.contains("no C input In[7]"),
        "missing numbered input was not diagnosed:\n{missing}"
    );

    let batch = run(&["1 + 1", "%edit 1", "2 + 2"]);
    assert!(
        batch.contains("%edit is available only in interactive REPL mode"),
        "batch edit did not explain that no prompt can be pre-filled:\n{batch}"
    );
    assert!(
        batch.contains("Out[2]: 4"),
        "batch edit consumed an input number or stopped the session:\n{batch}"
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
fn external_side_effect_replay_warning_is_shown_once() {
    let out = run(&[
        "void write_risk(FILE *fp) { fprintf(fp, \"line\\n\"); fclose(fp); }",
        "void process_risk(void) { system(\"echo not-called\"); }",
        "1 + 1",
    ]);
    assert!(
        out.contains("external side-effect call detected (fprintf(), fclose())"),
        "missing actionable replay warning:\n{out}"
    );
    assert_eq!(
        out.matches("file or process effects may happen repeatedly")
            .count(),
        1,
        "replay warning was not once-per-session:\n{out}"
    );
    assert!(
        out.contains("Out[3]: 2"),
        "session did not continue:\n{out}"
    );
}

#[test]
fn program_output_is_shown_once() {
    // Replay must not re-show output from earlier inputs.
    let out = run(&["puts(\"marker-once\");", "1 + 1"]);
    let hits = out.matches("marker-once").count();
    assert_eq!(hits, 1, "output replayed {hits} times:\n{out}");

    // The visible partial-line marker is terminal UI, never program output in
    // a deterministic script/pipe transcript.
    let partial = run(&["printf(\"partial\");", "1 + 1"]);
    assert!(
        partial.contains("partial\n"),
        "partial output lost:\n{partial}"
    );
    assert!(
        !partial.contains('↵'),
        "TTY marker leaked into pipe:\n{partial}"
    );
}

#[test]
fn time_and_timeit_magics_work() {
    let out = run(&[
        "int acc = 0;",
        "%time acc += 10;",
        "acc",
        "%timeit acc + 5",
        "acc",
    ]);
    assert!(
        out.contains("Wall time:"),
        "missing %time output:
{out}"
    );
    assert!(
        out.contains("Out[3]: 10"),
        "side effect of %time not committed:
{out}"
    );
    assert!(
        out.contains("per loop"),
        "missing %timeit report:
{out}"
    );
    assert!(
        out.contains("Out[4]: 10"),
        "%timeit modified session state:
{out}"
    );
}

#[test]
fn help_lists_commands_and_keeps_usage_notes_behind_verbose() {
    let plain = run(&["%help"]);
    assert!(
        plain.contains("Commands:"),
        "missing Commands list:
{plain}"
    );
    assert!(
        plain.contains("%where <identifier>"),
        "missing %where command:
{plain}"
    );
    assert!(
        plain.contains("%utf8/%utf16/%utf32"),
        "missing Unicode commands:
{plain}"
    );
    assert!(
        !plain.contains("Notes:"),
        "%help unexpectedly printed Notes:
{plain}"
    );

    let verbose = run(&["%help --verbose"]);
    assert!(
        verbose.contains("Commands:"),
        "missing Commands list:
{verbose}"
    );
    assert!(
        verbose.contains("Notes:"),
        "missing Notes in verbose help:
{verbose}"
    );
}

#[test]
fn header_lists_the_default_includes() {
    let out = run(&["%header"]);
    assert!(
        out.contains("#include <stdio.h>")
            && out.contains("#include <string.h>")
            && out.contains("#if defined(__has_include)")
            && out.contains("#  if __has_include(<uchar.h>)")
            && out.contains("#include <uchar.h>")
            && out.contains("#include <wchar.h>"),
        "default headers were not listed:\n{out}"
    );
}
