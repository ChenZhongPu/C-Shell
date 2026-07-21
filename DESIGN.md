# c-shell Design Notes

Settled architecture decisions, the one big open problem, and the traps buried
in the code that you must know about before changing it. README is for users;
this file is for whoever develops the tool next.

Status: v0.1.2 working. ~4200 lines of Rust, 73 tests (47 unit + 26
end-to-end smoke), clippy/fmt clean, English UI. Verified on Linux with gcc
16.1.1 and clang 22.1.6. CI exercises the default macOS compiler and two
Windows driver dialects (a GNU-style driver and MSVC); see
`.github/workflows/ci.yml` for the exact matrix.

---

## 1. Current state

| Module | Responsibility |
|---|---|
| `toolchain.rs` | compiler detection, cached capability probing, GNU/Clang/MSVC flag dialects, default-std detection, `_Generic` floor |
| `eval.rs` | trial-compile classification, compile, run with timeout, crash diagnosis |
| `editor.rs` | syntect highlighting, completion, multi-line input and indentation |
| `main.rs` | CLI, REPL, `-e`, script and piped-input modes |
| `lex.rs` | byte-level scan: bracket balance, literals/comments, purity and identifier heuristics |
| `errmap.rs` | diagnostic line remapping, stale-warning filtering |
| `tests/smoke.rs` | end-to-end tests driving the real binary and compiler |
| `proc.rs` | child deadlines, output capture and timeout-path process-tree cleanup |
| `magic.rs` | `%` commands and optional `clang-format` presentation |
| `codegen.rs` | program assembly, `_Generic` value-printing and type-name runtime |
| `session.rs` | session state, numbered history and completion vocabulary |
| `ui.rs` | terminal styling, startup banner |

Implemented: expression evaluation with value printing, statements, automatic
file-scope hoisting for functions/`#include`/`typedef`, multi-line input,
syntax highlighting, compiler detection and mid-session switching, warning
pass-through, diagnostic remapping, completion-marker validation, live
terminal output with bounded capture, crash reporting and timeout handling,
missing-semicolon repair, blank-line confirmation for a completed interactive
`if`, `if`/`else` batch lookahead, control/do-while/preprocessor continuation,
tab completion (magics, C keywords, session names), persistent input history,
`-e`/script/piped-input batch modes,
`%help %quit %clear %reset %history %src %type %undo %cc %std`, deadlines and
timeout-path tree cleanup for compiler/probe/user-program children, cached
compiler capability probes, CI for 4 platform configs, and a tag-triggered release
workflow.

---

## 2. Settled decisions

**Drive a real compiler; never interpret.** The tool exists to answer "what
does *my* compiler do with this invocation". Integer promotions,
implementation-defined layout, ABI choices and diagnostics must come from the
user's own toolchain. Undefined or unspecified behavior remains undefined or
unspecified: c-shell reports one observation, not a portable guarantee. An
interpreter would still describe the interpreter rather than the selected C
toolchain.

**Use a lexical file-scope heuristic, then classify by trial compilation.**
`eval::looks_file_scope` sends preprocessor directives, typedefs, tag
definitions and function-shaped inputs to a file-scope attempt first. Other
inputs never receive file scope as a fallback, because moving a declaration
out of `main` can silently change shadowing and redeclaration semantics. The
compiler validates the heuristic and arbitrates expression versus statement;
classics like `foo * bar;` follow the compiler's current typedef context.

**Accumulate and replay.** Every evaluation rebuilds and reruns the whole
program. Block-scope declarations are ordinary locals in `main`; inputs
classified as file-scope items are emitted above it. There is no separate
symbol table or declaration/initializer state store. The cost (side effects
replay) is the subject of §3.

**Forget only bare expressions judged pure.** A bare expression is normally a
question and is forgotten after its value is printed. Bare assignments,
`++`/`--` and calls are retained according to the deliberately conservative
`lex::may_have_side_effects` heuristic. Every successfully evaluated
statement/declaration and file-scope item is retained without purity analysis;
notably, adding a trailing `;` turns an expression into a stored statement.

