# c-shell

An interactive shell for the C language. To check how a piece of syntax
behaves, skip the new-file / write-`main` / compile / run loop — just type it
at the prompt.

```
c-shell 0.2.2  ·  cc (GCC) 16.1.1 (default std gnu23)
In [1]: int x = 41;
In [2]: x + 1
Out[2]: 42
In [3]: 3 / 2
Out[3]: 1
In [4]: 3.0 / 2
Out[4]: 1.5
In [5]: -1 > 0u
<input>:1:4: warning: comparison of integer expressions of different signedness
    1 | -1 > 0u
      |    ^
Out[5]: 1
```

## Not an interpreter

c-shell assembles your input into a complete C program and hands it to the
**real compiler on your machine**. That is deliberate: integer promotions,
implementation-defined details, ABI choices and compiler diagnostics are most
useful when they come from the toolchain you actually use. An interpreter's
answer speaks only for the interpreter.

A result is still an observation of one compiler invocation, not a guarantee
from the C standard. In particular, undefined or unspecified behavior may
change with compiler version, flags, surrounding code or another execution.

## Installation

Requires a Rust toolchain to build and, at runtime, a C compiler compatible
with one of the supported GNU/Clang or MSVC command-line dialects. GCC, Clang,
MSVC and tcc are the built-in detection candidates.

Install the published crate from crates.io:

```sh
cargo install c-shell
```

Or install directly from a source checkout:

```sh
cargo install --path .
```

With `--cc`, only that compiler is tried. Otherwise startup checks `$CC`, then
PATH (`cc`, `gcc`, `clang`, `tcc`; on Windows `gcc`, `clang`, `cc`,
`clang-cl`, `cl`). `--cc` and `$CC` must name one executable or path, not a
shell command containing flags. The first candidate that passes every check
wins. Capabilities are probed by trial compilation, never derived from version
strings — executable names can be aliases, and distros backport features. For
example, Apple Command Line Tools may provide `/usr/bin/gcc` as an Apple Clang driver, while
GNU GCC installed separately through Homebrew or MacPorts is genuine GCC.

```sh
c-shell                              # auto-detect a compiler
c-shell --cc clang --std c23         # request compiler and mode; verify the banner
c-shell --timeout 30                 # deadline for each compilation and program run
c-shell -e 'sizeof(long)'            # evaluate and exit: bare value on stdout,
                                     # diagnostics on stderr, exit code on failure
c-shell --script demo.csh            # run inputs from a file, then exit
c-shell --quiet                      # interactive, but skip the banner
echo '1 + 1' | c-shell               # piped input: transcript output, no banner
```

Terminal handling is automatic: when stdout is not a terminal (or
`TERM=dumb`, or `NO_COLOR` is set) colors are off; when stdin is not a
terminal the banner, prompts and farewell are suppressed and input is read
in batch mode — which also accumulates multi-line definitions, so scripts
and pipes can contain full function bodies.

The language standard defaults to **whatever your compiler defaults to**
(gnu23 for gcc 16, gnu17 for clang 22; the banner shows what was detected).
This matches the compiler's language mode, not its entire command line:
c-shell also adds diagnostics, output and platform link flags plus generated
REPL scaffolding.
Select a supported mode with `--std c17`, or switch mid-session with `%std
c17` / `%std default`. One exception: when the compiler's default mode cannot
compile `_Generic` (MSVC without `/std:` is the common case), c-shell tries
c17 and then c11, and the banner reports an automatic raise. `_Generic`
support is the actual capability floor: a candidate that cannot compile a
representative value-printer probe in the selected mode is skipped. An
unsupported explicit `--std` is an error both at startup and in `%std`.

## Usage

| Input                  | Behavior                                      |
| ---------------------- | --------------------------------------------- |
| `x + 1`                | evaluated, value printed as `Out[n]`          |
| `x + 1;`               | trailing `;` runs it silently (as in IPython) |
| `int x = 41;`          | declaration, visible to later inputs          |
| `int f(int a) { ... }` | function definition, hoisted to file scope    |
| `int main() { ... }`   | rejected with guidance; c-shell supplies main |
| `#include <time.h>`    | likewise                                      |

A completed interactive `if` uses blank-line confirmation: press Enter on the
empty continuation line to submit it, or type `else` / `else if` there to
continue the same statement. Functions and loops submit directly when their
required closing syntax is entered. A braced `struct`, `union` or `enum`
definition remains open after `}` until its mandatory declaration semicolon is
entered. Control headers with their body on the next line, mandatory
`do ... while`, and conditional preprocessor groups through `#endif` are
accumulated automatically. In scripts
and pipes, one-line lookahead attaches `else` without requiring a blank line.

