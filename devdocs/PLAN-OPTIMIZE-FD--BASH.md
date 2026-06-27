# Optimize `fd --bash`

## Goal

Reduce the overhead of `fd --bash` for common path/name predicates while preserving the current bash-condexp semantics and fallback behavior.

Seed observation:

```sh
time fd -g src | sort | md5sum
# e1da2c84abed1c02f6128d25c93a922f  -
# real 0m0.147s
# user 0m0.175s
# sys  0m0.663s

time fd --bash '${/} = src' | sort | md5sum
# e1da2c84abed1c02f6128d25c93a922f  -
# real 0m5.557s
# user 0m13.516s
# sys  0m12.81s
```

Those commands produce the same output hash, but the bash predicate is about 38x slower on wall time in this sample. The first target is to make simple basename conditions like `${/} = src`, `${/} == *.rs`, and `${} =~ ^src/` much closer to native `fd` matching without making full bash-condexp expressions less correct.

This plan covers changes in both repositories:

- `fd`, current repo.
- `../bash-condexp`, the evaluator crate used by `fd`.

Speculative experiments are allowed, including experiments that only pay off after another experiment lands. They must still carry benchmark evidence. If an experiment does not improve the agreed benchmark set, and is not required by a later experiment that does, cull it.

## Progress

2026-06-27:

- Initial plan committed as `11125f1` (`Document fd bash optimization plan`).
- Added `devdocs/bench-fd-bash.sh` to generate the synthetic corpus, verify hashes, and run the benchmark matrix.
- Accepted a first fd-side subset compiler for single-primary `--bash` inclusion predicates whose RHS glob or regex is constant. This precompiles bash-condexp glob/regex matchers once during config construction.
- Culled the first standalone `MapEnv` replacement experiment because it did not produce a measurable standalone improvement before the subset compiler was added.

Synthetic corpus evidence, using `/usr/bin/time -p` fallback because `hyperfine` was not installed:

| Case | Before user+sys | After user+sys | Notes |
| --- | ---: | ---: | --- |
| `native-glob` | 0.12s | 0.12s | baseline |
| `bash-name-eq` | 0.22s | 0.12s | optimized |
| `bash-name-eqeq` | 0.24s | 0.12s | optimized |
| `bash-name-glob` | 0.29s | 0.12s | optimized |
| `bash-path-regex` | 0.47s | 0.12s | optimized |
| `bash-file-test` | 0.13s | 0.14s | still generic |
| `bash-mixed` | 0.29s | 0.29s | still generic |
| `exclude-if-target` | 0.21s | 0.22s | still generic |
| `native-glob-t1` | 0.03s | 0.03s | single-thread baseline |
| `bash-name-eq-t1` | 0.09s | 0.03s | optimized |

The optimized cases also kept the same output hashes as their baseline equivalents.

## Current Hot Path

Relevant `fd` paths:

- `src/main.rs` parses `--bash` expressions once with `bash_cond::parse_expr`.
- `src/walk.rs` evaluates every selected bash expression for each candidate entry.
- `src/bash_cond.rs` builds a fresh `MapEnv` and `ContextFs` for each entry, then calls `bash_condexp::Evaluator`.
- `src/match_sets.rs` can also evaluate bash clauses through the same helper.

Relevant `bash-condexp` paths:

- `../bash-condexp/src/eval.rs` expands words into new `String`s during evaluation.
- `../bash-condexp/src/pattern.rs` compiles RHS glob and regex patterns during `==`, `!=`, and `=~` evaluation.
- `../bash-condexp/src/env.rs` provides `MapEnv`, which is convenient but allocates and hashes for the current `fd` per-entry use case.
- `../bash-condexp/src/fs_abs.rs` calls through `std::fs`/`libc` for file tests.

Likely costs for `fd --bash '${/} = src'`:

