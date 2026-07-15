# Plan: `--partial-path`

## Goal

Add `--partial-path`: like `--full-path`, the pattern is matched against the
full path instead of just the filename — but a hit only requires the pattern
to match a *portion* of that path, not the whole thing.

## Motivation

`--full-path` matches against the entire absolute path, and fd's two anchored
pattern modes anchor over that entire haystack. Both become footguns the
moment `-p` is baked into an alias (see `F-AS-ALIAS.md`):

```sh
alias f='fd -H -I -i -p -M vcs_meta,package,noise'

f --exact .git      # nothing: ^\.git$ must equal /Users/me/repo/.git
f -g '*.lock'       # nothing: the glob must span the whole absolute path
                    # ('*' cannot cross '/'; you must write '**/*.lock')
fd src/main.rs      # error: "pattern contains a path-separation character";
                    # the suggested fix (--full-path) only works for the
                    # regex mode, not for --exact or --glob
```

Both were verified against the current build (2026-07-14). `--partial-path`
is the fix: one flag that means "I'm giving you part of a path — find entries
whose path contains it".

## The Invariant

Unchanged from `PLAN-F-TO-FD.md`: the five base cases (`fd`, `fd .`,
`fd -g '*'`, `fd . dir`, `fd -g '*' dir`) stay byte-identical to upstream.
None of them uses a path-scope flag, so the default code path (filename
haystack, unmodified anchoring) must be untouched. The existing
`test_invariant_base_case_goldens` covers this.

## Semantics

### One knob, three settings

Path-match **scope** is a single tri-state, set by whichever scope flag
appears last on the command line (clap `overrides_with` last-wins, same as
the other boolean pairs):

| scope | flag | haystack | anchored modes (`--glob`, `--exact`) |
|---|---|---|---|
| filename (default) | — | basename | must span the whole basename |
| full | `-p`, `--full-path` | absolute path | must span the whole path |
| partial | `--partial-path` | absolute path | must span a **component-boundary-anchored suffix** of the path |

`--no-full-path` / `--no-partial-path` (hidden, aliases of one another) reset
the knob to filename scope, whichever of `-p`/`--partial-path` set it. With a
single knob there is no meaningful "undo only the partial part" — both `no-`
spellings mean "back to basenames".

### Per pattern mode

| pattern mode | full scope (today) | partial scope (new) |
|---|---|---|
| regex (default) | unanchored substring of the abs path | **identical** — regex users write their own anchors |
| `--fixed-strings` | literal substring of the abs path | **identical** |
| `--exact` | `^lit$` over the whole abs path | `(?:^|/)lit$` — pattern names the entry by its trailing path components |
| `--glob` | glob spans the whole abs path | glob is compiled as `**/pattern` — it spans the trailing components |
| `--bash` | n/a (own placeholder vars) | n/a — scope flags don't change `--bash`; reject or ignore identically to `--full-path` today |

So partial scope only *behaves* differently from full scope in the two
anchored modes; for plain regex and `-F` it is a synonym. That's fine — the
point of the flag is that it composes safely with every mode, so an alias can
bake it without knowing which mode a given invocation will use.

Examples (partial scope):

```sh
fd --partial-path src/main.rs           # regex: substring, like -p today
fd --partial-path --exact .git          # .git itself; NOT .gitignore, NOT .git/config
fd --partial-path --exact src/main.rs   # entries whose path ends with these components
fd --partial-path -g '*.lock'           # any *.lock at any depth
fd --partial-path -g '.git/*'           # direct children of any .git dir
fd --partial-path -g '.git/**'          # everything under any .git dir
```

### Decision: suffix-anchored, not component-run, not raw substring

The load-bearing choice for the anchored modes. Three candidates for what
"match a portion" means for `--exact`/`--glob`:

