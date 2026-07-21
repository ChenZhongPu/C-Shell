# c-shell Design Notes

Settled architecture decisions, the one big open problem, and the traps buried
in the code that you must know about before changing it. README is for users;
this file is for whoever develops the tool next.

Status: v0.1 working. ~2000 lines of Rust, 26 tests (19 unit + 7 end-to-end
smoke), clippy/fmt clean, English UI. Verified on Linux with gcc 16.1.1 and
clang 22.1.6; macOS and Windows (MinGW and MSVC) are covered by CI
(`.github/workflows/ci.yml`) once the repo is on GitHub, but have not been
exercised on real hardware.

---

## 1. Current state

| Module | Lines | Responsibility |
|---|---:|---|
| `toolchain.rs` | 435 | compiler detection, capability probing, GNU/Clang/MSVC flag dialects, default-std detection, C11 floor |
| `eval.rs` | 382 | trial-compile classification, compile, run with timeout, crash diagnosis |
| `lex.rs` | 257 | byte-level scan: bracket balance, literal/comment tracking, purity heuristic, function-signature detection |
| `errmap.rs` | 209 | diagnostic line remapping, stale-warning filtering |
| `main.rs` | 146 | CLI and REPL loop |
| `codegen.rs` | 145 | program assembly, `_Generic` value-printing runtime |
| `magic.rs` | 133 | `%` commands |
| `editor.rs` | 96 | syntect highlighting, multi-line input detection |
| `tests/smoke.rs` | 86 | end-to-end tests driving the real binary and compiler |
| `session.rs` | 69 | session state |
| `ui.rs` | 54 | terminal styling, startup banner |

Implemented: expression evaluation with value printing, statements, automatic
file-scope hoisting for functions/`#include`/`typedef`, multi-line input,
syntax highlighting, compiler detection and mid-session switching, warning
pass-through, diagnostic remapping, crash and infinite-loop isolation,
missing-semicolon repair, tab completion (magics, C keywords, session
names), persistent input history, `%help %quit %reset %history %src %undo
%cc %std`,
process-tree isolation with deadlines on every child (compiler, probes, user
programs), CI for 4 platform configs, tag-triggered release workflow.

---

## 2. Settled decisions

**Drive a real compiler; never interpret.** The tool exists to answer "what
does *my* compiler do with this". Integer promotion, bit-field layout,
evaluation order, UB — every answer worth trusting comes from the user's own
toolchain. An interpreter would give confident answers about the wrong
implementation.

**Classify by trial compilation, not by parsing C.** An input is wrapped as
an expression, a statement, and a file-scope item; whichever compiles is what
it was. The only judge that always agrees with the compiler is the compiler.
Classics like `foo * bar;` (declaration or multiplication, depending on a
typedef) disappear as a problem.

**Accumulate and replay.** Every evaluation rebuilds and reruns the whole
program. Session variables are ordinary locals in `main` — no symbol table,
no separating declarations from initializers. The cost (side effects replay)
is the subject of §3.

**Keep only inputs that may change state.** A bare expression is a question;
it is answered and forgotten. Assignments, `++`/`--` and function calls are
kept. The heuristic (`lex::may_have_side_effects`) is deliberately
conservative: anything it is unsure about is kept.

**The language standard follows the compiler's default; C11 is the hard
floor.** The original design forced `-std=c17` and was reversed: it bought no
purity (gcc accepts GNU extensions under `-std=c17` anyway) and contradicted
the tool's own thesis — a plain `gcc foo.c` gives you gnu23, and the tool must
match it. The default mode is detected by compiling and running a probe that
reports `__STDC_VERSION__` and `__STRICT_ANSI__` (that run doubles as the
works-at-all self-test). When the default mode cannot compile `_Generic`
(MSVC without `/std:` is C89), the std is auto-raised to c17/c11 and the
banner says so; a compiler that cannot reach C11 in any mode is disqualified,
and if no candidate qualifies, startup errors out — limping along with a
compiler that can never print a value reads as "the tool is broken" with no
hint why.

**Diagnostics must be remapped.** The compiler sees a generated file with a
prelude and all earlier inputs above the new text; its line numbers are
meaningless at the prompt. Without remapping the tool is unusable for
beginners — the error text is right but every location it cites is wrong.

---

## 3. The open problem: the state model

### Symptom

Accumulate-and-replay replays side effects. `scanf` prompts the user again on
every later evaluation, file writes repeat, and `%src` is an ever-growing
ledger rather than the current state. (`rand()` is fine: without `srand` it
is deterministic in C.)

Goal: closer to Python — the session holds *current state*, not input
history, and any input executes exactly once.

### Why it is hard — two findings