- Per-entry placeholder materialization for all fd variables, even when only `${/}` is used.
- Per-entry `HashMap` creation and lookup through `MapEnv`.
- Per-entry string allocation in `Evaluator::expand`.
- Per-entry compile of an anchored glob regex for a literal RHS that could be direct equality.
- Extra path and filesystem resolution scaffolding even when the expression does not use file tests.

## Measurement Rules

Do not accept performance work without measurements.

Use release builds:

```sh
cargo build --release --locked
```

Use the local binary explicitly:

```sh
FD_BIN="$PWD/target/release/fd"
```

End-to-end benchmark shape:

```sh
hyperfine --warmup 3 --runs 10 \
  "$FD_BIN -g src ~ | sort | md5sum" \
  "$FD_BIN --bash '\${/} = src' ~ | sort | md5sum"
```

If `hyperfine` is unavailable, use `/usr/bin/time -p` with at least 5 repeated runs and record median wall/user/sys times.

Correctness check for output equivalence:

```sh
"$FD_BIN" -g src ~ | sort | md5sum
"$FD_BIN" --bash '${/} = src' ~ | sort | md5sum
```

Also keep a benchmark that avoids sorting so fd-side improvements are not hidden by the pipeline:

```sh
hyperfine --warmup 3 --runs 10 \
  "$FD_BIN -g src ~ > /dev/null" \
  "$FD_BIN --bash '\${/} = src' ~ > /dev/null"
```

Run both default thread count and single-threaded variants:

```sh
hyperfine --warmup 3 --runs 10 \
  "$FD_BIN --threads 1 -g src ~ > /dev/null" \
  "$FD_BIN --threads 1 --bash '\${/} = src' ~ > /dev/null"
```

Add a reproducible synthetic corpus before landing larger changes. The corpus should include many files, many directories, repeated `src` basenames, hidden entries, symlinks, and a few sidecar files for file-test expressions. Keep the generator deterministic and cheap enough for local regression runs.

Initial harness:

```sh
devdocs/bench-fd-bash.sh all
```

The harness builds `target/release/fd`, generates a deterministic corpus under `target/fd-bash-bench-corpus`, checks output hashes, and runs the benchmark matrix with `hyperfine` when available or `/usr/bin/time -p` otherwise.

Minimum benchmark matrix:

| Case | Purpose |
| --- | --- |
| `-g src` | native glob baseline |
| `--bash '${/} = src'` | literal basename equality through bash semantics |
| `--bash '${/} == src'` | same as above, alternate operator spelling |
| `--bash '${/} == *.rs'` | precompiled glob candidate |
| `--bash '${} =~ ^src/'` | precompiled regex candidate |
| `--bash '-f ${}'` | file-test / metadata reuse candidate |
| `--bash '${/} == *.rs && -f ${}'` | mixed string + filesystem candidate |
| `--exclude-if '${/} = target'` | pruning/exclusion path still works |
| match-set bash clause | no regression in `src/match_sets.rs` users |

Acceptance gates:

- No semantic regressions in existing tests.
- No measurable regression for ordinary `fd -g`, regex, literal, and no-pattern searches.
- A standalone optimization should improve the targeted bash benchmark by at least 10 percent median wall time, or by at least 10 percent median user+sys time when wall time is noisy.
- A foundational optimization may be kept only if a combined branch demonstrates a measurable end-to-end improvement.
- If a change increases code complexity and does not survive the benchmark gate, drop it.

## Experiment 1: Add Profiling and Benchmark Harness

Hypothesis: the largest cost centers should be visible before rewriting the evaluator.

Work:

- Add a dev-only script or documented command block for building a release binary and running the matrix above.
- Add a deterministic corpus generator under `devdocs/` or `scripts/` if the repo accepts such scripts.
- Profile the seed case with a native sampler on macOS, for example `samply`, Instruments, or `cargo flamegraph` if available.
- Capture top frames for both wall time and CPU time.

Expected evidence:

- Baseline table for current `main`.
- Flamegraph or sampled stack summary showing how much time lands in `bash_cond::evaluate`, `MapEnv`, `pattern::compile_glob`, `regex` compilation, `std::fs`, and path/string conversions.

