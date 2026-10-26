# How c-shell Works

This document provides a detailed overview of the inner workings of `c-shell`, including how expressions and statements are evaluated, how stdin replay operates, how types and value printers work, and how compiler diagnostics are sanitized and remapped.

## Architecture & Implementation Principles

### 1. Accumulate and Replay
Every evaluation reassembles and reruns the whole session. Block-scope declarations become ordinary locals in `main`; a compiler-approved redeclaration starts a nested shadowing scope that encloses later statements. Items recognized as file-scope code are emitted above `main`, and approved redefinitions replace an older item in place. There is no separate symbol table or declaration/initializer state store.

### 2. Session-Local Stdin Tape
Direct `scanf`, `getchar`, `getc(stdin)`, `fgetc(stdin)` and `fgets(..., stdin)` calls are wrapped while libc retains responsibility for conversions, buffering, pushback and EOF. Non-stdin file streams go directly to libc. Call-start and call-completion markers distinguish an active read from a call already satisfied by libc's buffer. A completed buffered call retains an empty call boundary without consuming another parent-input line; an active call can receive one fresh line, retained only in memory. Historical calls replay those boundaries and lines without reconnecting to the terminal. Parent-input reads are polled on Unix and Windows so a completion marker or Ctrl-C can release a pending read even when stdin stays open. Calls inside functions, recursion and loops therefore replay in their original order. `%src` annotates the associated statement with only a request count—the bytes remain hidden. `%reset` discards the tape, and no tape is written to a history file. If function replacement or changed control flow consumes a different number of requests, evaluation stops with an explicit stdin-tape divergence instead of silently prompting or waiting for the normal timeout.

Parent read-ahead belongs to the process stdin stream and survives individual
evaluations, temporary checks and `%reset`. This keeps later lines from a
single Windows pipe read available to subsequent inputs. Reset still discards
the session's replay tape; it does not discard unread bytes from the stream.

### 3. Trial Compilation & Classification
A lexical heuristic routes function definitions, preprocessor directives, typedefs and tag definitions to file scope; these inputs are never demoted into `main`, which prevents GCC from silently accepting a nested function that Clang rejects. The compiler arbitrates expression-versus-statement cases, but an input whose final code token is `;` goes directly to the statement slot rather than launching an expression compile that cannot succeed. Clearly braced statements ending in `}` do the same; possible compound literals such as `(int){7}` retain expression classification. After a redeclaration diagnostic, alternate nested-scope or in-place replacement assemblies must compile as complete programs before c-shell accepts them. File scope remains unavailable as a general fallback.

### 4. Value Printing Mechanics
`_Generic` selects a print *function*, which is then called — selecting a call expression instead would type-check every unselected branch against the wrong argument type and fail to compile. Session-visible named structs and simple anonymous struct typedefs get recursive printers whose output uses designated-initializer form:

```text
Out[5]: {
    .name = (void *)0x7f068cbbac1b,
    .age = 30,
    .scores = {0, 7, 0},
    .next = NULL
}
```

An array is a harder case than it looks, because C converts one to a pointer before `_Generic` can see its type — the printer is handed an address and has no way to ask how many elements it addresses. So when a value does print as a bare address, c-shell re-prints it through an array-aware wrapper that settles the question with the real object at run time: an array begins at its own first element, a pointer variable does not.

```text
In [1]: int values[4] = {3, 1, 4, 1};
In [2]: values
Out[2]: {3, 1, 4, 1}
In [3]: int grid[2][2] = {{1, 2}, {3, 4}};
In [4]: grid
Out[4]: {{1, 2}, {3, 4}}
In [5]: int *ptr = values;
In [6]: ptr
Out[6]: 0x7ffd2f8c4a10
```

Element counts come from `sizeof`, so no declarator is parsed and a genuine pointer keeps printing as an address. `char text[] = "hi"` still prints `"hi"`, but character-array previews stop at the array's `sizeof` bound even when no NUL terminator is present. GNU/Clang obtain that bound through unevaluated type inspection. Actual `char *` values retain the ordinary NUL-terminated string behavior. Beyond 100 elements the rest are summarized as `... (n more)`. Nesting is the one static property this cannot recover at run time, so arrays deeper than two dimensions, and arrays whose elements have no printer of their own (an array of pointers, for instance), show `<unprintable>` elements rather than a guess.

Struct-member formatting never treats `char *` as a string: every pointer member is `NULL` or an address, fixed arrays are traversed structurally with the same 100-element bound, and a known nested struct calls its own printer. A top-level `struct P *` likewise prints only its address; explicit `*ptr` requests expansion. This differs intentionally from a top-level `char *` expression, where the user explicitly asked for the existing string dereference. Small flat structs stay on one line; larger, nested or array-bearing structs use indented multiline output. Multi-declarator fields, function-pointer declarators, bit-fields, flexible arrays, C11 anonymous members and unions conservatively produce a labelled raw byte dump instead of guessed member names.

String and character previews escape quotes, backslashes and control bytes using C escapes (`\n`, `\t`, `\033`, etc.). Non-ASCII bytes remain unchanged. This formatting applies to value previews; explicit program output from `printf`, `puts` and similar calls keeps its original bytes and platform line endings.

### 5. Purity Heuristic for Bare Expressions
A bare expression at the prompt is usually a question (`x + 1`, `sizeof(int)`), so it is answered and forgotten. Bare expressions that may have effects — assignments, `++`/`--` or calls — are retained. References to macros defined in retained inputs are also kept conservatively, because expansion can hide a mutation or call. Captured stdin events independently force retention of their expression before the tape is attached. Successfully evaluated statements/declarations (including an expression with a trailing `;`) and file-scope items are retained without purity analysis. `%src` shows what will actually be replayed.