**The language standard follows the compiler's default; `_Generic` is the
capability floor.** The original design forced `-std=c17` and was reversed:
it bought no purity (GCC accepts many GNU extensions without `-pedantic`) and
contradicted the tool's thesis of matching a plain compiler invocation. The
default mode is detected by compiling and running a probe that reports
`__STDC_VERSION__` and `__STRICT_ANSI__`; that run doubles as the
works-at-all self-test. When the default mode cannot compile `_Generic`
(MSVC without `/std:` is the common case), detection tries c17 and then c11
and marks the selected mode as automatic. The final gate compiles a
representative subset of the value-printer runtime (`inline`, `_Bool` and
`_Generic`) under the selected mode rather than inferring support from a
version string. An unsupported explicit standard is rejected.

**`%type` is a portable `_Generic` query, not reflection.** C has no portable
way to stringify an arbitrary type. The generated runtime therefore maps
scalar types and pointers to scalar types to canonical names; the controlling
expression is not evaluated. Compatible-type matching cannot recover typedef
spelling, and normal lvalue/array/function conversions remove top-level
qualifiers and decay arrays/functions to pointers. `_Generic` has no "any
aggregate" wildcard, so `codegen.rs` lexically collects complete named
`struct`/`union` definitions and simple anonymous aggregate typedefs from the
session and adds those exact type spellings to each query. Labels retain the
available canonical name (`Struct Point`, `Union Value`); anonymous aggregate
typedefs use their typedef name, while aliases of named tags resolve to the
tag name because compatible typedef and tag types cannot both appear in one
`_Generic` association list. Truly anonymous aggregates and types outside the
finite table deliberately return `<unrecognized type>` rather than relying on
GCC `typeof`, compiler diagnostic wording or debug-info formats unavailable on
other toolchains.

**Diagnostics must be remapped.** The compiler sees a generated file with a
prelude and all earlier inputs above the new text; its line numbers are
meaningless at the prompt. `errmap` rewrites locations attributable to the
newest input and labels older/scaffolding locations as session context.
Without that remapping, otherwise-correct diagnostics point at generated
lines the user never typed.

**Successful capability probes are cached, but never trusted indefinitely.**
Cold detection needs several compiler invocations; repeating those on every
launch adds latency without adding information. `toolchain.rs` caches a
successful result for seven days. Its key includes the canonical compiler
path and file metadata, requested standard, c-shell version, and environment
variables that affect driver/header/library/SDK resolution. Any mismatch,
expiry or malformed cache entry falls back to the real probes; cache I/O is
best effort and can never prevent startup.

---

## 3. The open problem: the state model

### Symptom

Accumulate-and-replay replays side effects. `scanf` prompts the user again on
every later evaluation, file writes repeat, and `%src` is an ever-growing
ledger rather than the current state. (`rand()` is fine: without `srand` it
is deterministic in C.)

Goal: closer to Python — the session holds *current state*, not input
history, and during normal healthy-runner operation each input executes once.
Crash recovery is necessarily weaker, as described below.

### Why it is hard — two findings

**The chosen compiler-agnostic resident design needs declaration parsing.** A
variable declared in step 1 does not automatically exist in step 2's
translation unit, so the runner must recover its name and type (or depend on a
compiler-specific incremental frontend that already did so). Snapshotting
needs the same knowledge to dump variables. For the portable slot-runner
proposed below, `tree-sitter-c` is therefore on the critical path.

**Arbitrary C state cannot be serialized generically.** Plain scalars and
resource-free arrays/structs can be dumped, but reconstructing `malloc`'d
pointer graphs requires application-specific ownership/type knowledge, and a
`FILE*` has no portable serialized representation. A serializable variable
may also have been computed through an unserializable resource, which forces
either dependency analysis or replay fallback. Python
gets away with it because objects live in a process that never dies — the C
equivalent of Python's model is a resident process, not a snapshot. A
snapshot-only design would therefore support only a restricted subset or
need replay fallback; it is not the state model selected here.

### Survey of existing C REPLs (2026-07)

| Project | Family | Takeaway |
|---|---|---|
| igcc | replay | gcc recompile, output prefix-trimming; same model as us |
| crepl (l-m.dev) | replay | rule buckets + linear undo/redo; picked tcc because its author found the alternatives too slow, illustrating the compiler-fidelity/performance tradeoff |
| c-repl (Evan Martin) | resident | one .so per line, dlopen'd into a child; state shared via the dynamic linker |
| cling / clang-repl | resident (in-process) | incremental TU + ORC JIT, transactional rollback; welded to clang |
| evcxr (Rust) | resident | variables in a `HashMap<String, Box<dyn Any>>`, **moved in and out by parameter passing, no linker involved**; crash loses variables |
| picoc / Ch / CINT | interpreter | rejected (fidelity) |