Cull rule:

- Do not proceed with broad rewrites until this confirms the hot path. If profiling contradicts the hypotheses, update this plan first.

## Experiment 2: Replace Per-Entry `MapEnv` in `fd`

Hypothesis: a purpose-built fd entry environment will remove avoidable allocations and hash lookups for placeholder variables.

Work in `fd`:

- Add an `FdEntryEnv` implementing `bash_condexp::Env`.
- Store borrowed entry context plus lazy `OnceCell<String>` values for only the placeholders requested by the expression:
  - `${}` path
  - `${/}` basename
  - `${//}` parent
  - `${.}` path without extension
  - `${/.}` basename without extension
- Return `nocasematch` from a direct bool instead of inserting an option into a map.
- Add a small static dependency analysis over `bash_condexp::Expr` so `fd` can know which placeholders are needed. If that proves too invasive, let the lazy cells pay for themselves and avoid up-front analysis in the first version.

Potential `bash-condexp` change:

- If `Env::var(&self) -> Option<&str>` makes lazy borrowed values awkward, add a separate helper type in `fd` first. Do not change the trait unless profiling shows the trait shape is blocking the optimization.

Correctness checks:

- Existing `test_bash_search`.
- Placeholder-specific tests for all five fd variables.
- Case-insensitive `nocasematch` behavior.

Expected payoff:

- Large drop in allocation count for simple `${/}` expressions.
- Reduced user CPU time on all bash cases.

Cull rule:

- Drop if allocation reduction is visible but end-to-end bash benchmarks do not improve after profiling confirms allocation was not material.

## Experiment 3: Precompile Constant RHS Glob and Regex Patterns

Hypothesis: repeated compilation inside `bash-condexp` is a major cost for `==`, `!=`, and `=~`, especially for expressions whose RHS does not depend on per-entry variables.

Work in `../bash-condexp`:

- Add a compiled representation, for example `CompiledExpr`, separate from the parsed `Expr`.
- During compilation, classify binary primaries:
  - RHS has no variable parts and no case-mode dependence beyond the fixed `nocasematch` value: compile once.
  - RHS contains variables or otherwise depends on the environment: keep dynamic evaluation.
- For glob patterns, detect literal patterns with no glob metacharacters and store an equality matcher instead of compiling a regex.
- For regex patterns, compile once when the pattern is constant.
- Preserve current quoting rules.

Work in `fd`:

- Parse `--bash`, `--prune-if`, `--exclude-if`, and match-set bash clauses into the compiled representation when available.
- Keep a fallback path for uncompiled/dynamic forms.

Correctness checks:

- `bash-condexp` unit tests for quoted literal glob metacharacters.
- `bash-condexp` tests where RHS uses variables and must remain dynamic.
- `fd` integration tests for `${/} = src`, `${/} == *.foo`, and `${} =~ ^one/`.

Expected payoff:

- `--bash '${/} = src'` should avoid per-entry regex compilation entirely.
- `--bash '${/} == *.rs'` and regex cases should pay compile cost once per expression, not once per entry.

Cull rule:

- If precompilation complicates `bash-condexp` but does not dominate the benchmark, prefer a narrower fd-specific fast path.

## Experiment 4: Add an `fd` Predicate Fast Path

Hypothesis: many useful `--bash` expressions can be compiled into an fd-native predicate without invoking the generic evaluator for every entry.

Work in `fd`:

- Add a translator from `bash_condexp::Expr` to an internal predicate enum for a safe subset:
  - `&&`, `||`, and `!`.
  - Binary string tests where the LHS is one fd placeholder and RHS is constant.
  - Literal equality and inequality.
  - Constant glob match and not-match.
  - Constant regex match.
  - Unary file tests that map directly to `DirEntry` metadata or file type for `${}`.
