# c-shell

An interactive shell for the C language. To check how a piece of syntax
behaves, skip the new-file / write-`main` / compile / run loop — just type it
at the prompt.

```
c-shell 0.1.0  ·  cc (GCC) 16.1.1 (default std gnu23)
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
**real compiler on your machine**. That is deliberate: integer promotion,
bit-field layout, evaluation order, undefined behavior — the things most worth
testing interactively — only have trustworthy answers from the compiler you
actually use. An interpreter's answer speaks only for the interpreter.

## Installation

Requires a Rust toolchain to build, and any C compiler (gcc / clang / MSVC)
at runtime.

```sh
cargo install --path .
```

On startup the compiler is resolved in the order `--cc` flag → `$CC` → PATH
(`cc`, `gcc`, `clang`, `tcc`; on Windows `gcc`, `clang`, `cc`, `clang-cl`,
`cl`). The first candidate that passes every check wins. Capabilities are
probed by trial compilation, never derived from version strings — version
numbers lie (`gcc` on macOS is actually clang, and distros backport
features).

```sh
c-shell                              # auto-detect a compiler
c-shell --cc clang --std c23         # pin compiler and standard
c-shell --flags -O2 -march=native    # extra flags, passed through untouched
c-shell --timeout 30                 # seconds before a runaway program is killed
```

The language standard defaults to **whatever your compiler defaults to**
(gnu23 for gcc 16, gnu17 for clang 22; the banner shows what was detected) —
the behavior you see here is the behavior a plain `gcc foo.c` would give you.
Pin a standard with `--std c17`, or switch mid-session with `%std c17` /
`%std default`. One exception: when the compiler's default mode is too old to
compile `_Generic` (MSVC without `/std:` is C89), the standard is auto-raised
to c17/c11 and the banner says so. **C11 is a hard floor**: a compiler that
cannot reach it in any mode is skipped, and if no candidate qualifies,
c-shell reports an error and exits.

## Usage

| Input | Behavior |
|---|---|
| `x + 1` | evaluated, value printed as `Out[n]` |
| `x + 1;` | trailing `;` runs it silently (as in IPython) |
| `int x = 41;` | declaration, visible to later inputs |
| `int f(int a) { ... }` | function definition, hoisted to file scope |
| `#include <time.h>` | likewise |

Common headers (stdio/stdlib/string/math/stdbool/stdint/limits/ctype) are
pre-included and `-lm` is linked by default. `-Wall -Wextra` is on by
default: the warning is often exactly the thing you came to check.

### Commands

`%`-prefixed, in the spirit of IPython:

```
%help      %quit      %reset     %history
%src       %undo      %cc        %std       %flags
```

`%src` prints the complete C program the session assembles — when you want to
know exactly what the tool is doing on your behalf, look there.

## How it works

**Accumulate and replay.** Every evaluation reassembles and reruns the whole
session. Session variables are therefore ordinary locals in `main` — no
global symbol table, no splitting declarations from initializers.

**Classification by trial compilation.** Whether a line is an expression, a
statement or a file-scope item is not decided by parsing C; it is wrapped
each way and compiled, and whichever form the compiler accepts is the answer.
The only judge that always agrees with the compiler is the compiler.

**Value printing.** `_Generic` selects a print *function*, which is then
called — selecting a call expression instead would type-check every
unselected branch against the wrong argument type and fail to compile.

**Only state-changing inputs are kept.** An expression at the prompt is
usually a question (`x + 1`, `sizeof(int)`); questions are answered and
forgotten, or the session would grow slower with every one. Only inputs that
may change state — assignment, `++`/`--`, function calls — are replayed.
`%src` shows what was actually kept.

**Diagnostics are remapped.** The compiler sees a generated file with a
prelude and all earlier statements above your input, so its line numbers are
meaningless at the prompt. Every location — including the line-number gutter
of gcc's source excerpts — is rewritten to input-relative lines. Locations
pointing into generated scaffolding are never presented as your code.

## Known limitations

- **Side effects replay.** Every evaluation reruns the session, so `scanf`,
  file writes and `time()` execute again each time. Pure syntax exploration
  never notices; be aware of it otherwise.
- **No redeclaration in the same scope.** Declaring `int x` twice is an
  error, because it is one in C. Use `%undo` to drop the previous one.
- **The purity heuristic is conservative.** Any expression containing a
  function call is kept, even if the function happens to be pure.
- **Crashing or timed-out inputs are not committed** — otherwise every later
  replay would crash again.
- No session save/load.
- MSVC must be started from a Developer Command Prompt, or `INCLUDE`/`LIB`
  are missing.

Before working on the code, read [DESIGN.md](DESIGN.md): it records the
architecture decisions, the open state-model question, and several traps you
need to know about before changing things.

## License

MIT
