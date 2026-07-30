app-about = An interactive prompt for C
arg-cc = C compiler to use (default: $CC, then the first one found on PATH)
arg-std = Language standard, e.g. c11, c17, c23 (default: compiler default)
arg-eval = Evaluate CODE and exit (repeatable); failures set a nonzero exit code
arg-script = Run inputs from FILE and exit; failures set a nonzero exit code
arg-quiet = Suppress the banner and hints
arg-timeout = Seconds each compilation or program run may take (default: 10)
arg-no-color = Disable colored output
arg-lang = UI language; overrides automatic locale detection (values: en, zh)
arg-web = Start a browser terminal available only on this machine
arg-no-open = Do not open the browser automatically (requires --web)
arg-help = Print help
arg-version = Print version
cli-error = error:
cli-argument = command-line argument
cli-invalid-value = invalid value "{$value}" for {$argument}
cli-unknown-argument = unrecognized argument {$argument}
cli-equals-required = {$argument} requires an equals sign before its value
cli-wrong-value-count = {$argument} has the wrong number of values
cli-missing-value = {$argument} requires a value
cli-argument-conflict = {$argument} cannot be used together with {$prior}
cli-invalid-utf8 = a command-line argument is not valid UTF-8
cli-invalid-arguments = invalid command-line arguments
cli-valid-values = valid values: {$values}
cli-suggestion = a similar argument exists: {$suggestion}
cli-usage = Usage:
cli-options = Options:
cli-more-info = For more information, try "--help".
script-open-error = cannot open script {$path}
startup-hint = Type C code to evaluate it · %help for commands · Ctrl-D to exit
web-launch-hint = Tip: use `c-shell --web` to launch the browser UI
bye = bye
web-listening = Browser terminal: {$url}
web-stop-hint = Available only on this machine · press Ctrl-C to stop
web-open-failed = could not open the default browser; open the URL above manually
web-connecting = Connecting…
web-disconnected = Disconnected
web-session-failed = c-shell could not start: {$error}

edit-interactive-only = %edit is available only in interactive REPL mode
external-side-effect-warning = warning: external side-effect call detected ({$calls}); retained inputs are replayed before every later evaluation, so input operations and file or process effects may happen repeatedly
note-missing-semicolon = (missing semicolon added automatically)
unprintable-value = valid expression, but this value category has no printer; evaluated without Out[n]
note-input-not-kept = (input not kept in the session)
note-replaced-file = (replaced previous file-scope definition)
note-added-file = (added at file scope)
note-shadowed = (opened a nested scope to shadow an earlier declaration)
note-stdin-captured = (captured {$count} stdin request(s) for replay; contents hidden)

edit-usage = usage: %edit [input-number]
edit-not-found = no C input In[{$number}]
nothing-to-edit = nothing to edit
unicode-missing-expression = missing expression
unicode-invalid-count = -n requires a non-negative code-unit count
unicode-count-limit = -n is limited to {$limit} code units
unicode-missing-after-count = missing expression after -n <count>

where-headers = headers:
where-header-column = header
where-doc-column = documentation
where-not-found = {$name} was not found in c-shell's ISO C standard library index
where-name = name:
where-kind = kind:
where-signature = signature:
where-availability-range = ISO C availability: {$since}–{$last}; removed in {$removed}
where-availability-later = ISO C availability: {$since} and later
where-selected-mode = selected mode:
where-available = available
where-unavailable = not available as an ISO C library identifier
where-auto-no = auto-included by c-shell: no
where-auto-yes = auto-included by c-shell: yes
where-auto-conditional = auto-included by c-shell when available:
where-note = note:
kind-function = function
kind-function-like-macro = function-like macro
kind-object-like-macro = object-like macro
kind-type-generic-macro = type-generic macro
kind-typedef = typedef
kind-type = type
index-note-gets = deprecated; removed from ISO C in C11 because it cannot perform bounded input
index-note-stdbool = provided by <stdbool.h> through C17; a language keyword in C23
index-note-stdalign = provided by <stdalign.h> through C17; a language keyword in C23
index-note-assert = provided by <assert.h> in C11/C17; a language keyword in C23
index-note-obsolescent = obsolescent in C23
index-note-noreturn = still provided by <stdnoreturn.h> in C23; prefer [[noreturn]] in new C23 code
index-note-macro-form = an implementation may additionally provide a macro form