- If any node is unsupported, fall back to the generic evaluator for the whole expression or for that subtree.
- Keep the translator conservative. Unsupported should mean slower, not different.

Candidate internal shape:

```rust
enum BashPredicate {
    And(Box<BashPredicate>, Box<BashPredicate>),
    Or(Box<BashPredicate>, Box<BashPredicate>),
    Not(Box<BashPredicate>),
    NameEq(String),
    NameGlob(globset::GlobMatcher),
    PathRegex(regex::bytes::Regex),
    FileType(FileTypePredicate),
    Generic(bash_condexp::Expr),
}
```

This is intentionally fd-specific. It should optimize fd's placeholder conventions rather than become a second full bash-condexp evaluator.

Correctness checks:

- Differential tests: evaluate random or table-driven supported expressions with both fast predicate and generic evaluator on the same fixture entries.
- Tests for fallback when RHS has variables, arithmetic, `-v`, `-o`, `-t`, file comparisons, or unsupported shell features.

Expected payoff:

- Best-case simple basename equality should approach the cost of existing glob/literal matching plus walker overhead.
- Mixed expressions should short-circuit cheaply before invoking generic file tests or dynamic expressions.

Cull rule:

- Drop or narrow this if `bash-condexp` compiled expressions deliver similar speed with less duplicated semantics.

## Experiment 5: Reuse `DirEntry` Metadata for File Tests

Hypothesis: file-test primaries such as `-f ${}`, `-d ${}`, `-e ${}`, `-s ${}`, and `-x ${}` should not restat the current entry when the walker already has file type or cached metadata.

Work in `fd`:

- Add a `FileSystem` implementation for bash evaluation that can answer tests against the current entry from `DirEntry`.
- Resolve `${}` and the current entry path to the cached/current entry fast path.
- Use `DirEntry::file_type()` for type checks when sufficient.
- Use `DirEntry::metadata()` for size, mode, owner, and time checks when metadata is required.
- Fall back to `StdFs` for sidecar paths like `CACHEDIR.TAG`, absolute paths, parent-relative paths, and file comparisons involving another path.

Potential `bash-condexp` change:

- If the `FileSystem` trait does not expose enough context to avoid repeated conversion, add an optional current-entry wrapper in `fd` first. Only expand the trait if it becomes generally useful.

Correctness checks:

- `-f ${}`, `-d ${}`, `-e ${}`, `-s ${}`, `-x ${}` on regular files, dirs, symlinks, and broken symlinks.
- Sidecar checks like `-f CACHEDIR.TAG` still resolve relative to `context_dir`.
- `--prune-if` and `--exclude-if` context rules remain unchanged.

Expected payoff:

- Lower sys time for file-test-heavy expressions.
- Better combined performance for match-set bash clauses that mix type/name/file sidecar checks.

Cull rule:

- Keep only if file-test benchmarks improve and no symlink/context behavior changes.

## Experiment 6: Expression Dependency Analysis

Hypothesis: knowing what an expression can touch lets `fd` avoid path work, filesystem scaffolding, and some filter ordering costs.

Work:

- Add a small analysis pass over parsed/compiled expressions:
  - placeholder variables used;
  - whether file tests are present;
  - whether RHS glob/regex compilation depends on variables;
  - whether expression can be evaluated from basename only;
  - whether expression can be evaluated from path only;
  - whether expression can prune directories safely.
- Use the analysis to choose the cheapest evaluator path:
  - fd-native predicate;
  - compiled bash-condexp;
  - generic bash-condexp.

Potential future optimization:

- For expressions proven to be basename-only and constant, consider feeding equivalent patterns into existing native pattern machinery. This should happen only if the translation is exact enough for fd's bash placeholder semantics.

Correctness checks:

- Analysis should be advisory. If uncertain, mark as dynamic/generic.
- Unit tests for every classification.

Expected payoff:

- Prevents unnecessary placeholder/path/metadata setup.
- Helps later experiments compose cleanly.

Cull rule:

- Keep only if it enables accepted experiments or has a direct benchmark payoff.

## Experiment 7: Filter Ordering and Short-Circuiting

Hypothesis: once bash predicates have cheap and expensive components, ordering them by cost can reduce per-entry work.

Work:

- Inside a compiled predicate, evaluate cheap name/path checks before metadata/file-system checks where short-circuit semantics permit it.
- Preserve user-visible short-circuit behavior only where bash-condexp could expose side effects through `BASH_REMATCH`. Since `fd` does not surface `BASH_REMATCH`, fast paths may have room to reorder pure predicates, but be conservative.
- Do not reorder generic evaluator subtrees unless `bash-condexp` explicitly supports a pure compiled form.

Correctness checks:

- Ensure expressions that currently error still error when they should.
- Ensure regex match behavior remains equivalent for accepted/fallback paths.

Expected payoff:

- Better performance for compound filters such as `${/} == *.rs && -f ${}`.

Cull rule:

- Drop if changes are too hard to reason about or benchmark gains are lost in noise.

## Experiment 8: Match-Set Bash Clause Integration

Hypothesis: any accepted `--bash` optimization should also benefit bash clauses in `match-sets.kdl`.

Work in `fd`:

- Store the same optimized/compiled predicate type for `Matcher::Bash`.
- Ensure include and exclude match sets use the same context rules as `--bash`, `--exclude-if`, and `--prune-if`.
- Benchmark a match set with a bash clause equivalent to the seed expression.

Correctness checks:

- Existing match-set tests.
- Add one test proving a bash match-set clause uses optimized behavior without changing output.

Expected payoff:

- Avoids two divergent bash execution paths.
- Makes match-set performance acceptable for config-heavy workflows.

Cull rule:

- If match-set integration introduces complexity before the core `--bash` path is proven, defer it. Do not leave a second slow path permanently if the core optimization lands.

## Cross-Repo Staging

Preferred sequence:

1. Benchmark current `fd` and current crates.io `bash-condexp` dependency.
2. Add any pure `fd` fast path that does not require a `bash-condexp` API change.
3. Prototype `bash-condexp` compiled expressions in `../bash-condexp`.
4. Temporarily wire `fd` to the sibling crate with a local path dependency or `[patch.crates-io]` on an experiment branch.
5. Benchmark the combined branch.
6. If the combined branch wins, publish or otherwise version `bash-condexp` deliberately, then update `fd` back to a normal versioned dependency.

Do not leave `fd` accidentally depending on an unpublished sibling path unless that is an explicit project decision.

## Validation

For `fd`:

```sh
cargo fmt
cargo check
cargo test test_bash_search
cargo test test_bash_search_empty_path_file_test
cargo test test_match_sets_include_entries
cargo test test_match_sets_exclude_entries_and_prune_dirs
cargo test
```

For `../bash-condexp`:

```sh
cargo fmt
cargo check
cargo test
cargo test --features bash-conformance
```

For benchmarks:

- Record machine, date, git commit, command, median wall time, median user+sys time, and output hash.
- Include both before and after numbers.
- Include at least one run with warm filesystem cache.
- For filesystem-heavy tests, include one run after clearing or perturbing cache only if it can be done consistently.

## Success Criteria

Near-term:

- A simple basename equality expression like `fd --bash '${/} = src'` should improve by at least 3x from the current baseline on the user's home-tree benchmark.
- No ordinary `fd -g src` regression beyond measurement noise.
- Existing bash semantics remain covered by fallback and tests.

Stretch:

- Simple basename equality should get within 2x to 5x of `fd -g src` on the same output set.
- Constant glob and regex bash expressions should pay compile cost once, not once per candidate entry.
- File-test expressions against the current entry should avoid redundant `stat` calls.

Non-goals for this pass:

- Implement full bash.
- Change the documented meaning of fd placeholders.
- Optimize every bash-condexp feature before the common fd predicate cases are fast.
- Land speculative rewrites without benchmark proof.
