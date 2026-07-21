//! Rewriting compiler diagnostics to refer to what the user actually typed.
//!
//! The compiler sees a generated file with a prelude and every earlier
//! statement above the new input, so its line numbers are meaningless at the
//! prompt. Without this remapping the tool is unusable for a beginner: the
//! error text is right but every location it cites is wrong.

/// Rewrite `raw` so locations inside the new input become input-relative.
///
/// `src` is the path handed to the compiler, `start` is the 1-based line in
/// the generated file where the user's text begins, and `count` is how many
/// lines that text occupies. Anything outside `start..start+count` belongs to
/// generated scaffolding or an earlier input and must not be presented as if
/// the user had typed it.
pub fn remap(raw: &str, src: &str, start: usize, count: usize, wrapped: bool) -> String {
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
    for line in raw.lines() {
        if basename.is_some_and(|b| line.trim() == b) {
            continue;
        }
        match split_location(line, src) {
            Some((gen_line, rest)) if map_line(gen_line).is_some() => {
                let mapped = map_line(gen_line).expect("checked in guard");
                out.push_str(&format!("<input>:{mapped}{rest}"));
            }
            // A complaint anchored in the prelude, in the generated `main`
            // wrapper, or in code from an earlier input. Kept, because it is
            // often the other half of a cross-referencing diagnostic such as
            // "previous definition was here", but never labelled `<input>`.
            Some((_, rest)) => out.push_str(&format!("<session>{rest}")),
            None => {
                let scrubbed = line.replace(src, "<input>");
                out.push_str(&remap_gutter(&scrubbed, start, count).unwrap_or(scrubbed));
            }
        }
        out.push('\n');
    }
    out
}

/// Rewrite the line number in a GCC/Clang source-excerpt gutter (`  42 | ...`).
///
/// Without this the header says `<input>:1` while the excerpt underneath it
/// still says `42`, which is exactly the kind of contradiction that makes a
/// beginner distrust the tool. Numbers outside the input are blanked rather
/// than renumbered: the excerpt text is real, its position is not the user's.
fn remap_gutter(line: &str, start: usize, count: usize) -> Option<String> {
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
    let shown = if n >= start && n < start + count {
        format!("{:>w$}", n - start + 1, w = digits.len())
    } else {
        " ".repeat(digits.len())
    };
    Some(format!("{}{}{}", " ".repeat(indent), shown, rest))
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
    filter_blocks(text, |header| {
        // Both spellings: GNU/Clang `warning:` and MSVC `warning C4552:`.
        let is_warning = header.contains("warning:") || header.contains("warning C");
        header.starts_with("<input>:") || !is_warning
    })
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
        if line.starts_with("<input>:") || line.starts_with("<session>") {
            keep = keep_header(line);
        }
        if keep {
            if let Some(p) = pending.take() {
                out.push_str(&p);
                out.push('\n');
            }
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// A `<input>: In function 'main':` style banner: anchored to the file but to
/// no particular line.
fn is_group_banner(line: &str) -> bool {
    for tag in ["<input>: ", "<session>: "] {
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
            remap(raw, "/tmp/x/in.c", 40, 5, false).trim_end(),
            "<input>:3:5: error: 'y' undeclared"
        );
    }

    #[test]
    fn maps_msvc_location() {
        let raw = "C:\\t\\in.c(42): error C2065: 'y': undeclared identifier";
        assert_eq!(
            remap(raw, "C:\\t\\in.c", 42, 1, false).trim_end(),
            "<input>:1: error C2065: 'y': undeclared identifier"
        );
    }

    #[test]
    fn labels_locations_outside_the_input() {
        let raw = "/tmp/x/in.c:10:1: note: previous definition";
        assert_eq!(
            remap(raw, "/tmp/x/in.c", 40, 1, false).trim_end(),
            "<session>:1: note: previous definition"
        );
        // Just past the end of a one-line input: the generated `return 0;`.
        let raw = "/tmp/x/in.c:41:5: error: expected ';' before 'return'";
        assert!(remap(raw, "/tmp/x/in.c", 40, 1, false).starts_with("<session>"));
    }

    #[test]
    fn renumbers_the_excerpt_gutter_to_match_the_header() {
        let raw = "   42 | int x = ;\n   43 |     return 0;";
        let got = remap(raw, "/tmp/x/in.c", 42, 1, false);
        assert!(got.contains("    1 | int x = ;"), "{got}");
        // Scaffolding keeps its text but loses its misleading number.
        assert!(got.contains("      |     return 0;"), "{got}");
    }

    #[test]
    fn wrapper_line_anchors_are_pulled_into_a_wrapped_input() {
        // MSVC's traditional preprocessor pins diagnostics from a macro
        // argument to the invocation's first line: `CS_PRINT((`, one line
        // above the input. Nothing but the wrapper lives there, so the
        // anchor is clamped to the expression.
        let raw = "C:\\t\\in.c(41): warning C4018: '>': signed/unsigned mismatch";
        assert_eq!(
            remap(raw, "C:\\t\\in.c", 42, 1, true).trim_end(),
            "<input>:1: warning C4018: '>': signed/unsigned mismatch"
        );
        // The closing `));` line clamps to the input's last line.
        let raw2 = "C:\\t\\in.c(43): error C2143: syntax error";
        assert_eq!(
            remap(raw2, "C:\\t\\in.c", 42, 1, true).trim_end(),
            "<input>:1: error C2143: syntax error"
        );
        // Unwrapped slots keep strict attribution.
        assert!(remap(raw, "C:\\t\\in.c", 42, 1, false).starts_with("<session>"));
    }

    #[test]
    fn drops_msvc_bare_filename_lines() {
        // cl.exe echoes the source filename to stdout on every compile.
        let raw = "input.c\n/tmp/x/input.c:42:1: warning: something";
        let got = remap(raw, "/tmp/x/input.c", 42, 1, false);
        assert!(!got.contains("input.c"), "{got}");
        assert!(got.contains("<input>:1:1: warning"), "{got}");
    }

    #[test]
    fn scrubs_temp_path_from_unlocated_lines() {
        let raw = "cc1: warning while reading /tmp/x/in.c";
        assert_eq!(
            remap(raw, "/tmp/x/in.c", 1, 1, false).trim_end(),
            "cc1: warning while reading <input>"
        );
    }
}
