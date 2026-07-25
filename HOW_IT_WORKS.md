# How c-shell Works

This document provides a detailed overview of the inner workings of `c-shell`, including how expressions and statements are evaluated, how `scanf` stdin replay operates, how types and value printers work, and how compiler diagnostics are sanitized and remapped.

## Architecture & Implementation Principles

### 1. Accumulate and Replay
Every evaluation reassembles and reruns the whole session. Block-scope declarations become ordinary locals in `main`; a compiler-approved redeclaration starts a nested shadowing scope that encloses later statements. Items recognized as file-scope code are emitted above `main`, and approved redefinitions replace an older item in place. There is no separate symbol table or declaration/initializer state store.

### 2. Session-Local `scanf` Stdin Tape
Direct `scanf` calls are routed through a small `vscanf` wrapper. Each dynamic call in the newest input requests one fresh line, which is retained only in memory; historical calls receive those recorded lines and never reconnect to the terminal. Calls inside functions, recursion and loops therefore replay in their original order. `%src` annotates the associated statement with only a request count—the bytes remain hidden. `%reset` discards the tape, and no tape is written to a history file. If function replacement or changed control flow consumes a different number of requests, evaluation stops with an explicit stdin-tape divergence instead of silently prompting or waiting for the normal timeout.

### 3. Trial Compilation & Classification
A lexical heuristic routes function definitions, preprocessor directives, typedefs and tag definitions to file scope; these inputs are never demoted into `main`, which prevents GCC from silently accepting a nested function that Clang/MSVC reject. The compiler arbitrates expression-versus-statement cases, but an input whose final code token is `;` goes directly to the statement slot rather than launching an expression compile that cannot succeed. Clearly braced statements ending in `}` do the same; possible compound literals such as `(int){7}` retain expression classification. After a redeclaration diagnostic, alternate nested-scope or in-place replacement assemblies must compile as complete programs before c-shell accepts them. File scope remains unavailable as a general fallback.

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

Element counts come from `sizeof`, so no declarator is parsed and a genuine pointer keeps printing as an address. `char text[] = "hi"` still prints `"hi"`: it reaches the string printer directly and never looks like an address. Beyond 100 elements the rest are summarized as `... (n more)`. Nesting is the one static property this cannot recover at run time, so arrays deeper than two dimensions, and arrays whose elements have no printer of their own (an array of pointers, for instance), show `<unprintable>` elements rather than a guess.

Struct-member formatting never treats `char *` as a string: every pointer member is `NULL` or an address, fixed arrays are traversed structurally with the same 100-element bound, and a known nested struct calls its own printer. A top-level `struct P *` likewise prints only its address; explicit `*ptr` requests expansion. This differs intentionally from a top-level `char *` expression, where the user explicitly asked for the existing string dereference. Small flat structs stay on one line; larger, nested or array-bearing structs use indented multiline output. Multi-declarator fields, function-pointer declarators, bit-fields, flexible arrays, C11 anonymous members and unions conservatively produce a labelled raw byte dump instead of guessed member names.

### 5. Purity Heuristic for Bare Expressions
A bare expression at the prompt is usually a question (`x + 1`, `sizeof(int)`), so it is answered and forgotten. Bare expressions that may have effects — assignments, `++`/`--` or calls — are retained. Successfully evaluated statements/declarations (including an expression with a trailing `;`) and file-scope items are retained without purity analysis. `%src` shows what will actually be replayed.

### 6. Remapping and Sanitizing Diagnostics
The compiler sees a generated file with a prelude and all earlier statements above your input, so its line numbers are meaningless at the prompt. Locations attributable to the newest input — including GCC source-excerpt gutters — are rewritten to input-relative lines. Code generation records which other lines came from retained user inputs; source excerpts from `CS_PRINT`, `_Generic`, marker calls and wrapper code are removed while genuine earlier-input cross-references remain. Parser fallout that names the marker macro's internal `do` token is removed; an incomplete expression reports `expected expression at end of input` instead of blaming the wrapper's closing `)`.

### 7. Compiler Capability Caching
Successful startup probes are cached for seven days under the platform cache directory (`$XDG_CACHE_HOME/c-shell`, falling back to `~/.cache/c-shell`, on Unix; `%LOCALAPPDATA%\c-shell` on Windows). The cache key changes with compiler file metadata, requested standard, relevant toolchain environment, or c-shell version. Expired or malformed entries simply cause fresh probes.
