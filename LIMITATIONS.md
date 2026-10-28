# Known Limitations of c-shell

`c-shell` evaluates your C code by passing complete generated C files to your system host compiler. While this design provides accurate answers matching your real toolchain's ABI and rules, it comes with several inherent design limitations and boundary conditions documented below.

## Security & Execution Model

- **Execution is not sandboxed.** Compilers and generated programs run with `c-shell`'s user permissions and working directory. Do not evaluate untrusted code.
- **Most external side effects still replay.** Supported stdin calls use the stdin tape (see [HOW_IT_WORKS.md](HOW_IT_WORKS.md)), but file and process changes still repeat on every replay. Calls to known APIs such as `fopen`, `fprintf`, `remove`, `rename` and `system` produce a one-time English warning before execution. Detection is lexical and finite: wrappers and application-defined effects can still escape it. Stream-taking APIs such as `fgets` can conservatively trigger this warning even when their stream is stdin.

## Stdin & IO Replay

- **The stdin tape wraps common direct reads, not every input API.** `scanf`, `getchar`, `getc(stdin)`, `fgetc(stdin)` and `fgets(..., stdin)` are captured, including mixed buffered reads, `ungetc` pushback and EOF. `fscanf`, `fread`, raw `read(0, ...)`, explicitly removing the wrapper macros, and calls inside a precompiled library are not captured. An active request can receive one line; calls already completed from libc's buffer do not require another line. Programs that require multiple interactive lines in a single call should put the values on one line. With piped or redirected source stdin is carrying C code, so current program reads receive EOF; use `-e` or an interactive terminal when the evaluated program needs stdin.
- **Program output is bounded.** When stdin is a real terminal, output from the newest input is streamed immediately, so a prompt before `scanf` is visible. If the program leaves a partial line, `c-shell` appends a dim `↵` marker and the protective newline it inserted; a real `\n` needs no marker. With non-terminal stdin output remains buffered for deterministic transcripts and receives no marker. Capture is limited to 8 MiB per stream; an overflow is reported and the input is not committed.

## Semantics & Scope

- **Rebinding is C shadowing, not assignment.** A redeclared local lives in a nested block. Its declarator is already in scope during its initializer, so `int x = x + 1;` does not read the outer `x`. File-scope replacement also makes retained earlier calls use the new function when the session replays.
- **The purity heuristic is conservative.** Any expression containing a function call or a reference to a macro defined in retained input is kept, even if it happens to be pure.
- **Indeterminate values remain undefined C.** Address-only struct-member formatting prevents `c-shell` from adding a `%s` memory dereference, but it cannot make `struct P p; p` well-defined when members were never initialized; reading an indeterminate pointer or scalar is itself not portable. Initialize structs (for example with `{0}`) when the value matters.
- **Incomplete inputs are not committed.** Besides crashes and timeouts, a successful early `exit(0)`, `_Exit` or top-level `return` is detected by a missing completion marker; committing it would terminate every later replay before the new input runs.

## Value Printing Limits

- **Not every C value is printable.** The `_Generic` runtime covers the standard boolean/integer/real-floating types, top-level `char` strings, common object pointers and session-visible structs as described in [HOW_IT_WORKS.md](HOW_IT_WORKS.md). Header-only/anonymous aggregate types that have no reusable C spelling, complex and `void` values are evaluated without `Out[n]` and receive an explanatory note. A member whose type is not in the scalar or generated aggregate table is shown as `<unprintable>` rather than being coerced.

## Resident mode (`--resident`)

- **Experimental.** Windows requires a MinGW-w64 compiler such as WinLibs GCC or Clang; `cl.exe` and `clang-cl` are not supported.
- **Changing compilers clears the resident session.** `%cc` resets the state when the selected compiler changes; `%std` keeps it.
- **No variable-length arrays.** Their size cannot be recovered from a declaration, so steps compile with `-Werror=vla`.
- **A type that earlier definitions still use cannot be redefined** until `%reset`; functions and macros can.
- **Declarations are one construct per input.** Mixing declarations and statements in one input is rejected, and variables of an anonymous struct/union/enum type are not supported.
- **A crash, timeout or interrupt restarts the runner** and silently replays the journal of successful steps, so side effects of those steps can repeat in that case.
- **Magic commands read the live state.** `%type`, `%bits`, `%utf8/16/32`, `%time` and `%timeit` run inside the resident runner and nothing is replayed. `%timeit` runs against the live session, like IPython, so `%timeit n++` really changes `n`.
- **Source views are reconstructions.** In resident mode, `%src` uses nested scopes for variable rebindings, but function or type redefinitions may still appear multiple times and yield invalid C. Live changes from probes or `%timeit` are not captured. `%src --raw` shows replay-backend scaffolding rather than resident step source.
- **Only output is allowed.** Resident mode refuses (before compiling) calls that read input or change files, processes or the environment, such as `scanf`, `getchar`, `fopen`, `system`, `remove` or `setenv`; `printf`, `puts`, `fprintf`, `fputs`, `fflush` and `write` are fine. The check is lexical, so a call made through a function pointer is not seen. This keeps crash recovery safe: replaying the journal can only repeat output. Use replay mode (no `--resident`) for I/O.
- **Not yet covered:** live output streaming.

## Platform & Process Management

- **Process cleanup follows the launched process tree.** On Unix, the process group is killed when each run ends; a child that explicitly creates a new session with `setsid` can still escape. On Windows, each run is contained in a Job Object that is closed when the run ends.
- **No session save/load.** The input history is bounded (up to 1000 lines) within the current process memory/file session, but session state cannot be saved to a file or loaded across launches.
- **Windows uses WinLibs; MSVC is not supported.** c-shell drives MinGW-w64 GCC or Clang on Windows, and release binaries are built for `x86_64-pc-windows-gnu`. Put the toolchain's `bin` directory on `PATH`. `cl.exe` and `clang-cl` are not supported.
- **Windows verification uses native WinLibs CI.** The Windows job runs the end-to-end tests with GCC, including resident mode, and checks switching to Clang. Other Windows compiler configurations are not covered by the full suite.
