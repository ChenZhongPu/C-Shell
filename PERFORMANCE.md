# Evaluation Performance

Every ordinary query still compiles and runs the complete retained C program.
The optimization caches a compiler-confirmed printing strategy, never an
executable, expression result, or initialized C state. No new shell commands
are needed.

## Real-compiler measurement

Measured on Linux with `cc (GCC) 16.2.1 20260810`, using the Rust debug test
profile. The baseline is v0.3.2 (`487c88d`) with the same measurement harness,
before the strategy cache. Session setup is outside the measured interval.
Each session contains an integer, a pointer and a 2×2 integer array, plus
0/10/100/1000 retained declarations. Each expression is evaluated three
times in one evaluator; the table gives the median wall time, including
source generation, all compilation attempts, process startup and replay.
The first sample is cold; the next two can reuse the strategy. Compiler
startup capability probes are outside the interval.

| Retained extra declarations | Expression | Before (ms) | After (ms) |
|---:|---|---:|---:|
| 0 | `value` | 68.65 | 62.24 |
| 0 | `pointer` | 120.37 | 58.58 |
| 0 | `matrix` | 167.57 | 62.03 |
| 10 | `value` | 68.65 | 67.90 |
| 10 | `pointer` | 136.03 | 52.47 |
| 10 | `matrix` | 182.85 | 68.92 |
| 100 | `value` | 65.95 | 63.38 |
| 100 | `pointer` | 129.49 | 64.89 |
| 100 | `matrix` | 169.77 | 62.97 |
| 1000 | `value` | 70.19 | 68.13 |
| 1000 | `pointer` | 142.81 | 78.81 |
| 1000 | `matrix` | 219.05 | 64.25 |

Wall times vary with the host, toolchain and load; three samples are a small
measurement, not a latency guarantee. Compile counts provide the stable
comparison at every history size:

| Expression | Before, every query | After, first query | After, repeated query |
|---|---:|---:|---:|
| Integer | 1 | 1 | 1 |
| Pointer | 2 | 2 | 1 |
| 2D array | 3 | 3 | 1 |

Run the opt-in benchmark with:

```sh
cargo test --bin c-shell benchmark_evaluation_strategies -- --ignored --nocapture
```

`CC` selects the compiler. The benchmark prints its version, cold/warm
compile counts and median times. To repeat the historical comparison, apply
only the ignored measurement test and `compile_count` instrumentation to the
baseline; adapt its session setup to that revision's public fields.

## Cache boundaries

- Any retained mutation, local shadowing, file-item replacement, reset or
  attached stdin event changes the session revision. Divergent session clones
  have distinct revisions. Compiler/standard changes clear strategies too.
- The strategy cache holds at most 256 expressions for a revision; timed
  and untimed wrappers have separate keys. A compile failure evicts the
  strategy and retries ordinary classification. Runtime failures are returned
  immediately without retrying execution.
- Explicit includes bypass strategy reuse because external headers can
  change independently. Explicit UTF-8 previews bypass reuse because their
  validity depends on current bytes.
- Identifier and macro reference counts update from changed inputs rather
  than rescanning all history. Completion refreshes after session mutation;
  retained-function purity analysis is reused until file-scope code changes.
- Highlighting retains lexical state across physical lines. Cursor movement
  reparses no lines; an edit reparses only the suffix starting at the first
  changed line.

## Regression coverage

`tests/smoke.rs` counts real compiler invocations and checks invalidation
after shadowing and a standard change. Editor tests verify block-comment
context, zero reparsing on cursor movement and suffix-only reparsing after
an edit. Session tests compare incremental indexes with a fresh scan after
replacement/shadowing and check clone/reset invalidation.

Other end-to-end tests escape value previews without rewriting explicit
output and mix `scanf`/character/line reads with buffering, pushback and EOF.
File-stream reads remain native. A Web PTY test exercises Ctrl-C during editing,
compilation, execution and pending input, followed by continued evaluation,
CRLF terminal input and compiler-descendant cleanup.

CI runs real compilers on Linux (GCC and Clang), macOS (Clang) and Windows
(WinLibs GCC). Windows execution is verified by that native CI job, without
local Windows cross-compilation.