crepl illustrates the replay tradeoff: it chose tcc to keep repeated builds
fast, which means its answers describe tcc. c-shell's differentiator is the
user-selected compiler, so its long-term performance plan cannot require
replacing that compiler.

### Chosen architecture: journaled resident runner

The key import is evcxr's move: share state by *parameter passing*, not
through the linker. In C it is stronger than in Rust, because slots can be
addressed through lvalue macros:

```c
#define x   (*(int *)cs_slot[0])       /* lvalue: &x stable, sizeof right */
#define arr (*(int (*)[5])cs_slot[1])
CS_EXPORT void cs_step(void **cs_slot) { /* user input appears verbatim */ }
```

```
c-shell (Rust) ──pipe──> runner child (slot table + the user's heap/files)
     │            per input: compile one small TU → dlopen/LoadLibrary → cs_step()
     └── journal (today's session.rs) = crash recovery + %undo + %save/%load
```

- Each step library exports exactly one c-shell entry symbol (`cs_step` —
  trivial even on MSVC) and imports no symbols from the Rust runner. Normal C
  runtime/system-library imports still exist. Avoiding runner imports removes
  the Windows import-library problem that once made this option expensive.
- `&x` is stable across inputs; heap allocations and `FILE*` can survive in
  the runner process, and `scanf` runs once on the normal hot path. Per-input
  compilation no longer includes the accumulated statement journal. On
  Windows, every step DLL must use the same dynamic CRT (`/MD`); separate
  static CRT instances would make allocation and `FILE*` ownership unsafe
  across DLL boundaries. Compilation is still not strictly constant while
  shared declarations and recompiled session function definitions continue
  to grow.
- **Replay is demoted, not deleted: it becomes a best-effort recovery layer.**
  After a runner crash, the journal can reconstruct in-process state, and
  recorded stdin can make prior reads repeatable. It cannot roll back or
  safely reproduce arbitrary external effects such as file writes, network
  operations, time or interactions with other processes; recovery must warn
  about that limitation rather than promise exactly-once execution.
- Redeclaration can become a Python-style rebind (`int x` then `double x` =
  new slot).
- Cost: tree-sitter-c lands on the critical path; the hard 20% is
  constructing the one-more-indirection type spelling for gnarly declarators
  (function pointers, arrays).

Three traps already identified for that design:

1. **Never unload a step library.** String literals live in its `.rodata`,
   and function pointers taken earlier point into old step code; unloading
   dangles them.
2. **`static` locals in session functions split state**: function definitions
   are recompiled into every step TU, so each step gets its own copy. Rare;
   warn when detected.
3. **VLAs cannot persist** (size unknown at compile time — no slot for them);
   block-scope ones that die within a step are unaffected.

Staging: v0.2 stdin recording + journal polish (the recovery layer — nothing
wasted regardless of what follows) → v0.3 tree-sitter declaration parsing
(first used only for better classification and redeclaration detection) →
v0.4 the slot-runner hot path, feature-gated alongside replay.