magic-help =
    Commands:
      %help [--verbose]  show commands; --verbose adds usage notes
      %quit / %exit      quit (Ctrl-D works too)
      %clear             clear the screen without changing the session
      %reset             clear the session and start fresh
      %src [--raw]       show user C; --raw includes generated runtime/protocol
      %header            list the headers included in every program
      %edit [n]          copy latest or In[n] into the prompt for editing
      %type <expression> query an expression's type without evaluating it
      %bits <expression> inspect a scalar value using lowercase hexadecimal
      %Bits <expression> same as %bits, with uppercase hexadecimal
      %utf8/%utf16/%utf32 [-n N] <expression> decode Unicode code units
      %where <identifier> find an ISO C library identifier's standard header
      %time <code...>    time the execution of a statement or expression once
      %timeit <code...>  benchmark a statement or expression over multiple loops
      %cc [path]         show or switch the C compiler
      %std [std]         show or switch the language standard (c11/c17/c23);
                         %std default returns to the compiler's own default

magic-help-notes =
    Notes:
      A bare expression prints its value; a trailing ';' runs it silently.
      A completed if waits for a blank continuation line; type else / else if
      there instead to continue it. Other closed blocks submit immediately, but
      a struct/union/enum definition waits for its mandatory trailing ';'.
      Function definitions, #include and typedef go to file scope automatically.
      %edit n can reopen any C In[n] from this session, including a failed one;
      it only fills the next prompt. Modify it and press Enter to submit it under
      a new number; the original numbered input remains unchanged.
      c-shell supplies main(); enter its body as statements and omit final return.
      Redeclaring a local opens a nested shadowing scope. Redefining a function
      or type replaces the prior file-scope input only if the compiler accepts
      the complete rewritten session; functions are never demoted into main.
      %type uses _Generic matching: scalar types and scalar pointers are named;
      complete named structs/unions report e.g. Struct Point or Union Value;
      simple anonymous typedefs use the typedef name. Other aliases and top-level
      qualifiers are canonicalized, and arrays/functions undergo their normal
      expression conversions.
      %bits/%Bits evaluate a scalar expression once, then show its type, value,
      size, hexadecimal and binary representation, memory bytes and byte order.
      Command spelling is case-sensitive; only %Bits selects uppercase hex.
      IEEE-754 float/double values also show sign, exponent and fraction fields;
      aggregates, arrays and function pointers are not supported.
      %utf8/%utf16/%utf32 read integer arrays or pointers as Unicode code units.
      By default they stop at NUL or 100 units; -n N reads exactly N units, up to
      4096. Invalid Unicode is reported without replacement characters. Pointer
      reads are explicit but still require the addressed memory to be valid.
      %where uses a built-in ISO C89-C23 index, not the host's transitive header
      visibility. It also links each matching header to cppreference. POSIX,
      platform/compiler extensions and user names are excluded.
      %time evaluates an expression or statement once, displays its output/value,
      and times only that input inside C; compilation, process startup and retained
      session replay are excluded. Side effects are retained in session.
      %timeit benchmarks an expression or statement in an auto-ranged loop over
      multiple runs without modifying the session state. Inputs that may change
      state warn because they execute repeatedly.
      Struct values use designated members; nested known structs and arrays
      expand, but pointer members are shown only as addresses or NULL. Use an
      explicit member expression (p.name) or dereference (*ptr) to drill down.
      Arrays print their elements, one or two dimensions deep, bounded at 100 per
      dimension; a real pointer still prints as an address, and elements with no
      printer of their own show <unprintable>.
      Pure bare expressions (x + 1, sizeof(int)) are evaluated and forgotten.
      Statements and bare expressions that may have effects are kept.
      Direct scanf calls record one private input line per dynamic request; later
      replay uses that tape, including calls in functions and loops. Other known
      file/input/process APIs warn because their external effects may repeat.

