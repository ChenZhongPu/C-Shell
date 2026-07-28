//! Rewriting compiler diagnostics to refer to what the user actually typed.
//!
//! The compiler sees a generated file with a prelude and every earlier
//! statement above the new input, so its line numbers are meaningless at the
//! prompt. Without this remapping the tool is unusable for a beginner: the
//! error text is right but every location it cites is wrong.

/// Rewrite `raw` so locations inside the new input become input-relative.
///
/// `src` is the path handed to the compiler, `generated` is its contents,
/// `start` is the 1-based line where the newest input begins, and `count` is
/// how many lines that text occupies. `session_ranges` distinguishes retained
/// user text from runtime/wrapper lines so source excerpts can hide generated
/// scaffolding without discarding useful cross-reference diagnostics.
pub fn remap(
    raw: &str,
    src: &str,
    generated: &str,
    start: usize,
    count: usize,
    session_ranges: &[(usize, usize)],
    wrapped: bool,
) -> String {
    // Map a generated-file line to an input-relative one. With `wrapped`,
    // the wrapper lines directly above and below the input hold nothing but
    // `CS_PRINT((` and `));`, so a diagnostic anchored there — MSVC's
    // traditional preprocessor does exactly that for macro arguments — can
    // only be about the expression itself and is clamped to it.
    let map_line = |n: usize| -> Option<usize> {
        if n >= start && n < start + count {
            Some(n - start + 1)
        } else if wrapped && n + 1 == start {
            Some(1)
        } else if wrapped && n == start + count {
            Some(count)
        } else {
            None
        }
    };
    // cl.exe prints the bare source filename to stdout on every compile; it
    // carries no information and would leak through every later filter.
    // Only compiler diagnostics pass through here, so a line that is
    // exactly the filename can never be a real message.
    let basename = std::path::Path::new(src)
        .file_name()
        .and_then(|s| s.to_str());
    let mut out = String::with_capacity(raw.len());
    let mut dropping_generated_gutter = false;
    for line in raw.lines() {
        if basename.is_some_and(|b| line.trim() == b) {
            continue;
        }

        if let Some(gutter) = remap_gutter(line, start, count, session_ranges) {
            match gutter {
                Gutter::Keep(line) => {
                    dropping_generated_gutter = false;
                    out.push_str(&line);
                    out.push('\n');
                }
                Gutter::Generated => dropping_generated_gutter = true,
            }
            continue;
        }
        if is_gutter_continuation(line) {
            if !dropping_generated_gutter {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        dropping_generated_gutter = false;

        match split_location(line, src) {
            // An incomplete expression can make the closing `));` wrapper
            // appear as the unexpected token. Report the actual user-facing
            // condition without quoting punctuation c-shell injected.
            Some((gen_line, rest))
                if is_wrapper_fallout(gen_line, rest, generated, start, count) =>
            {
                out.push_str(&format!(
                    "<input>:{count}: error: expected expression at end of input"
                ));
            }
            // A parser error at the end of a statement can name `do` solely
            // because the next generated line expands CS_MARK to do/while.
            // Mark that whole diagnostic block as generated so later filters
            // remove it rather than blaming a token the user never typed.
            Some((gen_line, rest))
                if is_marker_fallout(gen_line, rest, generated, start, count) =>
            {
                out.push_str(&format!("<generated>{rest}"));
            }
            Some((gen_line, rest)) if map_line(gen_line).is_some() => {
                let mapped = map_line(gen_line).expect("checked in guard");
                out.push_str(&format!("<input>:{mapped}{rest}"));
            }
            Some((gen_line, rest)) if in_ranges(gen_line, session_ranges) => {
                out.push_str(&format!("<session>{rest}"));
            }
            // Diagnostics anchored in runtime macros, marker calls or the
            // generated main wrapper are retained only under an internal tag;
            // public warning/error filters drop the complete block.
            Some((_, rest)) => out.push_str(&format!("<generated>{rest}")),
            None => out.push_str(&line.replace(src, "<input>")),
        }
        out.push('\n');
    }
    out
}

enum Gutter {
    Keep(String),
    Generated,
}

/// Rewrite the line number in a GCC/Clang source-excerpt gutter (`  42 | ...`).
/// New-input lines are renumbered, retained session lines keep their text with
/// the meaningless generated number blanked, and scaffolding lines disappear.
fn remap_gutter(
    line: &str,
    start: usize,
    count: usize,
    session_ranges: &[(usize, usize)],
) -> Option<Gutter> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = &trimmed[digits.len()..];
    if !rest.starts_with(" |") {
        return None;
    }
    let n: usize = digits.parse().ok()?;
    if n >= start && n < start + count {
        let shown = format!("{:>w$}", n - start + 1, w = digits.len());
        return Some(Gutter::Keep(format!(
            "{}{}{}",
            " ".repeat(indent),
            shown,
            rest
        )));
    }
    if in_ranges(n, session_ranges) {
        return Some(Gutter::Keep(format!(
            "{}{}{}",
            " ".repeat(indent),
            " ".repeat(digits.len()),
            rest
        )));
    }
    Some(Gutter::Generated)
}

fn is_gutter_continuation(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

fn in_ranges(line: usize, ranges: &[(usize, usize)]) -> bool {
    ranges
        .iter()
        .any(|&(start, count)| line >= start && line < start + count)
}

fn is_wrapper_fallout(
    gen_line: usize,
    rest: &str,
    generated: &str,
    start: usize,
    count: usize,
) -> bool {
    let last_input_gen_line = start + count - 1;
    let opening_wrapper_gen_line = start.saturating_sub(1);
    if gen_line != opening_wrapper_gen_line
        && gen_line != last_input_gen_line
        && gen_line != last_input_gen_line + 1
    {
        return false;
    }
    let Some(last_input_line) = generated.lines().nth(last_input_gen_line - 1) else {
        return false;
    };
    if last_input_line.trim_end().ends_with(')') {
        return false;
    }
    let Some(next_line) = generated.lines().nth(last_input_gen_line) else {
        return false;
    };
    let closes_wrapper = matches!(next_line.trim(), "));" | ")));" | "),");
    closes_wrapper
        && (rest.contains("expected expression")
            || (rest.contains("syntax error")
                && ["')'", "‘)’", "`)'"]
                    .iter()
                    .any(|quoted| rest.contains(quoted))))
}

fn is_marker_fallout(
    gen_line: usize,
    rest: &str,
    generated: &str,
    start: usize,
    count: usize,
) -> bool {
    if gen_line != start + count - 1 {
        return false;
    }
    let Some(last_input_line) = generated.lines().nth(gen_line - 1) else {
        return false;
    };
    // If the user really wrote a `do` token, the diagnostic is theirs. A
    // false negative here merely leaves compiler prose intact; it is safer
    // than hiding a real parser error.
    if last_input_line
        .split(|c: char| !(c == '_' || c.is_ascii_alphanumeric()))
        .any(|token| token == "do")
    {
        return false;
    }
    let Some(next_line) = generated.lines().nth(gen_line) else {
        return false;
    };
    if !next_line.trim_start().starts_with("CS_MARK(CS_M_DONE)") {
        return false;
    }
    let names_generated_do = ["'do'", "‘do’", "`do'", "\"do\""]
        .iter()
        .any(|quoted| rest.contains(quoted));
    if names_generated_do {
        return true;
    }

    // Clang diagnoses the same fallback as "expected ';' after expression"
    // without naming the following marker. It is synthetic only when the
    // caret is just past the final user byte; a semicolon complaint before a
    // real token on that line must remain visible.
    let missing_semicolon = [
        "expected ';' after expression",
        "expected ‘;’ after expression",
        "expected ';' at end",
        "expected ‘;’ at end",
    ]
    .iter()
    .any(|text| rest.contains(text));
    missing_semicolon
        && diagnostic_column(rest).is_some_and(|column| {
            column >= last_input_line.trim_end().chars().count().saturating_add(1)
        })
}

fn diagnostic_column(rest: &str) -> Option<usize> {
    let after_colon = rest.strip_prefix(':')?;
    let end = after_colon.find(|c: char| !c.is_ascii_digit())?;
    after_colon[..end].parse().ok()
}

/// Keep only the diagnostic blocks anchored in the newest input.
///
/// Applied to warnings, never to errors. Every evaluation recompiles the whole
/// session, so without this a warning about input 2 reappears under every
/// input thereafter. Errors keep their `<session>` blocks because those carry
/// the other half of cross-referencing notes such as "previous definition".
pub fn only_new(text: &str) -> String {
    filter_blocks(text, |header| header.starts_with("<input>:"))
}

/// Drop stale warnings from an error report, keeping everything else.
///
/// Errors need their `<session>` blocks: a "previous definition was here"
/// note points at an earlier input and is the more useful half of the
/// message. A warning about an earlier input, on the other hand, has nothing
/// to do with the error being reported and only buries it.
pub fn drop_stale_warnings(text: &str) -> String {
    let filtered = filter_blocks(text, |header| {
        // Both spellings: GNU/Clang `warning:` and MSVC `warning C4552:`.
        let is_warning = header.contains("warning:") || header.contains("warning C");
        header.starts_with("<input>:") || !is_warning
    });
    if !filtered.trim().is_empty() {
        return filtered;
    }

    // Normally generated-runtime diagnostics are implementation details and
    // disappear as expression/statement fallbacks are tried. A fatal error
    // with no surviving user diagnostic is different: hiding it leaves an
    // entirely blank result and makes every input appear to do nothing.
    // Preserve only the compiler's reason, not generated line numbers or
    // source excerpts.
    text.lines()
        .find_map(|line| {
            line.starts_with("<generated>")
                .then(|| line.split_once("fatal error").map(|(_, rest)| rest))
                .flatten()
        })
        .map(|reason| {
            format!(
                "error: c-shell's generated C runtime failed to compile: fatal error{reason}\n\
                 run %src --raw to inspect the generated program\n"
            )
        })
        .unwrap_or(filtered)
}

/// Walk diagnostics block by block, keeping those whose anchored header line
/// satisfies `keep`.
fn filter_blocks(text: &str, keep_header: impl Fn(&str) -> bool) -> String {
    let mut out = String::new();
    // A diagnostic's continuation lines (source excerpt, caret) are indented
    // and unanchored, so they inherit the decision made for their header.
    let mut keep = true;
    // GCC prefixes a group of diagnostics with a bare `file: In function 'f':`
    // banner carrying no line number. Emitting it eagerly would leave it
    // stranded above nothing whenever the group underneath is dropped, so it
    // waits until something it introduces actually survives.
    let mut pending: Option<String> = None;

    for line in text.lines() {
        if is_group_banner(line) {
            pending = Some(line.to_string());
            continue;
        }
        if line.starts_with("<input>:")
            || line.starts_with("<session>")
            || line.starts_with("<generated>")
        {
            keep = !line.starts_with("<generated>") && keep_header(line);
        }
        if keep {
            if let Some(p) = pending.take() {
                out.push_str(&p);
                out.push('\n');
            }
            if out.lines().next_back() != Some(line) {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// A `<input>: In function 'main':` style banner: anchored to the file but to
/// no particular line.
fn is_group_banner(line: &str) -> bool {
    for tag in ["<input>: ", "<session>: ", "<generated>: "] {
        if let Some(rest) = line.strip_prefix(tag) {
            return !rest.starts_with(|c: char| c.is_ascii_digit());
        }
    }
    false
}

/// Recognise a leading `path:LINE` (GNU, Clang) or `path(LINE)` (MSVC)
/// location, returning the line number and everything after it.
fn split_location<'a>(line: &'a str, src: &str) -> Option<(usize, &'a str)> {
    let rest = line.strip_prefix(src)?;

    if let Some(after) = rest.strip_prefix(':') {
        let end = after.find(|c: char| !c.is_ascii_digit())?;
        let n: usize = after[..end].parse().ok()?;
        return Some((n, &after[end..]));
    }
    // MSVC writes `path(LINE)` or `path(LINE,COL)`; the closing paren belongs
    // to the location and must not survive into the rewritten message.
    if let Some(after) = rest.strip_prefix('(') {
        let end = after.find(|c: char| !c.is_ascii_digit())?;
        let n: usize = after[..end].parse().ok()?;
        let tail = &after[end..];
        let tail = match tail.strip_prefix(',') {
            Some(col) => {
                let e = col.find(|c: char| !c.is_ascii_digit())?;
                &col[e..]
            }
            None => tail,
        };
        return Some((n, tail.strip_prefix(')')?));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_gnu_location_into_input_space() {
        let raw = "/tmp/x/in.c:42:5: error: 'y' undeclared";
        assert_eq!(
            remap(raw, "/tmp/x/in.c", "", 40, 5, &[], false).trim_end(),
            "<input>:3:5: error: 'y' undeclared"
        );
    }

    #[test]
    fn maps_msvc_location() {
        let raw = "C:\\t\\in.c(42): error C2065: 'y': undeclared identifier";
        assert_eq!(
            remap(raw, "C:\\t\\in.c", "", 42, 1, &[], false).trim_end(),
            "<input>:1: error C2065: 'y': undeclared identifier"
        );
    }

    #[test]
    fn distinguishes_retained_session_locations_from_generated_ones() {
        let raw = "/tmp/x/in.c:10:1: note: previous definition";
        assert_eq!(
            remap(raw, "/tmp/x/in.c", "", 40, 1, &[(10, 1)], false).trim_end(),
            "<session>:1: note: previous definition"
        );
        // Just past the end of a one-line input: the generated `return 0;`.
        let raw = "/tmp/x/in.c:41:5: error: expected ';' before 'return'";
        assert!(remap(raw, "/tmp/x/in.c", "", 40, 1, &[], false).starts_with("<generated>"));
    }

    #[test]
    fn renumbers_the_excerpt_gutter_to_match_the_header() {
        let raw = "   42 | int x = ;\n   43 |     return 0;";
        let got = remap(raw, "/tmp/x/in.c", "", 42, 1, &[], false);
        assert!(got.contains("    1 | int x = ;"), "{got}");
        assert!(!got.contains("return 0"), "scaffolding leaked: {got}");
    }

    #[test]
    fn wrapper_line_anchors_are_pulled_into_a_wrapped_input() {
        // MSVC's traditional preprocessor pins diagnostics from a macro
        // argument to the invocation's first line: `CS_PRINT((`, one line
        // above the input. Nothing but the wrapper lives there, so the
        // anchor is clamped to the expression.
        let raw = "C:\\t\\in.c(41): warning C4018: '>': signed/unsigned mismatch";
        assert_eq!(
            remap(raw, "C:\\t\\in.c", "", 42, 1, &[], true).trim_end(),
            "<input>:1: warning C4018: '>': signed/unsigned mismatch"
        );
        // The closing `));` line clamps to the input's last line.
        let raw2 = "C:\\t\\in.c(43): error C2143: syntax error";
        assert_eq!(
            remap(raw2, "C:\\t\\in.c", "", 42, 1, &[], true).trim_end(),
            "<input>:1: error C2143: syntax error"
        );
        // Unwrapped slots keep strict attribution.
        assert!(remap(raw, "C:\\t\\in.c", "", 42, 1, &[], false).starts_with("<generated>"));
    }

    #[test]
    fn removes_wrapper_source_excerpts_but_keeps_user_code() {
        let raw = "/tmp/x/in.c:42:1: warning: pointer used after free\n\
                    41 |     CS_PRINT((\n\
                       |              ~\n\
                    42 | *p\n\
                       | ^~\n\
                    43 |     ));\n\
                       |     ~";
        let got = remap(raw, "/tmp/x/in.c", "", 42, 1, &[], true);
        assert!(got.contains("1 | *p"), "user excerpt missing: {got}");
        assert!(!got.contains("CS_PRINT"), "opening wrapper leaked: {got}");
        assert!(!got.contains("));"), "closing wrapper leaked: {got}");
    }

    #[test]
    fn incomplete_expression_does_not_blame_the_closing_wrapper() {
        let generated = "int main(void)\n1 +\n    ));\n";
        let raw = "/tmp/x/in.c:2:4: error: expected expression before ')' token\n\
                   /tmp/x/in.c:2:4: error: expected expression before ')' token";
        let mapped = remap(raw, "/tmp/x/in.c", generated, 2, 1, &[], true);
        let got = drop_stale_warnings(&mapped);
        assert_eq!(
            got.trim_end(),
            "<input>:1: error: expected expression at end of input"
        );

        let msvc = "C:\\t\\in.c(1): error C2059: syntax error: ')'";
        let mapped = remap(msvc, "C:\\t\\in.c", generated, 2, 1, &[], true);
        assert_eq!(
            drop_stale_warnings(&mapped).trim_end(),
            "<input>:1: error: expected expression at end of input"
        );
    }

    #[test]
    fn drops_parser_fallout_that_names_the_generated_marker_macro() {
        let generated = "int main(void)\nnumbr + 1\n    CS_MARK(CS_M_DONE);\n";
        let raw = "/tmp/x/in.c:2:1: error: 'numbr' undeclared\n\
                   /tmp/x/in.c:2:10: error: expected ';' before 'do'";
        let mapped = remap(raw, "/tmp/x/in.c", generated, 2, 1, &[], false);
        let got = drop_stale_warnings(&mapped);
        assert!(got.contains("numbr"), "real diagnostic missing: {got}");
        assert!(!got.contains("do"), "generated macro token leaked: {got}");

        let clang = "/tmp/x/in.c:2:10: error: expected ';' after expression";
        let mapped = remap(clang, "/tmp/x/in.c", generated, 2, 1, &[], false);
        assert!(
            drop_stale_warnings(&mapped).is_empty(),
            "synthetic Clang semicolon diagnostic survived: {mapped}"
        );
    }

    #[test]
    fn fatal_generated_runtime_errors_never_become_blank() {
        let raw = "<generated>:15:10: fatal error: 'uchar.h' file not found\n\
                   15 | #include <uchar.h>\n\
                      |          ^~~~~~~~~\n";
        assert_eq!(
            drop_stale_warnings(raw),
            "error: c-shell's generated C runtime failed to compile: fatal error: 'uchar.h' file not found\n\
             run %src --raw to inspect the generated program\n"
        );
    }

    #[test]
    fn drops_msvc_bare_filename_lines() {
        // cl.exe echoes the source filename to stdout on every compile.
        let raw = "input.c\n/tmp/x/input.c:42:1: warning: something";
        let got = remap(raw, "/tmp/x/input.c", "", 42, 1, &[], false);
        assert!(!got.contains("input.c"), "{got}");
        assert!(got.contains("<input>:1:1: warning"), "{got}");
    }

    #[test]
    fn scrubs_temp_path_from_unlocated_lines() {
        let raw = "cc1: warning while reading /tmp/x/in.c";
        assert_eq!(
            remap(raw, "/tmp/x/in.c", "", 1, 1, &[], false).trim_end(),
            "cc1: warning while reading <input>"
        );
    }
}