**Every alternative to replay needs declaration parsing.** A resident-process
design cannot dodge it: a variable declared in step 1 does not exist in step
2's translation unit, so something must know its name and type. Snapshotting
needs the same knowledge to dump variables. Conclusion: `tree-sitter-c` is on
the critical path for anything beyond replay.

**C state cannot be fully serialized.** Scalars, arrays and structs can be
dumped; `malloc`'d graphs and `FILE*` cannot (addresses are meaningless
across processes; a `FILE*` has no serializable representation), and a
snapshottable variable may have been computed through an unsnapshottable one,
which forces either dependency analysis or full replay as a fallback. Python
gets away with it because objects live in a process that never dies — the C
equivalent of Python's model is a resident process, not a snapshot. Snapshot
designs are strictly dominated and are ruled out.

### Survey of existing C REPLs (2026-07)

| Project | Family | Takeaway |
|---|---|---|
| igcc | replay | gcc recompile, output prefix-trimming; same model as us |
| crepl (l-m.dev) | replay | rule buckets + linear undo/redo; **picked tcc because "anything else is too slow"** — proving that making replay fast means giving up the real compiler |
| c-repl (Evan Martin) | resident | one .so per line, dlopen'd into a child; state shared via the dynamic linker |
| cling / clang-repl | resident (in-process) | incremental TU + ORC JIT, transactional rollback; welded to clang |
| evcxr (Rust) | resident | variables in a `HashMap<String, Box<dyn Any>>`, **moved in and out by parameter passing, no linker involved**; crash loses variables |
| picoc / Ch / CINT | interpreter | rejected (fidelity) |

crepl marks the ceiling of the replay family: speed had to be bought by
switching to tcc, i.e. by testing tcc. Our differentiator is "test your own
compiler", so speed has to be bought with architecture instead.

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

- Each step library exports exactly one symbol (`cs_step` — trivial even on
  MSVC) and imports nothing from the host: **zero cross-module linkage**,
  which dissolves the Windows import-library problem that once made this the
  expensive option.
- `&x` is stable across inputs; `malloc`'d memory and `FILE*` survive
  naturally (same runner process); `scanf` runs once; per-input compile cost
  is constant instead of growing with the session.
- **Replay is demoted, not deleted: it becomes the recovery layer.** When the
  runner segfaults, the journal is replayed to rebuild state (stdin recording
  makes `scanf` read the same bytes during recovery). evcxr loses variables
  on crash; the journal does better.
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

- **No introspection commands**: `:layout` (struct offsets/padding),
  `:expand` (preprocessor-only view of a macro), `:asm`, `:type`,
  multi-compiler comparison. These, not the REPL loop, are what would set the
  tool apart — comparison especially: when gcc and clang disagree on the same
  snippet, that is UB or implementation-defined behavior made visible.
- **MSVC status (first CI contact 2026-07):** detection, the C89 default →
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
- **Windows tree-kill uses `taskkill /T`**, which walks the parent-child
  tree and misses orphans whose parent already exited. Job Objects
  (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) are the airtight upgrade if it
  ever matters in practice.
- Probe results are not cached. Startup now runs ~4 compiler invocations
  (banner, version+self-test run, `_Generic` gate, plus `supports_std` when
  needed); adding more probes without an on-disk cache will make cold start
  noticeable.

---

## 5. Traps in the code (read before changing)

**The `M_VAL` marker must be emitted inside the print helpers**
(`codegen.rs`, `RUNTIME`). An argument is fully evaluated before the call, so
the marker lands after all of the expression's own side effects. Emitting it
at the call site instead files `puts("hi")`'s own output under its return
value (`Out[1]: hi\n3`). This bug was introduced once, while removing the
`_n` bindings.

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

**The validator must recognize a signature awaiting its body**
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

**Every child goes through `proc::run_captured`** — compiler, probes, user
programs. It owns three subtleties that must not be unbundled: (1) the child
gets its own Unix process group, and on timeout the *group* is killed —
killing only the child leaves forked descendants running and, worse, holding
the output pipe so the reader threads block forever; (2) when stdin is a
real terminal the child's group must be handed the foreground
(`tcsetpgrp`, both from `pre_exec` and the parent — the double set closes a
race) or any tty read gets the program stopped by SIGTTIN, and SIGTTOU must
be ignored before the parent can take the terminal back; (3) readers drain
into shared buffers and are *abandoned* after a grace period, never joined
unconditionally — a `setsid` escapee can hold the pipe open forever, and
that must cost one thread, not the REPL. The group is only killed on the
timeout path, while the pid is still a zombie: after reaping, the pid can be
recycled and `killpg` could hit an innocent process.

**A crashing or timed-out input must never be committed** (`main.rs`).
Every later evaluation replays the session; committing one crash makes the
prompt permanently unusable.