1. **Raw substring** — `.git` also hits `.gitignore` and
   `foo.github.io`. Defeats the entire point of `--exact` ("literal,
   non-substring"). Rejected.
2. **Component-run** (gitignore-flavored) — `(?:^|/)pat(?:/|$)`: the pattern
   matches any run of *whole* components, so `--exact .git` hits `.git` *and
   every entry inside it*. Rejected because it removes the ability to name
   just the entry — which is `--exact`'s whole job — and everything it can
   express is already expressible: regex users write `(^|/)\.git(/|$)`
   themselves, glob users write `.git/**` alongside `.git`.
3. **Suffix at a component boundary** — `(?:^|/)pat$`: the pattern names an
   entry by its trailing path components. `--exact .git` → `.git` only.
   Globs keep full expressiveness (`**` opts into depth explicitly).
   **Chosen.**

This also matches fd's philosophy that the pattern identifies the entry, not
its subtree (`fd . dir` is the idiom for subtrees, per fd's own error text).

### Decision: haystack stays the absolute path

Same haystack as `--full-path`, for consistency and least surprise (the name
says "like --full-path, but…"). This inherits full-path's known wart: with a
substring regex, components *above* the search root can match
(`fd --partial-path mclark` under `/Users/mclark` matches everything). The
anchored modes don't suffer from it (a suffix match can only extend above the
search root if the pattern itself spans it). A search-root-relative haystack
was considered and parked — it would make partial regex differ from `-p`
regex, and it's severable: nothing here forecloses a future
`--relative-to-root` style option. Revisit only if the wart bites in
practice.

## CLI surface

```text
    --partial-path         Match pattern against any part of the full path
    --no-partial-path      (hidden) reset to filename matching; alias --no-full-path
```

- Long-only for now. `-P` is free (checked 2026-07-14) and mnemonic, but the
  alias bakes the flag so per-invocation typing should be rare; a short can
  be added later without breakage. (Old `f` used `-P` for prune/exclude-if —
  a reason to hesitate before reusing the letter.)
- Override matrix (clap, mutual last-wins):
  - `full_path`: add `overrides_with = "partial_path"`.
  - `partial_path: bool` (new, visible): `overrides_with = "full_path"`.
  - `no_full_path: ()` (exists, hidden): becomes
    `overrides_with_all(["full_path", "partial_path"])` and gains
    `alias = "no-partial-path"`.
- `--partial-path` + `--glob`/`--exact`/`--fixed-strings`/`--and`: all
  compose (that's the point). `--bash`: same treatment as `--full-path`
  today (the bash expression has its own path/basename variables; scope
  flags are irrelevant to it — no new conflict rule).

## Implementation

Small; `walk.rs` is untouched because anchoring lives entirely in the
compiled pattern regex and the haystack machinery is shared with full scope.

### `src/cli.rs`

- New `partial_path: bool` field next to `full_path` (visible in `-h`,
  long help explains the suffix semantics + the alias use case).
- Override wiring per the matrix above.
- Optional clarity helper: `enum PathMatchScope { FileName, Full, Partial }`
  + `Opts::path_match_scope()`, so `main.rs` reads as a three-way scope
  instead of two booleans. (Config struct does not need it — see below.)

### `src/main.rs`

- `ensure_search_pattern_is_not_a_path` (line ~230): early-return for
  partial scope too (a path separator in the pattern is now *expected*).
- `build_pattern_regex` (line ~279) learns the scope:
  - glob branch: under partial scope compile `format!("**/{pattern}")`
    instead of `pattern`. `globset` treats a leading `**/` as "zero or more
    leading components", which yields the component-boundary suffix
    semantics with no regex surgery. Skip the prepend if the pattern
    already starts with `/` (user is deliberately root-anchoring) — that
    degrades to full-scope behavior, which is what a leading `/` means.
    Keep the existing `!pattern.is_empty()` guard.
  - exact branch: under partial scope emit
    `format!("(?:^|/){}$", regex::escape(pattern))` instead of `^…$`.
  - regex / fixed-strings branches: unchanged.
- `full_path_base` (line ~387): populate for partial scope as well
  (`full_path || partial_path`); update the error-context string to name
  both flags. `Config` keeps its existing `full_path_base: Option<PathBuf>`
  field — walk-side matching is identical for full and partial.

### Error message upgrade

`ensure_single_search_pattern_is_not_a_path`'s suggestion currently ends
with "use: fd --full-path '…'". Replace that suggestion with
`--partial-path` — it is strictly the better fix for a pasted subpath (works
in every pattern mode; `--full-path` only helps the substring modes). Keep
the `fd . 'dir'` suggestion as-is. The existing tests around
`tests/tests.rs:389–431` assert the first line only, but re-check when the
text changes.

### Windows note

The haystack for full/partial scope is the joined absolute path with native
`\` separators. For the `--exact` anchor use `(?:^|[/\\])` on Windows (`/`
is harmless to include unconditionally; simplest is one cfg'd constant).
The glob mode inherits `globset`'s existing `/`-only separator handling —
same jank as `--full-path --glob` today; explicitly out of scope to fix.

## The `f` alias

Once landed, `F-AS-ALIAS.md`'s recommended alias swaps `-p` for
`--partial-path`:

```sh
alias f='fd -H -I -i --partial-path -M vcs_meta,package,noise'
```

- No behavior change for plain-regex muscle memory (partial ≡ full there,
  and old `f` was substring-over-full-path anyway).
- `f --exact .git` and `f -g '*.lock'` start working — the two motivating
  footguns disappear without any per-invocation undo.
- Doc updates: alias lines (bash/zsh/pwsh), defaults table (`-p` row →
  `--partial-path`), undo table (`--no-partial-path`; `-p` now means
  "escalate to whole-path anchoring"), old-`f` `-n` row → `--no-partial-path`.

## Tests

Integration (`tests/tests.rs`), all against `DEFAULT_DIRS`/`DEFAULT_FILES`
plus a `.git`-shaped fixture where noted:

1. Regex equivalence: `--partial-path 'one/two'` ≡ `--full-path 'one/two'`
   output.
2. `--exact` suffix: `--partial-path --exact <basename>` finds the file;
   `--exact one/b.foo` finds exactly `one/b.foo`; `--exact b.foo` does NOT
   match `ab.foo`-style basenames (component boundary) and does NOT match a
   *prefix* or *middle* of the path (`--exact one` matches the dir `one`
   only, not `one/b.foo`).
3. `--exact` vs. lookalikes: pattern `.git` must not hit `.gitignore`
   (needs a fixture with both).
4. Glob suffix: `--partial-path -g '*.foo'` matches at every depth;
   `-g 'two/c.foo'` matches on component boundaries; `-g '*/c.foo'` etc.;
   `*` still refuses to cross `/`.
5. Glob depth opt-in: `-g 'one/**'` matches descendants; plain `-g 'one'`
   matches only the dir.
6. Root-anchored glob passthrough: leading-`/` pattern behaves like full
   scope (no `**/` prepend).
7. Scope last-wins: `-p --partial-path`, `--partial-path -p`,
   `--partial-path --no-partial-path`, `--partial-path --no-full-path`,
   `--no-partial-path -p` — extend `test_opposing` where the `.` pattern is
   discriminating; otherwise dedicated anchored-pattern tests (the `.`
   pattern can't tell full from partial, cf.
   `test_no_full_path_overrides_full_path`).
8. Path-separator diagnostic: suppressed under `--partial-path`; message
   suggests `--partial-path` when it does fire.
9. `--and` composition: every additional pattern gets the same scope
   anchoring.
10. Smart case / `-i` / `-s` behave under partial scope as under full.
11. Invariant goldens: already exist; must stay green.

## Docs sweep (same surfaces as the matchsets sweep)

- `README.md`: refresh the embedded `fd -h` dump (new visible flag) and add
  a sentence to the full-path discussion.
- `doc/fd.1`: entry after `-p, --full-path` (cross-referencing both
  directions), mention in the `-p` entry that `--partial-path` is the
  substring-friendly variant. Watch the leading-apostrophe roff trap.
- `contrib/completion/_fd`: add `--partial-path` (the `no-` spellings stay
  hidden, consistent with the other override pairs).
- `CHANGELOG.md`: Unreleased feature entry.
- `devdocs/F-AS-ALIAS.md`: per the alias section above.
- `devdocs/PLAN-F-TO-FD.md`: decision-log entry linking here.

## Milestones

- **M1 — core**: cli flag + overrides, `build_pattern_regex` scope handling,
  `full_path_base` wiring, path-separator early-return, tests 1–7, 9–11.
- **M2 — polish**: error-message suggestion swap (+ test 8), Windows anchor
  constant.
- **M3 — docs**: the sweep + `F-AS-ALIAS.md` alias change.

M1 and M2 are small enough to land as one commit if convenient; M3 follows
the established sweep pattern.

## Open questions

1. Short flag: reserve `-P` now, later, or never? (Plan says: later, if it
   earns it.)
2. Should the `-p` long help / man entry actively steer alias authors toward
   `--partial-path`? (Plan says: yes, one cross-referencing sentence.)
3. `--exact` + component-run semantics ("the entry *and* its subtree") — if
   real demand appears, it would be a new spelling (e.g. a trailing `/**` on
   the exact pattern is currently a non-match and thus available), not a
   change to the suffix default.