c-shell generates its own `main` to host session locals and the output
protocol. Pasting a complete program such as `int main() { puts("hi"); }`
therefore produces a direct explanation instead of a compiler redefinition or
GCC nested-function warning: enter the statements from the body directly and
omit the final `return`.

Common headers (`stdio`, `stdlib`, `string`, `math`, `stdbool`, `stdint`,
`inttypes`, `stddef`, `limits`, `ctype`, `stdarg`, `time`) are pre-included, so
no `#include` is needed for the everyday library; `%header` lists them. Unix
builds link `-lm`; Windows math functions come from the C runtime. GNU-style GCC/Clang drivers use
`-Wall -Wextra`; MSVC-style `cl`/`clang-cl` drivers use `/W3`. The warning is
often exactly the thing you came to check.

### Rebinding declarations and definitions

When a block-scope declaration fails with a compiler redeclaration diagnostic,
c-shell retries it inside a new nested block. If the complete program then
compiles, later inputs remain inside that block and see the shadowing binding:

```text
In [1]: int x = 1;
In [2]: x = 5;
In [3]: int x = 2;
(opened a nested scope to shadow an earlier declaration)
In [4]: x
Out[4]: 2
```

`%src` shows the actual braces; no C declaration is rewritten into an
assignment. Consequently C's own scope rules still apply—for example, in
`int x = x + 1;` the initializer refers to the newly declared `x`, not the
outer one.

A rejected file-scope redefinition is handled differently: c-shell tries the
new function or type in place of each previous non-preprocessor file-scope
input, and commits only the substitution for which the real compiler accepts
the entire session:

```text
In [1]: int f(int n) { return n * 2; }
(added at file scope)
In [2]: f(3)
Out[2]: 6
In [3]: int f(int n) { return n * 3; }
(replaced previous file-scope definition)
In [4]: f(3)
Out[4]: 9
```

The old item is replaced at its original position to preserve declaration
order. A function-shaped input is never retried inside `main`; this prevents
GCC nested-function extensions from creating a session that Clang or MSVC
interprets differently.

Both forms only ever move forward: there is no undo. A binding is corrected by
declaring or assigning it again, and a definition by defining it again. When a
retained statement blocks a replacement—say it calls a function whose signature
you are changing—`%reset` is the way out.

![demo](demo.gif)

### Commands

`%`-prefixed, in the spirit of IPython:

```
%help      %quit      %clear     %reset     %edit [n]
%src       %header    %type      %time      %timeit
%undo      %cc        %std
```

`%help` lists the commands and nothing else, so it stays a one-screen
reference. `%help --verbose` appends the usage notes: which inputs are
retained, how continuation and rebinding behave, what the value printer and
`%type` cover, and how the `scanf` tape replays.

`%clear` erases the terminal display and returns the cursor to the top without
changing variables, retained C code or the input counter.

`%src` defaults to the user-facing program: current file-scope definitions and
retained statements inside a clean `main`, including any rebinding braces, but
without printers or protocol markers. `%src --raw` prints the complete
compiler input with `CS_PRINT`, `_Generic` and marker machinery. Either view is
formatted with `clang-format` when available; formatting is presentation only,
has a three-second deadline, and evaluation still compiles the unformatted
generated source.

`%edit` copies the most recent C input into the terminal prompt; `%edit 12`
retrieves `In[12]`. Numbered C inputs—including failed compilations and
forgotten pure queries—remain addressable until `%reset`. The command itself
does not compile, submit or consume an input number: it returns to the same
`In[n]` prompt with the selected text pre-filled and the cursor at its end.
Modify it with the normal line-editor keys and press Enter when ready; even
unchanged text is submitted only at that point as a fresh `In[n]`, while the
original numbered input remains unchanged. Ctrl-C or clearing the buffer
cancels naturally. The eventual submitted block enters process-local Up/Down
recall and follows the normal shadowing or file-scope replacement rules.
`%edit` is available only in the interactive REPL.

`%type <expression>` reports the expression type without evaluating that
expression, committing code or consuming an `In[n]` number:

```text
In [1]: const char *message = "hello";
In [2]: %type message
const char *
In [2]: %type 1 + 0.5
double
```

Aggregate categories retain an available name:

```text
In [2]: struct Point { int x, y; } point;
(added at file scope)
In [3]: %type point
Struct Point
In [3]: typedef union { int i; double d; } Value;
(added at file scope)
In [4]: Value value = { 1 };
In [5]: %type value
Union Value
```

The implementation uses C11 `_Generic`, not compiler-specific `typeof`. It
names scalar types and pointers to scalar types. Generic matching sees
compatible expression types rather than source spelling: scalar typedefs and
aliases of named tags are canonicalized, top-level qualifiers are removed by
lvalue conversion, and arrays/functions decay to pointers. Complete named
aggregate definitions visible in the session, plus simple `typedef struct {
... } Name` and `typedef union { ... } Name` forms, are added to each query
dynamically. A typedef of a named tag uses the tag's canonical name; an
anonymous aggregate typedef uses its typedef name. Truly anonymous aggregates
and other types outside the table report `<unrecognized type>`.

