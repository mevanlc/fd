# Plan: Windows Path Embetterments (August 2026)

## Goal

Make two narrow Windows path-matching repairs while preserving upstream `fd`'s
CLI and pattern-language behavior:

1. `--exact` accepts `/` and `\` interchangeably as Windows path separators.
2. Full-path `--glob` matching works on native Windows paths by honoring the
   separator normalization already performed by `globset`.

These are compatibility fixes, not a project-wide path-semantics redesign.

## Compatibility Rule

Upstream behavior is the baseline. In particular, retain the existing
`--path-separator <sep>` option, spelling, accepted values, and outward-facing
effects.

`--path-separator` predates this fork and is deliberately more general than a
native-versus-Unix switch. Upstream applies it to printed paths, formatted and
long-listing output, and placeholders passed to `--exec`/`--exec-batch`. This
fork additionally defaults it to `/` when the Windows environment contains a
non-empty `MSYSTEM` value.

Therefore:

- Do not replace it with `--path-separator-native` or
  `--path-separator-unix`.
- Do not restrict it to `/` and `\`; existing uses such as
  `--path-separator=#` and `--path-separator==` remain valid.
- Do not make an arbitrary presentation character part of matching semantics.
- Preserve the `MSYSTEM` default of `/` and the native Windows default of `\`.
- Preserve all non-Windows behavior.

The useful boundary is **path-valued input versus pattern-language source**.
Paths emitted by `fd` should remain usable as filesystem paths, and literal
path matching should recognize both Windows separator spellings. Raw regex,
PCRE2, glob, and bash-expression text is code and still follows the escaping
rules of its language.

## Current Behavior

### Output and placeholders

`filesystem::default_path_separator` returns `/` on Windows when `MSYSTEM` is
set. Otherwise output uses `std::path::MAIN_SEPARATOR`. An explicit
`--path-separator` overrides that output choice. The formatter applies the
chosen separator as paths leave `fd`; filesystem traversal continues to use
native `Path` values.

This is the desired separation and should remain intact.

### `--exact`

`build_pattern_regex` currently compiles an exact pattern as:

```rust
format!("^{}$", regex::escape(pattern))
```

Full-path matching supplies the regex with the native absolute path. On
Windows, a pattern containing `/` is consequently escaped as a literal slash
and cannot match the native `\` in the haystack. This can make an fd-rendered
MSYSTEM path spelling fail when reused as an exact path spelling.

`Opts::uses_full_path_matching` already uses `std::path::is_separator`, so
either `/` or `\` selects path matching for `--exact` under `-p`. The missing
piece is separator equivalence inside the compiled literal pattern.

### `--glob`

On Windows, `globset` treats both `/` and `\` in a glob as separators and
normalizes separator tokens to `/`. With `literal_separator(true)`, its
generated regex also assumes a `/`-normalized candidate.

`fd` currently takes `Glob::regex()`, compiles it as an ordinary byte regex,
and applies it directly to the native full-path haystack. That bypasses
`globset`'s candidate normalization. The existing full-path glob integration
tests are disabled on Windows with a `TODO` for precisely this mismatch.

### Regex, PCRE2, and bash conditional expressions

The default regex and PCRE2 engines match the native path bytes supplied by
the walker. In those grammars, `\` is an escape character; treating every
backslash as a path separator would change valid patterns.

The fork's bash conditional-expression family has its own additional rules:

- unquoted `==`/`!=` right-hand sides use bash glob escaping;
- unquoted `=~` right-hand sides use regex escaping;
- quoted right-hand-side portions are literal; and
- file-test operands are resolved as filesystem paths.

No behavior in these modes changes in this plan.

## Workstream 1: Windows `--exact` Separator Equivalence

### Semantics

For a Windows exact pattern that is being matched against a full path, each
input separator matches either Windows spelling:

```text
C:\work\repo\src/main.rs
C:/work/repo/src/main.rs
C:\work/repo/src/main.rs
```

All three describe the same exact textual path match. Components and separator
count remain exact; this does not turn `--exact` into substring or suffix
matching.

Separator-free exact patterns continue to match only basenames, including
when `-p` came from an alias. Full-path exact matching continues to use the
absolute-path haystack. A relative fd result does not become an absolute exact
pattern merely because the separator spellings are now equivalent.

### Implementation direction

Add a small Windows-only exact-path escaping helper. It should:

1. iterate over the literal input;
2. escape every non-separator run with `regex::escape`; and
3. emit `[/\\]` for every `/` or `\` separator.

For example, either `C:\foo\bar` or `C:/foo/bar` compiles to the equivalent
of:

```text
^C:[/\\]foo[/\\]bar$
```

Use this helper only for `--exact` when the option is in full-path matching
mode. Keep the existing `regex::escape` path everywhere else. Do not rewrite
the haystack and do not pass a rewritten path to filesystem APIs.

This direction is preferable to normalizing all internal paths because it:

- changes only literal exact matching;
- preserves native paths for traversal and every other matcher;
- accepts mixed separator spellings without allocating per candidate; and
- preserves repeated separators and Windows prefixes textually.

### Tests

Add Windows integration coverage proving that:

- absolute exact patterns written entirely with `\` match;
- the same patterns written entirely with `/` match;
- mixed spellings match;
- a changed component, missing component, or extra component does not match;
- separator-free `-p --exact test1` still uses basename matching;
- exact matching remains case-sensitive or insensitive according to the
  existing `-s`/`-i`/smart-case rules; and
- `--path-separator=/`, `--path-separator=\`, and an arbitrary custom output
  separator do not change matching results.

Keep the existing Unix exact tests unchanged.

## Workstream 2: Windows Full-Path Glob Candidate Normalization

### Semantics

Glob matching should follow `globset`'s Windows path rules:

- `/` and `\` in the glob both denote path separators;
- `*` and `?` do not cross a separator under `literal_separator(true)`; and
- `**` retains its recursive-component behavior.

This should work independently of whether fd was launched from cmd.exe,
Windows PowerShell, pwsh, Git Bash, MSYS2, or another frontend. The shell may
still impose its own quoting and argument-conversion rules before fd receives
the glob.

### Implementation direction

Retain the existing `globset` parser and generated regex, but preserve the fact
that a compiled pattern came from glob mode. At the matcher boundary on
Windows, normalize native `\` bytes in a glob candidate to `/` before applying
that generated regex.

Possible internal shapes include a `Pattern::Glob` variant or a matcher-kind
field beside the compiled regex. Prefer the smallest shape that keeps these
properties explicit:

- only glob candidates are normalized;
- default regex and PCRE2 candidates remain byte-for-byte native;
- basename globs take the allocation-free path because they contain no
  separators; and
- all `--and` glob patterns receive the same treatment as the primary glob.

Use a borrowed-or-owned byte buffer so normalization allocates only when a
candidate actually contains `\`. Do not perform string replacement on
`Glob::regex()` itself: `/` appears both as a literal token and inside generated
classes such as `[^/]`, so textual regex surgery is unnecessarily fragile.

Using `GlobMatcher` directly may also solve candidate normalization, but it is
a larger change from fd's current byte-regex pipeline. Prefer the tagged-regex
approach unless direct matching demonstrates a concrete correctness or
non-Unicode-path advantage.

### Matchset consistency

This fork repeats the same `Glob::regex()` plus native-candidate pattern in
matchset `path glob` clauses and in `$<home>`/`$<vroot>` anchored glob tails.
When the shared normalization can be reused without changing literal or regex
clauses, apply it there too. Represent glob matchers explicitly rather than
normalizing every matchset subject.

Name globs require no special work because a Windows basename cannot contain a
path separator.

### Tests

Enable or replace the currently Unix-only full-path glob tests on Windows and
cover:

- `**/one/**/*.foo` over a native Windows tree;
- equivalent patterns written with `/` and `\`;
- `*` refusing to cross a directory boundary;
- `**` crossing directory boundaries;
- primary and `--and` glob patterns;
- `--path-separator` not changing glob results; and
- matchset path and anchored-tail globs if their shared fix is included.

Do not weaken the existing Unix glob suite.

## Explicit Non-Goals

- Renaming, replacing, deprecating, or narrowing `--path-separator`.
- Making `--path-separator` affect traversal or matching.
- Converting every internal Windows path to `/`.
- Making raw fd output valid unescaped regex, PCRE2, glob, or bash source.
- Changing default-regex full-path representation.
- Changing PCRE2 path representation.
- Changing `${}`, `${//}`, or other bash placeholder values or file-test
  resolution.
- Changing `--fixed-strings`; it remains separate from this exact-match fix.
- Changing full-path matching from absolute to search-root-relative.
- Solving Windows verbatim/device paths beyond preserving current behavior.

If demand appears for portable regex-path syntax or different bash placeholder
rendering, design and test that independently rather than smuggling it into a
literal or glob bugfix.

## Validation

### Local gate

```sh
cargo fmt -- --check
cargo check --all-features
cargo nextest run --all-features
git diff --check
```

### Windows gate

Run focused integration tests on both MSVC and GNU Windows builds. The existing
CI matrix exercises x86 Windows integration tests; its ARM Windows job builds
and runs only binary unit tests, so also verify the behavior manually on the
Windows 11 ARM VM.

For Git Bash/MSYS probes, export `MSYS_NO_PATHCONV=1` so the observed argument
spelling reaches fd without MSYS path rewriting. Check both a non-empty
`MSYSTEM` environment and native cmd/PowerShell execution.

The minimum manual matrix is:

| frontend | default output | exact input | glob input |
|---|---|---|---|
| cmd.exe / Windows PowerShell / pwsh | `\` | `/`, `\`, mixed | `/`, `\` |
| Git Bash with `MSYS_NO_PATHCONV=1` | `/` | `/`, `\`, mixed | `/`, `\` |

Record the received command spelling when a shell-layer failure is suspected;
do not attribute shell conversion to fd.

## Documentation Sweep After Implementation

- Add narrow bugfix entries to the Unreleased section of `CHANGELOG.md`.
- Remove or revise the Windows glob `TODO` and the stale Windows note in
  `devdocs/PLAN--PARTIAL-PATH.md`.
- Keep CLI help, completions, and the `--path-separator` documentation
  unchanged unless implementation work discovers a currently false claim.
- Mention the fixes in the fork README only if they remain material fork-only
  behavior; do not expand the README for implementation details.

## Milestones

1. **Exact touch-up** — separator-aware literal regex construction plus focused
   Windows tests.
2. **Glob scratch-and-dent repair** — tagged glob matcher, candidate
   normalization, Windows integration tests, and matchset reuse where narrow.
3. **Windows verification and docs** — MSVC/GNU/ARM probes, full validation,
   changelog, and stale-plan cleanup.

Each milestone should be independently reviewable. Do not hold the exact fix
behind the larger glob matcher refactor.

## Acceptance Criteria

- Windows `-p --exact` accepts `/`, `\`, and mixed separator spellings for the
  same absolute path.
- Windows full-path globs behave according to `globset`'s separator rules.
- `--path-separator <sep>` remains source- and behavior-compatible with
  upstream fd.
- The MSYSTEM output default remains `/`; native Windows output remains `\`.
- Regex, PCRE2, bash conditional expressions, fixed strings, traversal, and
  Unix behavior do not change.
- The complete Rust test suite passes, including all features.