help-usage = usage: %help [--verbose]
session-cleared = session cleared
headers-intro = Automatically included (optional headers are guarded):
src-usage = usage: %src [--raw]
type-usage = usage: %type <expression>
type-no-result = type query produced no result
bits-usage = usage: %{$command} <expression>
bits-unsupported = %{$command} supports standard scalar values and pointers to scalar types
bits-no-result = bits query produced no result
bits-type = type:
bits-size = size:
bits-byte = byte
bits-bytes = bytes
bits-value = value:
bits-hex = hex:
bits-binary = binary:
bits-memory = memory:
bits-byte-order = byte order:
bits-little-endian = little-endian
bits-big-endian = big-endian
bits-sign = sign:
bits-exponent = exponent:
bits-fraction = fraction:
bits-zero-subnormal = zero/subnormal
bits-infinity-nan = infinity/NaN
unicode-usage = usage: %{$command} [-n code-units] <expression> ({$message})
unicode-unsupported = %{$command} supports pointers and arrays of integer code-unit types
unicode-no-result = Unicode query produced no result
where-usage = usage: %where <identifier>
time-usage = usage: %time <expression or statement>
wall-time = Wall time:
wall-time-unavailable = Wall time: unavailable (input did not complete)
timeit-usage = usage: %timeit <expression or statement>
timeit-state-warning = "%timeit input" may execute repeatedly and is not retained for replay; later evaluations do not include its C state changes
std-unsupported = this compiler does not support -std={$standard}
unknown-command = unknown command %{$command} — try %help

temp-source-write-error = cannot write temporary source: {$error}
temp-dir-error = failed to create temporary directory
compiler-run-error = cannot run compiler: {$error}
compiler-output-truncated = compiler output exceeded {$mib} MiB per stream and was truncated
compiler-timeout = compiler timed out after {$seconds}s and was killed
main-already-provided = c-shell already provides main(); enter the statements from its body directly and omit the final return
program-start-error = failed to start {$path}
program-killed = killed after {$seconds}s (possible infinite loop)
program-output-truncated = program output exceeded {$mib} MiB per stream and was truncated
stdin-tape-diverged = stdin tape diverged while replaying retained input; use %reset
program-exited-early = program exited before the input completed
timeit-run = run
timeit-runs = runs
timeit-loop = loop
timeit-loops = loops
timeit-report = {$mean} ± {$deviation} per loop (mean ± std. dev. of {$runs} {$run-word}, {$loops} {$loop-word} each)

unicode-code-units = code units:
unicode-encoding = encoding:
unicode-address = address:
unicode-width-error = error: expected {$expected}-byte code units, but the expression points to {$actual}-byte elements
unicode-text = text:
unicode-text-prefix = text prefix:
unicode-no-nul = note: no NUL terminator in the first {$limit} code units
unicode-invalid-utf8-unit = invalid UTF-8 code unit at index {$index}
unicode-invalid-utf8-sequence = invalid UTF-8 sequence at code unit {$index}
unicode-invalid-utf32 = invalid UTF-32 scalar value at index {$index}
unicode-invalid-utf16-unit = invalid UTF-16 code unit at index {$index}
unicode-unpaired-high = unpaired UTF-16 high surrogate at index {$index}
unicode-unpaired-low = unpaired UTF-16 low surrogate at index {$index}
array-more = ... ({$count} more)

signal-segv = program crashed: segmentation fault (SIGSEGV) — usually a NULL/wild pointer dereference or an out-of-bounds index
signal-abrt = program aborted (SIGABRT) — usually a failed assert, or heap corruption caught by the C library
signal-fpe = arithmetic error (SIGFPE) — usually integer division by zero
signal-ill = illegal instruction (SIGILL)
signal-bus = bus error (SIGBUS) — usually a misaligned memory access
signal-other = program terminated by signal {$signal}
windows-access-violation = program crashed: access violation — usually a NULL/wild pointer dereference or an out-of-bounds index
windows-division-zero = arithmetic error: integer division by zero
windows-illegal-instruction = illegal instruction
windows-stack-overflow = stack overflow — usually runaway recursion or a huge stack array
windows-buffer-overrun = stack buffer overrun detected

compiler-cannot-build = {$path} (cannot build a working program)
compiler-standard-unsupported = {$path} (does not support requested standard {$standard})
compiler-printer-unsupported = {$path} (selected mode cannot compile the value printer)
compiler-not-found =
    no usable C compiler found (tried: {$tried}).
    c-shell requires a mode capable of its C11-style value printer.
    Install gcc or clang, or point c-shell at one with --cc <path>.{$windows-note}
compiler-windows-note =
    On Windows, MSVC (cl.exe) only works from a Developer Command Prompt.
compiler-default-std = default std
compiler-default-std-value = default std {$standard}
compiler-auto-std = -std={$selected}, auto-raised: default {$default} lacks _Generic