References: [crepl](https://l-m.dev/cs/crepl/)
([source](https://github.com/l1mey112/crepl)),
[igcc](https://codeberg.org/andybalaam/igcc),
[evcxr HOW_IT_WORKS](https://github.com/evcxr/evcxr/blob/main/evcxr/HOW_IT_WORKS.md),
[c-repl](https://github.com/martine/c-repl), clang-repl (LLVM docs).

---

## 4. Known gaps

- **Limited introspection commands**: `%type` covers scalar expressions and
  session-visible named aggregate categories, but `%layout` (struct
  offsets/padding), `%expand` (preprocessor-only view of a macro), `%asm`,
  exact anonymous-aggregate/type-alias reflection and multi-compiler
  comparison remain open. These, not the REPL loop, are what would set the
  tool apart. A GCC/Clang disagreement can reveal implementation-defined or
  unspecified behavior, undefined behavior, differing extensions/ABIs, or a
  compiler bug; the comparison should report the environment rather than
  claim a single cause.
- **MSVC status (first CI contact 2026-07):** detection, the legacy default →
  `/std:c17` auto-raise, and `_Generic` value printing all worked on the
  real cl 19.51. Two papercuts found and fixed: cl echoes the bare source
  filename to stdout (now dropped in `errmap::remap`), and C4552/C4553
  joined the suppressed unused-family list. Still anticipated but not yet
  seen: antivirus briefly locking freshly-exited executables (unique exe
  names per step would be the fix); ANSI colors on legacy conhost.
- No session save/load. `Session` is not serializable yet; adding serde is
  straightforward.
- UI strings are English but scattered across `main.rs` / `magic.rs` /
  `eval.rs` / `toolchain.rs`; centralize before attempting i18n.
- Highlight theme is hardcoded (`base16-ocean.dark`); poor on light terminals.
- Arbitrary token-level line splitting is not inferred. Inputs such as
  `value +` followed by the right operand, or a declaration split after its
  type name, need an explicit trailing `\\`; recognizing every continuation
  point without a C parser would also make intentionally incomplete snippets
  impossible to submit.
- The value printer handles the standard boolean/integer/real-floating types,
  `char` strings and common object pointers. Struct/union, complex and `void`
  expressions have no printer; a silent expression probe now keeps them from
  being misclassified as statements and the UI explains the missing
  `Out[n]`, but richer value formatting is still absent.
- The purity heuristic keeps any expression containing a call, even calls to
  pure functions. (Hardened 2026-07 after a P0: calls disguised as `f/**/()`,
  `(f)()`, `(*fp)()` were misjudged pure and their side effects silently
  dropped from the session. Call detection now runs on code bytes with
  comments invisible; `(` after an identifier, `)` or `]` is a call. Casts
  like `(int)(x)` are indistinguishable without a symbol table and are kept —
  an extra replay is cheap, lost state is not. AST-level judgment arrives
  with tree-sitter in §3's staging.)
- **Type-ahead at a real terminal is dropped between prompts** (pre-existing,
  verified identical before/after the process-isolation work): rustyline
  flushes the tty queue when re-entering raw mode, so a line typed while a
  program is still running is lost. Piped input is unaffected, which is why
  the smoke tests never see it. Fix would mean patching or replacing the
  raw-mode toggle; noted, not urgent.
- **Process-tree cleanup is timeout-path cleanup, not a sandbox.** On Unix a
  process group is killed only when the direct child is still running at the
  deadline. A detached child, or a child left behind by a parent that exits
  successfully, can survive. Windows uses `taskkill /T`, which likewise
  misses some orphans. Job Objects (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) are
  the airtight Windows upgrade; Unix would also need an explicit policy for
  descendants after normal parent exit.

---

## 5. Traps in the code (read before changing)

**The `M_VAL` marker must be emitted inside the print helpers**
(`codegen.rs`, `RUNTIME`). An argument is fully evaluated before the call, so
the marker lands after all of the expression's own side effects. Emitting it
at the call site instead files `puts("hi")`'s own output under its return
value (`Out[1]: hi\n3`). This bug was introduced once, while removing the
`_n` bindings.

**An MSVC probe cannot trust exit status alone.** `cl.exe` reports an unknown
option such as `/std:bogus` as warning D9002 and exits successfully; clang-cl
can similarly report an unused argument. Capability probes treat these
specific diagnostics as failure, or an invalid explicit `--std` can be
silently accepted. Changing this rule also requires a cache-schema bump so a
previous false-positive result cannot survive on disk.

**gcc accepts nested functions under `-std=c17`** (GNU extension); clang
rejects them. Function definitions must be routed to file scope by the
heuristic up front — trial-compile failure cannot be relied on to do it, or
gcc silently buries the function inside `main` and the session breaks the
moment you switch to clang. The fallback (file-scope-looking input that only
compiles inside `main`) still exists for the heuristic's false positives,
but it is no longer silent: the outcome carries `demoted` and the prompt
prints "(note: kept inside main — it did not compile at file scope)".

**File scope must not be a fallback slot** (`eval.rs`, `attempt`). It is
wider than `main`: `int dup = 2;` after `int dup = 1;` must be a
redefinition error, but a file-scope fallback would silently make it a
global shadowed by the earlier local. The heuristic alone decides file-scope
membership; trial compilation only arbitrates expression vs statement.

**Interactive completeness and batch completeness are different policies**
(`editor.rs`). Structural checks cover open brackets/literals, function and
control headers awaiting a body, mandatory `do ... while`, backslash
continuations and conditional preprocessor groups. Only a completed leading
interactive `if` adds blank-line confirmation so it can still receive `else`;
functions and other closed blocks submit immediately. Batch mode instead holds
a complete leading `if` for one physical line of
lookahead and submits it before an unrelated next line. Reusing the
interactive blank-line policy in scripts would accidentally merge the next C
statement into the same input.

**The validator must recognize a function signature awaiting its body**
(`lex::awaits_body`). `int fact(int n)` has balanced brackets; without the
check it is submitted immediately, then "repaired" with a semicolon into a
forward declaration — functions in Allman style become impossible to type.

**Diagnostic gutters must be rewritten too** (`errmap::remap_gutter`).
Remapping only the header line leaves gcc's source excerpt showing the
generated file's `42 |` under a header that says `<input>:1` — exactly the
kind of contradiction that makes a beginner distrust the tool. Gutter
numbers pointing at scaffolding are blanked, never passed off as user code.

**syntect must use `default-fancy`, not the default `default-onig`.** The
latter pulls in oniguruma, a C library — awkward for a tool whose whole
point is driving C compilers, and a needless native build dependency.

**Load `SyntaxSet`/`ThemeSet` exactly once** (`editor.rs` constructor).
Highlighting runs on every keystroke; loading in the callback is visible
input lag.

**Stale warnings must be filtered** (`errmap::only_new`). Every evaluation
recompiles the whole session, so input 2's warning would otherwise reappear
under every later input. gcc's `file: In function 'f':` group banner carries
no line number: hold it back until something under it survives, or it is
left stranded above nothing.

**MSVC pins macro-argument diagnostics to the invocation's first line**
(`errmap::remap`, the `wrapped` flag). The traditional cl preprocessor
attributes a diagnostic arising inside a multi-line macro invocation to the
line of the macro *name* — for the Expr slot that is the `CS_PRINT((`
wrapper line just above the input, which strict attribution labels
`<session>` and the warning filter then silently drops. Symptom: values
print but warnings vanish, only on MSVC, only for expressions. Wrapper-line
anchors are clamped into the input; nothing but the wrapper lives on those
lines, so the clamp cannot mislabel foreign diagnostics.

**Every compiler, capability probe, generated user program and optional
`clang-format` process goes through the deadline-aware helpers in `proc`.**
The module owns three subtleties that must not be unbundled: (1) the child
gets its own Unix process group, and on timeout the *group* is killed —
killing only the child leaves forked descendants running and, worse, holding
the output pipe so the reader threads block forever; (2) when stdin is a
real terminal the child's group must be handed the foreground
(`tcsetpgrp`, both from `pre_exec` and the parent — the double set closes a
race) or any tty read gets the program stopped by SIGTTIN, and SIGTTOU must
be ignored before the parent can take the terminal back; (3) readers drain
into shared buffers and are *abandoned* after a grace period, never joined
unconditionally — a `setsid` escapee can hold both pipes open forever, and
that must cost at most two reader threads, not the REPL. The group is only killed on the
timeout path while the group leader is still represented by the unreaped
`Child`, so its PID cannot yet be recycled; killing after reaping could target
an unrelated process group.

**Live output filtering must work across arbitrary pipe chunks** (`eval.rs`,
`LiveFilter`). Replay output before `M_NEW`, value bytes after `M_VAL`, and
protocol markers are suppressed; only the newest input's own stdout/stderr is
forwarded. Retain only a trailing byte sequence that is an actual marker
prefix. Retaining `marker.len() - 1` bytes unconditionally delays short prompts
such as `"name: "` and defeats the purpose of streaming before `scanf`.
Because streamed bytes are no longer present in `Outcome::output`, the filter
also records whether the last visible byte was `\n`; the renderer must insert
one before warnings, `Out[n]` or the next rustyline prompt. Otherwise a prompt
can repaint a no-newline `printf` out of view, or the value appears as
`textOut[n]`. Capture remains bounded at `proc::MAX_CAPTURE_BYTES`, but readers must keep
draining and discard excess bytes or the child will deadlock on a full pipe.

**An input must reach `M_DONE` before it is committed** (`codegen.rs`,
`eval.rs`, `main.rs`). Exit status zero is insufficient: `exit(0)`, `_Exit`
or a top-level `return` can terminate before the generated epilogue, and
committing that input would silently prevent every later input from running.
Crashes, timeouts and capture overflow likewise remain uncommitted.