Tab completes `%` commands, C keywords, stdlib staples and retained session
identifiers of at least two characters. When a prefix is ambiguous, Tab opens a
dropdown menu under the cursor listing every candidate, IPython-style; Tab and
the arrow keys move through it and Enter accepts. Up/Down recalls up to 1000
inputs from the current process so multi-line blocks can be recovered, but
nothing is loaded or saved across launches. There is no `%history` or history
file. The
separate current-session input archive exists only for direct `%edit n` lookup
and is cleared by `%reset`.

## How it works

**Accumulate and replay.** Every evaluation reassembles and reruns the whole
session. Block-scope declarations become ordinary locals in `main`; a
compiler-approved redeclaration starts a nested shadowing scope that encloses
later statements. Items recognized as file-scope code are emitted above
`main`, and approved redefinitions replace an older item in place. There is
no separate symbol table or declaration/initializer state store.

**`scanf` uses a session-local stdin tape.** Direct calls are routed through a
small `vscanf` wrapper. Each dynamic call in the newest input requests one
fresh line, which is retained only in memory; historical calls receive those
recorded lines and never reconnect to the terminal. Calls inside functions,
recursion and loops therefore replay in their original order. `%src` annotates
the associated statement with only a request count—the bytes remain hidden.
`%reset` discards the tape, and no tape is written to a history file. If function replacement or changed control flow consumes a
different number of requests, evaluation stops with an explicit stdin-tape
divergence instead of silently prompting or waiting for the normal timeout.

**Classification uses a small heuristic plus trial compilation.** A lexical
heuristic routes function definitions, preprocessor directives, typedefs and
tag definitions to file scope; these inputs are never demoted into `main`,
which prevents GCC from silently accepting a nested function that Clang/MSVC
reject. The compiler arbitrates expression-versus-statement cases, but an
input whose final code token is `;` goes directly to the statement slot rather
than launching an expression compile that cannot succeed. Clearly braced
statements ending in `}` do the same; possible compound literals such as
`(int){7}` retain expression classification. After a redeclaration diagnostic,
alternate nested-scope or in-place replacement assemblies must compile as
complete programs before c-shell accepts them. File scope remains unavailable
as a general fallback.

**Value printing.** `_Generic` selects a print _function_, which is then
called — selecting a call expression instead would type-check every
unselected branch against the wrong argument type and fail to compile.
Session-visible named structs and simple anonymous struct typedefs get
recursive printers whose output uses designated-initializer form:

```text
Out[5]: {
    .name = (void *)0x7f068cbbac1b,
    .age = 30,
    .scores = {0, 7, 0},
    .next = NULL
}
```

An array is a harder case than it looks, because C converts one to a pointer
before `_Generic` can see its type — the printer is handed an address and has
no way to ask how many elements it addresses. So when a value does print as a
bare address, c-shell re-prints it through an array-aware wrapper that settles
the question with the real object at run time: an array begins at its own
first element, a pointer variable does not.

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

Element counts come from `sizeof`, so no declarator is parsed and a genuine
pointer keeps printing as an address. `char text[] = "hi"` still prints
`"hi"`: it reaches the string printer directly and never looks like an
address. Beyond 100 elements the rest are summarized as `... (n more)`.
Nesting is the one static property this cannot recover at run time, so arrays
deeper than two dimensions, and arrays whose elements have no printer of their
own (an array of pointers, for instance), show `<unprintable>` elements rather
than a guess.

Struct-member formatting never treats `char *` as a string: every pointer
member is `NULL` or an address, fixed arrays are traversed structurally with
the same 100-element bound, and a known nested struct calls its own printer. A top-level `struct P *` likewise
prints only its address; explicit `*ptr` requests expansion. This differs
intentionally from a top-level `char *` expression, where the user explicitly
asked for the existing string dereference. Small flat structs stay on one
line; larger, nested or array-bearing structs use indented multiline output.
Multi-declarator fields, function-pointer declarators, bit-fields, flexible
arrays, C11 anonymous members and unions conservatively produce a labelled raw
byte dump instead of guessed member names.

**Bare expressions judged pure are forgotten.** A bare expression at the
prompt is usually a question (`x + 1`, `sizeof(int)`), so it is answered and
forgotten. Bare expressions that may have effects — assignments, `++`/`--`
or calls — are retained. Successfully evaluated statements/declarations
(including an expression with a trailing `;`) and file-scope items are
retained without purity analysis. `%src` shows
what will actually be replayed.

