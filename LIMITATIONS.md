# Known Limitations of c-shell

`c-shell` evaluates your C code by passing complete generated C files to your system host compiler. While this design provides accurate answers matching your real toolchain's ABI and rules, it comes with several inherent design limitations and boundary conditions documented below.

## Security & Execution Model

- **Execution is not sandboxed.** Compilers and generated programs run with `c-shell`'s user permissions and working directory. Do not evaluate untrusted code.
- **Most external side effects still replay.** Direct standard `scanf` calls use the stdin tape (see [HOW_IT_WORKS.md](HOW_IT_WORKS.md)), but file and process changes still repeat on every replay. Calls to known APIs such as `fopen`, `fprintf`, `remove`, `rename` and `system` produce a one-time English warning before execution. Detection is lexical and finite: wrappers and application-defined effects can still escape it.

## Stdin & IO Replay

- **The stdin tape currently wraps `scanf`, not every input API.** `fscanf`, `fgets`, `getchar`, raw `read(0, ...)`, an explicit `#undef scanf`, and code inside a precompiled library are not captured yet. One line is supplied per dynamic `scanf` request; programs that require multiple interactive lines in a single call should put the values on one line. In piped-REPL mode stdin is carrying C source, so a current `scanf` receives EOF; use `-e`, `--script`, or an interactive terminal when the evaluated program needs stdin.
- **Program output is bounded.** When stdin is a real terminal, output from the newest input is streamed immediately, so a prompt before `scanf` is visible. If the program leaves a partial line, `c-shell` appends a dim `↵` marker and the protective newline it inserted; a real `\n` needs no marker. With non-terminal stdin output remains buffered for deterministic transcripts and receives no marker. Capture is limited to 8 MiB per stream; an overflow is reported and the input is not committed.

## Semantics & Scope

- **Rebinding is C shadowing, not assignment.** A redeclared local lives in a nested block. Its declarator is already in scope during its initializer, so `int x = x + 1;` does not read the outer `x`. File-scope replacement also makes retained earlier calls use the new function when the session replays.
- **The purity heuristic is conservative.** Any expression containing a function call is kept, even if the function happens to be pure.
- **Indeterminate values remain undefined C.** Address-only struct-member formatting prevents `c-shell` from adding a `%s` memory dereference, but it cannot make `struct P p; p` well-defined when members were never initialized; reading an indeterminate pointer or scalar is itself not portable. Initialize structs (for example with `{0}`) when the value matters.
- **Incomplete inputs are not committed.** Besides crashes and timeouts, a successful early `exit(0)`, `_Exit` or top-level `return` is detected by a missing completion marker; committing it would terminate every later replay before the new input runs.

## Value Printing Limits

- **Not every C value is printable.** The `_Generic` runtime covers the standard boolean/integer/real-floating types, top-level `char` strings, common object pointers and session-visible structs as described in [HOW_IT_WORKS.md](HOW_IT_WORKS.md). Header-only/anonymous aggregate types that have no reusable C spelling, complex and `void` values are evaluated without `Out[n]` and receive an explanatory note. A member whose type is not in the scalar or generated aggregate table is shown as `<unprintable>` rather than being coerced.

## Platform & Process Management

- **Process cleanup follows the launched process tree.** On Unix, the process group is killed when each run ends; a child that explicitly creates a new session with `setsid` can still escape. On Windows, each run is contained in a Job Object that is closed when the run ends.
- **No session save/load.** The input history is bounded (up to 1000 lines) within the current process memory/file session, but session state cannot be saved to a file or loaded across launches.
- **`cl.exe` setup requirement.** MSVC `cl.exe` needs a configured build environment with `INCLUDE`/`LIB` environment variables set up (a Developer Command Prompt is standard).
- **Windows testing status.** CI covers GNU-style and MSVC compiler drivers, but interactive behavior has not been extensively tested on real Windows installations. Feedback and bug reports are welcome.