### 6. Remapping and Sanitizing Diagnostics
The compiler sees a generated file with a prelude and all earlier statements above your input, so its line numbers are meaningless at the prompt. Locations attributable to the newest input — including GCC source-excerpt gutters — are rewritten to input-relative lines. Code generation records which other lines came from retained user inputs; source excerpts from `CS_PRINT`, `_Generic`, marker calls and wrapper code are removed while genuine earlier-input cross-references remain. Parser fallout that names the marker macro's internal `do` token is removed; an incomplete expression reports `expected expression at end of input` instead of blaming the wrapper's closing `)`.

### 7. Compiler Capability Caching
Successful startup probes are cached for seven days under the platform cache directory (`$XDG_CACHE_HOME/c-shell`, falling back to `~/.cache/c-shell`, on Unix; `%LOCALAPPDATA%\c-shell` on Windows). The cache key changes with compiler file metadata, requested standard, relevant toolchain environment, or c-shell version. Expired or malformed entries simply cause fresh probes.

### 8. Batch Results and Repeated Measurements
Ordinary inputs and `%time` share output rendering and session commits. In `-e` mode, values and program stdout go to stdout; diagnostics and timing information go to stderr. Failed magic commands contribute to the nonzero exit status in `-e` mode. Piped and redirected input keep the interactive REPL's tolerant exit status. Returning an exit code through `main` lets generated-source temporary directories clean up on success and failure.

`%timeit` compiles once and passes the loop count as a runtime argument for both calibration and measured samples. Calibration grows in bounded steps of 2x to 10x until a batch reaches roughly 100 ms (less for short execution deadlines), so a single iteration below the host clock resolution cannot trigger an immediate jump to 100 million iterations. Each sample still starts a fresh process and replays the same retained session. Source generation maintains a running line count so recording diagnostic spans does not repeatedly scan the accumulated program.

### 9. On-demand Runtime Checks

`%check [--address | --undefined] <code>` creates a temporary evaluator with
sanitizer compiler/linker flags, preserving the current compiler, standard,
timeout, terminal streaming and stdin replay. It never commits checked input
or consumes an input number. Automatic selection tries ASan+UBSan, then ASan,
then UBSan; an explicit backend request never falls back to unchecked code.
No sanitizer probes run during startup or ordinary evaluation.

Each candidate must compile and link, execute a valid program, and terminate
with the expected report for deliberately faulty memory/arithmetic operations.
This rejects ignored flags and missing runtime libraries. Results are cached
only in the evaluator and cleared when `%cc` or `%std` changes the toolchain.
Instrumentation flags precede the source path so the compiler cannot treat
them as input files.

Checks stop at the first detected error, and leak detection is disabled for
session allocations. Sanitizer reports use a private log directory per run,
separate from program stderr, so live prompts remain immediate and native
stack traces cannot bypass formatting in the terminal. Some combined GCC
runtimes still write UBSan reports to stderr: a streaming prefix filter
withholds those reports across pipe chunks while preserving program prompts,
and the captured report follows the same formatting path. Probes accept both
reporting paths. Report reads obey the capture limit.
Runtime failures are recognized even during retained replay or exit handlers
after the completion marker. Source locations remain internal; summaries omit
line numbers and paths in both terminal and batch modes.
Output explains common findings in two lines. Unknown findings get a brief
generic message. Messages remain English in every UI language.
The stderr fallback requires a native source-location header and a failed
execution; ordinary program messages such as `runtime error: demo` are kept.
If a successful program prints a native-looking example, any withheld live
stderr is restored. The protocol parser consumes raw stderr before diagnostic
filtering, so a diagnostic-looking example cannot hide stdin requests or
completion markers. The filter retains bounded file-location prefixes across
chunks, including locations in other headers, and saves withheld bytes within
the output capture limit for restoration after successful execution.
Stack symbolization is disabled because the concise
summary does not use stack traces or an external symbolizer.

### 10. Reusing Analysis Without Reusing Execution

An evaluator remembers up to 256 compiler-confirmed expression printing
strategies for one session revision. Repeated pointer and array queries can
then compile the appropriate wrapper directly. Every query still compiles
and executes the complete retained program. Session mutation, shadowing,
file-item replacement, reset, or a toolchain change invalidates the strategies.
Explicit includes bypass this cache because header contents can change
outside the session; UTF-8 previews also stay fresh because their validity
depends on current bytes. A cached wrapper that fails to compile falls back
to ordinary classification. See [PERFORMANCE.md](PERFORMANCE.md) for measurements.

Session identifier and macro indexes are updated only for added, replaced or
removed text, with reference counts for names shared by multiple inputs.
Completion vocabulary refreshes only after the session revision changes.
The purity analysis for retained functions is reused until file-scope code
changes. These are lexical indexes, not a C type or value interpreter.

The editor keeps syntect state after each physical line. Multiline block
comments therefore retain their context. Repainting or moving the cursor
reuses unchanged lines; editing reparses from the first changed line onward.

### 11. Interrupts and Platform Verification

Ctrl-C cancels the current edit, compiler invocation, executing program, or
pending program-input read. Child process groups (Unix) or Job Objects
(Windows) clean up descendants, and interrupted inputs are not committed.
The next input clears the interrupt flag and uses the same session.

CI runs real compilers on Linux (GCC and Clang), macOS (Clang) and Windows
(WinLibs GCC). Linux/macOS jobs require working ASan and UBSan. MinGW-w64 GCC
ships no sanitizer runtime, so the Windows job does not require one and its
`%check` tests skip. A required backend cannot silently skip tests. The Web
PTY regression exercises edit, compile, run and input interruptions,
recovery, descendant cleanup and terminal CRLF input on each native runner.
Windows validation belongs to the native WinLibs CI job; no local Windows
cross-build is required.