**Diagnostics are remapped and sanitized.** The compiler sees a generated file
with a prelude and all earlier statements above your input, so its line numbers
are meaningless at the prompt. Locations attributable to the newest input —
including GCC source-excerpt gutters — are rewritten to input-relative lines.
Code generation records which other lines came from retained user inputs;
source excerpts from `CS_PRINT`, `_Generic`, marker calls and wrapper code are
removed while genuine earlier-input cross-references remain. Parser fallout
that names the marker macro's internal `do` token is removed; an incomplete
expression reports `expected expression at end of input` instead of blaming
the wrapper's closing `)`.

**Compiler capabilities are cached.** Successful startup probes are cached
for seven days under the platform cache directory
(`$XDG_CACHE_HOME/c-shell`, falling back to `~/.cache/c-shell`, on Unix;
`%LOCALAPPDATA%\c-shell` on Windows). The cache key changes with compiler file
metadata, requested standard, relevant toolchain environment, or c-shell
version. Expired or malformed entries simply cause fresh probes.

## Known limitations

- **Execution is not sandboxed.** Compilers and generated programs run with
  c-shell's user permissions and working directory. Do not evaluate untrusted
  code.
- **Most external side effects still replay.** Direct standard `scanf` calls
  use the stdin tape above, but file and process changes still repeat. Calls to
  known APIs such as `fopen`, `fprintf`, `remove`, `rename` and `system`
  produce a one-time English warning before execution. Detection is lexical
  and finite: wrappers and application-defined effects can still escape it.
- **The stdin tape currently wraps `scanf`, not every input API.** `fscanf`,
  `fgets`, `getchar`, raw `read(0, ...)`, an explicit `#undef scanf`, and code
  inside a precompiled library are not captured yet. One line is supplied per
  dynamic `scanf` request; programs that require multiple interactive lines in
  a single call should put the values on one line. In piped-REPL mode stdin is
  carrying C source, so a current `scanf` receives EOF; use `-e`, `--script`,
  or an interactive terminal when the evaluated program needs stdin.
- **Rebinding is C shadowing, not assignment.** A redeclared local lives in a
  nested block. Its declarator is already in scope during its initializer, so
  `int x = x + 1;` does not read the outer `x`. File-scope replacement also
  makes retained earlier calls use the new function when the session replays.
- **The purity heuristic is conservative.** Any expression containing a
  function call is kept, even if the function happens to be pure.
- **Indeterminate values remain undefined C.** Address-only struct-member
  formatting prevents c-shell from adding a `%s` memory dereference, but it
  cannot make `struct P p; p` well-defined when members were never initialized;
  reading an indeterminate pointer or scalar is itself not portable. Initialize
  structs (for example with `{0}`) when the value matters.
- **Not every C value is printable.** The `_Generic` runtime covers the
  standard boolean/integer/real-floating types, top-level `char` strings,
  common object pointers and session-visible structs as described above.
  Header-only/anonymous aggregate types that have no reusable C spelling,
  complex and `void` values are evaluated without `Out[n]` and receive an
  explanatory note. A member whose type is not in the scalar or generated
  aggregate table is shown as `<unprintable>` rather than being coerced.
- **Program output is bounded.** When stdin is a real terminal, output from
  the newest input is streamed immediately, so a prompt before `scanf` is
  visible. If the program leaves a partial line, c-shell appends a dim `↵`
  marker and the protective newline it inserted; a real `\n` needs no marker.
  With non-terminal stdin output remains buffered for deterministic transcripts
  and receives no marker. Capture is limited to 8 MiB per stream; an overflow
  is reported and the input is not committed.
- **Timeout cleanup follows the launched process tree.** On Unix the process
  group is killed when the direct child times out, but a child that detaches
  or outlives a parent that exits successfully can escape cleanup. Windows
  `taskkill /T` has similar orphan limitations.
- **Incomplete inputs are not committed.** Besides crashes and timeouts, a
  successful early `exit(0)`, `_Exit` or top-level `return` is detected by a
  missing completion marker; committing it would terminate every later
  replay before the new input runs.
- No session save/load.
- `cl.exe` needs an MSVC build environment with `INCLUDE`/`LIB` configured;
  a Developer Command Prompt is the usual way to obtain one.
- **Windows testing is still limited.** CI covers GNU-style and MSVC compiler
  drivers, but interactive behavior has not been extensively tested on real
  Windows installations. Windows feedback and bug reports are very welcome;
  please open an issue with the terminal and compiler details.

## Development

After cloning, enable the pre-commit gate (fmt + clippy + tests — the same
checks CI runs):

```sh
git config core.hooksPath .githooks
```

The toolchain is pinned by `rust-toolchain.toml` so local clippy and CI
clippy are the same version; new lints arrive when the pin is bumped, never
by surprise on push.

Before working on the code, read [DESIGN.md](DESIGN.md): it records the
architecture decisions, the open state-model question, and several traps you
need to know about before changing things.

## License

MIT
