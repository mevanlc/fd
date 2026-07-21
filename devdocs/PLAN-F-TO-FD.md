# Plan: Merge `f`'s Features into `fd`

## Goal

Finish absorbing `f` (`~/p/my/f/`, the bash/pwsh fd wrapper) into this fd fork so that
every capability `f` provides is available natively, while `fd`'s default behavior stays
untouched. When everything lands, `f` itself shrinks to a trivial wrapper that only
remaps its single-letter grammar onto native fd flags.

Naming note: this repo uses the one-word term **matchsets** (see
`PLAN-MATCHSETS.md`, which standardized the term and the `matchsets.kdl`
filename). This plan continues that convention. The compound stays one word in every
casing: `matchsets` in snake_case and flags, `Matchset`/`Matchsets` in CamelCase
(e.g. `CompiledMatchset`) — never `MatchSet`.

## The Invariant

These invocations must produce byte-identical output to upstream `fd` (given identical
trees, flags, and terminal state):

```sh
fd
fd .
fd -g '*'
fd . dir
fd -g '*' dir
```

Consequences:

- No config file is read unless a matchset flag is present on the command line.
  A malformed `~/.config/fd/matchsets.kdl` must not break a plain `fd` run.
- Built-in matchsets are compiled in but inert until selected.
- All new behavior is strictly opt-in via new flags or new flag values.

## Where Things Stand

Most of `f`'s heavy features are already native in this fork (commits since upstream
merge-base `40d8eb3`):

| `f` feature | fd status |
|---|---|
| `-P` prune-if (vendored scanner) | **Done, superseded** — native `--bash`, `--prune-if`, `--exclude-if` (`src/bash_cond.rs`) plus predicate optimizations |
| `-R` metadata sort with mini-help syntax | **Done** — `--sort`/`-R` redesign (`parse_sort` in `src/cli.rs`) |
| `-l` list details | **Done** — internal long-listing, ls-like timestamps, works with `-a` |
| Default exclusion "seams" (vcs, metadata) | **Done** — matchsets complete (M1, 2026-07-03): annotation clause grammar, built-ins (`src/matchset_builtins.kdl`), `--matchset-file`, `--list-matchsets`, layering/shadowing, fixed case sensitivity |
| `-w` AND patterns | **Already upstream** — `--and` |
| `-Q` single result | **Already upstream** — `-1`/`--max-one-result` |
| `-N` non-empty | **Already upstream** — `-S +1b` |
| Full-path / hidden / no-ignore / ignore-case defaults | **Already upstream as flags** — `-p`, `-H`, `-I` (`-uu`), `-i`; stays opt-in per the invariant |
| Postfix flags (`f pattern -f`) | **Already works** — clap parses interspersed options |
| `--arg help` mini-helps (`-t`, `-S`, `-A`, `-B`, `-R`, `-x`, `-X`) | **Done** — M3 (2026-07-03): `src/value_help.rs`, `OrHelp` value-parser combinator, post-parse checks for the `--bash` family |

Remaining work is four streams: finish matchsets, add mini-helps, build the invariance
test net, then rewrite `f` as a thin wrapper.

---

## Workstream 1: Finish Matchsets

The registry, KDL parsing, `-m`/`-M` selection, and walk integration (include filter,
exclude filter with directory pruning) already work. What's missing is the part the
original design punted on: built-ins, extra files, and discoverability.

### Target CLI surface

```text
Selection (shorts unchanged):
  -m, --include-matchsets <set[,set...]>  keep only entries matching any named set
  -M, --exclude-matchsets <set[,set...]>  drop entries matching any named set;
                                          matching directories are pruned

Registry (new / revised):
      --matchset-file <path>         load sets from a KDL file (repeatable)
      --no-user-matchsets            skip the user matchset file
      --list-matchsets               load everything, print available sets
                                      with provenance and clause summaries, exit
```

Terminology: `~/.config/fd/*` is the **user configuration space**; the file it holds
for this feature is the **user matchset file**. Docs and flag names say "user", not
"well-known".

The long forms are plural (`--include-matchsets` and `--exclude-matchsets`) because
the value is a comma-separated list of set names (and the flags are repeatable) —
same convention as cargo's `--features`. Naming both sides makes the inclusion versus
exclusion distinction explicit and separates selection from registry flags such as
`--matchset-file` and `--list-matchsets`.

The old boolean `--matchsets` toggle ("load and validate the config") is retired —
`--list-matchsets` covers its only real use. The inclusion selection flag that was
initially `--match` is now `--include-matchsets`; `--matchsets` is not retained as an
alias. All fork-internal; nothing has shipped.

### Registry layering

Three sources, later shadows earlier **by set name**:

1. **Built-ins** (compiled into the binary)
2. **User matchset file** `~/.config/fd/matchsets.kdl` (via `etcetera`
   `choose_base_strategy`, so `$XDG_CONFIG_HOME` works on every non-Windows platform)
3. **`--matchset-file` files**, in command-line order

Shadowing rather than collision errors: it mirrors gitconfig/PATH layering, and it lets
a user redefine `vcs_meta` without needing an escape hatch. `--list-matchsets` prints
provenance so shadowing is visible:

```text
NAME       SOURCE                                    CLAUSES
vcs_meta   builtin (shadowed by user matchsets.kdl) 4 dir-name literals
package    builtin                                   2 dir-name literals
mystuff    --matchset-file ./sets.kdl               1 dir bash
```

Loading stays lazy: the registry is only assembled when `-m`, `-M`,
`--matchset-file`, or `--list-matchsets` appears. Built-ins are static data with no
startup cost when unselected.

### `--no-matchsets` → `--no-user-matchsets`

The flag is renamed and rescoped: it means "don't read the user matchset file"
(analogous to `--no-ignore`), **not** "disable matchsets entirely". Built-ins and
explicit `--matchset-file` files remain available, so
`fd --no-user-matchsets -M vcs_meta` works even when the user matchset file is broken —
that's the escape hatch a wrapper script wants. This replaces the current
`--no-matchsets` flag and its hard error in `matchsets::load_selected` ("matchsets
were requested, but --no-matchsets disabled matchset loading"), plus its test in
`tests/tests.rs`.

### Built-in sets

The built-in registry is the taxonomy in
`tests/fixtures/matchsets/matchsets-sketch.kdl` (decided 2026-07-03):

```kdl
// shown in the revised clause grammar; the sketch file migrates with it
"vcs_meta"     { (d) name literal full { ".git"; ".svn"; ".hg"; ".bzr" }
                 (f) name literal full { ".git" } }
"build_output" { (d) bash { /* target + CACHEDIR.TAG, gradle build dirs */ } }
"cache"        { (d) name literal full { "__pycache__"; ".cache" } }
"package"      { (d) name literal full { "node_modules"; "__pypackages__"; ".venv" } }
"os_meta"      { (f) name literal full { ".DS_Store" } }
"trash"        { (d) path literal full { "$<home>/.Trash"; "$<home>/.local/share/Trash" }
                 (d) path literal full { "$<vroot>/.Trashes"; "$<vroot>/$RECYCLE.BIN";
                                         "$<vroot>/System Volume Information" }
                 (d) path glob full { "$<vroot>/.Trash-*" } }
```

`trash` (added 2026-07-04) is a separate set, not part of `os_meta`: `os_meta` stays
"metadata litter files" so the `f` wrapper's default
(`--exclude-matchsets vcs_meta,package,os_meta`) keeps old `f` behavior; trash
exclusion is opt-in via `-M trash`.

`.venv` lives in `package` (resolved 2026-07-03; already present in the sketch) — a
virtualenv is installed packages, the direct analog of `__pypackages__`. `f`'s default
exclusions therefore map to `--exclude-matchsets vcs_meta,package,os_meta` with zero config
(the extra `__pypackages__`/`.bzr` coverage is in `f`'s spirit).

Umbrella sets (e.g. an f-style `metadata` grouping) wait for set composition
(a `use "vcs_meta"`-style clause) rather than duplicating pattern lists — parked, see
Open Decisions.

### Case sensitivity (decided 2026-07-03)

Matchset patterns compile **case-sensitive, always** — no coupling to `-s`/`-i`/smart
case. A named set is a definition; its meaning must not drift with the casing of an
adjacent search pattern (previously, `fd -M vcs_meta foo` vs `fd -M vcs_meta Foo` produced
different exclusion behavior, because smart case resolves off the search pattern).
Fixed sensitivity also matches `-E`, whose ignore-crate override globs are
unconditionally case-sensitive, and bash clauses get real `[[ ]]` semantics.

Implementation: drop the `case_sensitive` parameter from `matchsets::load_selected`
and `compile_matcher`; compile with `case_insensitive(false)`. A per-clause `icase`
atom can be added later if a real need appears — do not build it speculatively.

Representation: embed the KDL source via `include_str!` and parse it through the same
`Registry::parse` path as user files (one parser, one validator; a unit test asserts
the embedded source always parses). The sketch fixture then ceases to be a fixture and
becomes the shipped source of truth (move it under `src/` or reference it from there).

### Clause grammar: subject-named nodes, type-annotation constraint

Decided 2026-07-03 (supersedes the original one-type-atom grammar in
`parse_atoms`, a bare-atom-conjunction draft, and a `type=` property draft from
the same day). Clause nodes are named by what they match on; the entry-type
constraint is a KDL **type annotation** prefixing the node name:

```text
[([-]type[,[-]type...])] <name|path> <pattern-kind> <mode> { patterns... }
[([-]type[,[-]type...])] bash { conditions... }
```

```kdl
"vcs_meta" {
    (d) name literal full { ".git"; ".svn"; ".hg"; ".bzr" }
}

"backups" {
    name glob full { "*.bak"; "*~" }          // unannotated → any entry kind
}

"stray" {
    (f,x,e) name glob full { "*" }            // empty executable files
}

"non-empty-plain" {
    (file,-x,-e) name glob full { "*" }       // non-executable, non-empty files
}

"build_output" {
    (d) bash { "${/} == target && -f CACHEDIR.TAG" }
}
```

Rules:

- Canonical style is annotation-then-space-then-name: `(d) name literal full`.
  The constraint leads, the clause verb follows. The fused form `(d)name` and
  inner padding `( d )` are equally legal KDL; docs and builtins use the spaced
  form.
- Spec fitness: KDL 2.0 defines a node-name annotation as "a context-specific
  elaboration of the more generic type the node name indicates"
  (`draft-marchan-kdl2.md` §Type Annotation) — precisely this use. Parsers
  expose it directly (kdl-rs: `node.ty()`).
- The annotation value is a comma-separated list of the nine type names or
  their one-letter aliases, each optionally negated with a leading `-`. Bare
  (unquoted) values are legal: per the spec's `identifier-char` grammar, commas
  and hyphens are permitted (verified empirically against kdl-rs 6.5, including
  `(-d)` and whitespace separation). Quoted forms also work.
- List elements are independent predicates ANDed together. At most one
  *positive structural* type (`f/d/l/s/p/b/c`); `x` and `e` are attribute
  predicates. `-` inverts a predicate: `(-d)` is anything but a directory,
  `(f,-e)` is non-empty files. A positive structural forbids negated
  structurals in the same list (redundant or contradictory); `x,-x`-style
  pairs and duplicate elements are errors.
- **`x` is the pure exec-bit predicate in the KDL** — unlike CLI `-t x`, it
  does not imply "regular file". Write `(f,x)` for that meaning (the docs and
  builtins always do). `(d,x)` (traversable dir) means what it says.
- Negative predicates are the logical NOT of the positives, so `(-d)` matches
  entries whose file type cannot be determined; positives never do.
- **No annotation means no type constraint**: the clause applies to every entry
  kind, including unknown-file-type entries (which today can never match any
  clause). No `any` keyword.
- Migration: builtins, fixtures, and doc examples rewrite from
  `dir name literal full` to `(d) name literal full`; the old bare-atom type
  prefix is dropped from the grammar entirely.
- Note for the parked scoping-controls sketch (Open Decisions): it also uses
  annotations (`(local)` on set and `use` nodes). No conflict — different node
  kinds — and it makes annotations the config format's general mechanism for
  node metadata.
- Future per-clause options (e.g. a case-sensitivity override) slot in as
  properties (`key=value`) with no grammar change.

Implementation: `EntryType` becomes a conjunction of signed predicates
(`structural: Option<(Structural, bool)>`, `executable: Option<bool>`,
`empty: Option<bool>` — `bool` = polarity); the clause parser reads
`node.ty()` for the constraint, the node name for the clause kind, and
positional args for pattern-kind/mode. An unannotated clause skips the
`file_type()` lookup entirely.

### Implementation notes

- `src/matchsets.rs`: split `load_selected` into "assemble registry from
  (builtins, user file, `--matchset-file` files)" and "select names"; add provenance
  to each set for `--list-matchsets`; keep per-name shadowing at insert time. Types
  follow the one-word casing: `CompiledMatchset`, `SelectedMatchsets`, etc. — sweep
  any remaining `MatchSet`-humped identifiers.
- `src/cli.rs`: rename the selection long forms (`long = "match"` → `"include-matchsets"`,
  `long = "exclude-match"` → `"exclude-matchsets"`); add
  `matchset_files: Vec<PathBuf>` and `list_matchsets: bool`; remove the old
  `--matchsets` bool; rename `no_matchsets` to `no_user_matchsets`.
  `parse_matchset_name` stays as-is: `help` is a legitimate set name (no `-m help`
  mini-help; see Workstream 2).
- `src/main.rs`: trigger registry load on any of the four flags; handle
  `--list-matchsets` before `construct_config` and exit 0.

---

## Workstream 2: `--arg help` Mini-Helps

`f` lets users pass `help` as the value of a syntax-heavy option to get a focused
cheat-sheet. Port the convention to fd for the options with non-obvious value syntax:

| Option | Topic content (adapted from `f`) |
|---|---|
| `-t` | type letters/names, OR-composition |
| `-S` | `+`/`-` prefixes, base-10 vs base-2 units, examples |
| `--changed-within` / `--changed-before` | durations, date formats, `@unix` |
| `-R` / `--sort` | field letters, case = direction, `z`/`Z` collation, examples |
| `-x` / `-X` | placeholder table (`{}`, `{/}`, `{//}`, `{.}`, `{/.}`), implicit `{}`, examples |
| `--summarize` | summary-spec grammar, `fext` options |
| `--bash` / `--prune-if` / `--exclude-if` | condexp variables and operators |

Explicitly **not** `-m`/`-M`: `help` is a plausible matchset name, and the workaround
for discoverability is easy — `--list-matchsets`, `--help`, or the man page.
(Decided 2026-07-03.)

### Mechanism

Two cases, one shared source of topic text (new module `src/value_help.rs` holding the
topic strings, printed to stdout with exit code 0, like `--help`):

1. **Typed values parsed eagerly by clap** (`-S`, `-t`, `-R`, `--changed-*`,
   `--summarize`): wrap the existing value parser in an `or_help` combinator that
   recognizes the literal `help`, prints the topic, and exits. Without this, clap
   rejects `help` as a parse error before `main` ever sees it. Exiting inside a value
   parser is a deliberate side effect — it runs once, inside `Opts::parse()`, exactly
   where `--help` itself exits.
2. **String-ish values that survive parsing** (`-x`/`-X` first command token,
   `--bash`-family expressions): check post-parse in `main` before `construct_config`.

Escape hatch, same caveat `f` carries: only the bare word `help` triggers it. A real
program named `help` is reachable as `-x ./help` or an absolute path.

Mention the convention once in `--help` ("many options accept 'help' as a value for
format details") rather than repeating it per-option.

---

## Workstream 3: Invariance Test Net

Matchsets are opt-in by design — nothing loads without `-m`, `-M`,
`--matchset-file`, or `--list-matchsets` — so the base cases should hold trivially.
These tests are regression guards, locking that property in before the remaining
features churn the surface:

- **Config isolation test**: point `XDG_CONFIG_HOME` at a directory containing a
  deliberately malformed `matchsets.kdl`; assert each of the five base invocations
  succeeds with output identical to a run with no config at all. This is the test that
  proves lazy loading (`etcetera`'s base strategy honors `XDG_CONFIG_HOME` on macOS and
  Linux; gate the test off Windows or set the Known Folder equivalent).
- **Base-case goldens**: integration tests running the five invocations against the
  existing test fixture tree, asserting exact output. These exist implicitly across
  `tests/tests.rs`; make them explicit and named so a failure identifies itself as an
  invariant break, not a feature bug.
- **Fork-vs-upstream spot check** (optional, CI or script in `devdocs/`): build
  upstream at the merge-base, run both binaries over a fixture tree for the five cases,
  diff. Cheap insurance against accidental default drift (e.g. the `--list-details`
  timestamp work touching non-`-l` output).

`f`'s `tests/fd_compat/` harness (extracts fd's own integration tests and replays them
through `f`) stays in the `f` repo — after Workstream 4 it re-validates the wrapper
against this fork.

---

## Workstream 4: Rewrite `f` as a Thin Wrapper

Once built-ins land, everything `f` does is a flag remap. The bash script drops from
~550 lines (plus the vendored `tools/f-prune-scan` cargo project) to a small
translation table:

| `f` | native fd |
|---|---|
| (default) | `-uu -p -i --exclude-matchsets vcs_meta,package,os_meta` |
| `-O` | drop `-H` from defaults |
| `-G` | drop `-I` from defaults |
| `-n` | drop `-p` from defaults |
| `-C` | `-s` instead of `-i` |
| `-V` | drop `vcs_meta` from default `--exclude-matchsets` |
| `-M` | drop `package,os_meta` from default `--exclude-matchsets` |
| `-f` / `-r` / `-b` | `-tf` / `-td` / `-tx` |
| `-w <pat>` | `--and <pat>` |
| `-P <cond>` | `--exclude-if <cond>` (delete vendored scanner, `tools/`, `vendor/`). `--exclude-if`, not `--prune-if`: old `-P` hid a matching directory entirely (prune paths became `-E` excludes), and that is `--exclude-if`'s directory behavior; `--prune-if` would keep the pruned directory itself in the output. Conditions rewrite from scanner variables (`$path`, `$name`, `$root`, …) to condexp placeholders (`${}`, `${/}`, relative file tests); the root-relative variables (`$root`, `$rpath`, …) have no condexp equivalent and retire with the scanner. |
| `-A` / `-B` | `--changed-within` / `--changed-before` |
| `-Q` | `-1` |
| `-N` | `-S +1b` |
| `-m` (mount) | `--one-file-system` |
| `-z` | `-0` |
| `-g`, `-F`, `-a`, `-l`, `-L`, `-d`, `-e`, `-E`, `-t`, `-S`, `-R`, `-x`, `-X` | pass through unchanged |

`f`'s own mini-help text blocks are deleted: `help` values forward straight to fd,
which now owns the single copy (Workstream 2). Same change in `f.ps1`.

Where it lives (decided 2026-07-03): out of fd core. Either ship the rewritten
wrapper as a contrib script (`contrib/f` in this repo, or it stays in the `f` repo),
or reduce further to an alias / shell-function suggestion in the docs — e.g.

```sh
f() { fd -uu -p -i --exclude-matchsets vcs_meta,package,os_meta "$@"; }
```

for users who want the defaults but not `f`'s single-letter grammar. The full wrapper
and the documented alias are not exclusive; do both.

**Superseded 2026-07-05**: the wrapper is retired; the alias *is* the f story
(`devdocs/F-AS-ALIAS.md`). The `-m`/`-M` selection-undo grammar (trailing `-`,
bare `-` clear) removed the last daily-use gap between alias and wrapper —
old f's reversal letters (`-V`, `-M`, `-O`, `-G`, …) are now spelled inline
(`-M vcs_meta-`, `-M-`, `--no-hidden`, `--ignore`). Keeping the wrapper meant
maintaining ~300 fresh lines of bash grammar (and eventually a `f.ps1` again),
which is exactly what this project set out to stop. The rewrite shipped
briefly as `contrib/scripts/f` and is recoverable from git history.

Alternatives considered for "the f experience" and rejected:

- **argv[0] detection** (busybox-style `f` hardlink): requires reimplementing `f`'s
  distinct short-flag grammar inside fd's clap definition, where half the letters
  (`-m`, `-M`, `-F`, `-l`, …) already mean something else. Two grammars in one parser
  is the opposite of elegant.
- **`--preset f` / profile flag**: adds surface for something an alias does; presets
  invite "which flags does the preset expand to at which precedence" questions.
- **Config-file default args**: violates the invariant in spirit (base-case output
  becomes machine-dependent) and diverges from upstream fd's no-config philosophy more
  than the on-demand `matchsets.kdl` does.

The wrapper keeps `f`'s real value — a different *grammar*, not different capabilities —
while fd stays pure.

---

## Workstream 5: Docs and Release Hygiene

- README: matchset section (concept, KDL format, built-ins, layering, examples),
  mini-help convention, one-line pointer from the `--exclude` docs to `-M`, and an
  f-style alias / shell-function suggestion (condensed from `devdocs/F-AS-ALIAS.md`).
- Man page (`doc/fd.1`): new flags and the `help`-value convention.
- Shell completions: regenerate; consider completing built-in set names for `-m`/`-M`.
- CHANGELOG: feature entries for matchsets (`-m/--include-matchsets`,
  `-M/--exclude-matchsets`, built-ins, `--matchset-file`, `--list-matchsets`,
  `--no-user-matchsets`) and mini-helps. The internal renames need no entries —
  none of the old spellings ever shipped.
- Delete or fold `PLAN-MATCHSETS.md`'s open-decisions section into this doc once
  Workstream 1 resolves them.

---

## Decision Log (2026-07-03, amended 2026-07-21)

- Terminology is **matchsets** — one word in every casing (`Matchset` in CamelCase,
  never `MatchSet`); `~/.config/fd/*` is the **user configuration space**.
- Selection flags: `-m/--include-matchsets`, `-M/--exclude-matchsets` (long forms
  renamed from `--match`/`--exclude-match`; plural because the value is a CSV of set
  names, and explicit `include` gives the pair conceptual parity).
  Registry layering with name shadowing: as specified above.
- `--no-matchsets` becomes `--no-user-matchsets`.
- `--list-matchsets` subsumes the old boolean `--matchsets` validate role; the later
  inclusion-selection spelling `--matchsets` is also retired in favor of
  `--include-matchsets`.
- Built-ins = the `matchsets-sketch.kdl` taxonomy (not the minimal f-exclusions
  pair).
- `-m help` / `-M help`: **will not implement** — `help` is a plausible set name.
- The f experience stays out of fd core: contrib script and/or documented alias.
- Clause grammar: nodes named by subject (`name`/`path`) or `bash`; the entry-type
  constraint is a KDL type annotation prefixing the node name —
  `(file,-d,x,empty) name ...` — a CSV of optionally-`-`-negated type predicates,
  AND semantics, `x` = pure exec bit. Canonical style is spaced:
  `(d) name literal full`. An unannotated clause has no type constraint.
  Bare-atom type prefixes and the interim `type=` property draft are dropped.
  Negation is part of the spec (not deferred). Spec-sanctioned use: annotations
  are "a context-specific elaboration of the more generic type the node name
  indicates".
- Matchset patterns compile **case-sensitive, always** — no `-s`/`-i`/smart-case
  coupling (see Workstream 1, "Case sensitivity").
- `.venv` lives in the builtin `package` set (already present in the sketch).
- The VCS builtin is named **`vcs_meta`**, not `vcs` (renamed 2026-07-03 after
  M1 landed): in fd's existing flag vocabulary (`--no-ignore-vcs`) "vcs" means
  VCS ignore *rules*, while this set matches VCS *metadata* entries — one word
  for two referents invited misreading (`-M vcs` ≠ "exclude vcs-ignored
  entries"). A type-suffixed name (`vcsdir`) was rejected because the set is
  not directories-only: it also matches `.git` *files* (git worktree and
  submodule pointers) via a `(f)` clause added in the same change, restoring
  `f`'s `-E .git` coverage.
- Set composition stays parked; direction and a scoping-controls sketch are recorded
  under Open Decisions.

## Decision Log (2026-07-04)

- **OS-dependent rules: rejected.** Patterns like `.DS_Store` or `$RECYCLE.BIN`
  are self-gating — cross-OS exclusion is desirable (network shares, foreign
  volumes, polluted repos). If ever needed, the reserved syntax is a clause
  property (`os=macos`), not a positional atom.
- **Location anchoring: adopted as pattern variables** — `$<home>/...` and
  `$<vroot>/...` at the start of a `path` pattern anchor it to a semantic
  location. Chosen over a trailing positional atom (muddies the fixed
  `<pattern-kind> <mode>` grammar) and over clause properties (`at=`/`under=`
  needed two concepts; a path expresses both depths naturally). Rules:
  leading position only, `path` clauses only, `full` mode only, `literal`/`glob`
  kinds only (no `regex` tails); `$` is literal unless followed by `<`
  (`$RECYCLE.BIN` needs no escape); a leading literal `$<` is written `$$<`;
  unknown variables are hard errors.
- **`$<home>`** resolves via `etcetera::home_dir()` (already a dependency) at
  matchset-load time — a hard error if unresolvable, which by laziness can only
  fire when a matchset flag is used. Matching is prefix equality against both
  the reported and canonicalized home on the entry's lexically-normalized
  absolute path (so symlinked homes match under either spelling).
- **`$<vroot>`** is a match-time predicate, not an expansion: the anchored
  prefix must be a volume root — no parent (filesystem/drive/UNC root) or a
  device number differing from its parent's (the same `st_dev` trick
  `--one-file-system` uses via the `ignore` crate). Enumerating mounts via
  `sysinfo` was considered and rejected: the predicate needs no new dependency
  and covers every mount type by definition. Known limits: Windows
  junction-style mount points are not detected (drive/UNC roots only);
  same-device bind mounts are invisible; macOS's root/Data volume group shares
  one `st_dev`, so that particular boundary is not detected (verified 2026-07-04;
  real volume boundaries like `/System/Volumes/VM` and external disks are).
- **`trash` builtin added** (see Built-in sets): home- and vroot-anchored, in
  its own set rather than `os_meta`, opt-in for the `f` wrapper.

## Decision Log (2026-07-05)

- **Selection undo for `-m`/`-M`** — so an alias like
  `alias f='fd -M vcs_meta,package,os_meta'` can be partially or fully unwound
  at the prompt:
  - **Trailing-dash negation**: `f -M package-` removes `package` from the
    selection accumulated so far (left-to-right fold across all occurrences;
    plain names add once, `name-` subtracts). Chosen over ripgrep's `!name`
    (shell history expansion forces quoting) and a leading `-name` (needs
    `allow_hyphen_values`, which lets a forgotten option value swallow the
    next flag). Precedent: `apt-get install foo bar-`. Removal is
    *idempotent* — `name-` means "ensure this is not selected", a no-op if it
    never was (like rg's `-tpdf -Tpdf -Tpdf`); a strict not-in-selection
    error was implemented first and rejected because it forces the caller to
    know what an alias selected, defeating the feature. Typo safety comes
    from registry validation instead: every mentioned name, including
    removed ones, must be a *known* set (`-M pakage-` → "unknown matchset
    'pakage'"). Set names declared in KDL must not end with `-` (load-time
    error) so the selection syntax stays unambiguous.
  - **Bare `-` clears**: a lone `-` list item discards the selection
    accumulated so far (`f -M-` ≙ `--clear-exclude-matchsets`; `f -M-,os_meta`
    = "exclude only OS metadata"). clap accepts a lone `-` as an option value even
    space-separated, so `-M -`, `-M-`, and `-M=-` all work; only a
    dash-leading *list* in space form (`-M -,os_meta`) doesn't — use the
    attached form. Empty-token spellings (`-M,,foo`) were rejected: an empty
    list item stays a parse error so a stray comma can't silently clear the
    selection.
  - **`--clear-include-matchsets` / `--clear-exclude-matchsets`**: hidden
    `overrides_with` flags (the `--no-hidden` idiom) that discard all earlier
    occurrences of the paired flag; later occurrences start fresh. Named
    `clear-` rather than `no-` because `no-` reads as disabling the feature
    (and `--no-matchsets` is a retired spelling that must keep failing).

## Open Decisions

1. **Set composition** (`use "name"` inside a set, enabling umbrella sets like an
   f-style `metadata`) — parked 2026-07-03, to be revisited. Agreed direction when it
   lands: `use` as a reserved child-node name, mixable with a set's own clauses;
   resolve against the final layered registry (post-shadowing); validate the whole
   assembled registry (unknown targets and cycles are hard errors); flatten at load
   time; no builtin umbrella set.

   Follow-on idea to evaluate alongside it — **scoping controls** via KDL type
   annotations:

   ```kdl
   (local)"metadata" {          // don't expose this set to CLI selection
       use (local)"package"     // don't look past the current unit (file)
       use "os_meta"            // full scope resolution (final registry)
       dir name literal full { ".venv" }
   }
   ```

   Ordering: composition ships first; scoping controls are the milestone after it
   (relative ordering, independent of this plan's M1–M5 numbering).


## Milestones

1. **M1 — Matchsets complete** *(landed 2026-07-03)*: clause-grammar revision (subject-named nodes,
   type-annotation constraints with negation, typeless clauses, builtin/fixture
   migration), built-ins
   (sketch taxonomy), `--matchset-file`, `--list-matchsets`, `--no-user-matchsets`
   rename/rescope, provenance, shadowing, fixed case sensitivity (drop the
   `case_sensitive` parameter). Tests: embedded-KDL parse, grammar
   conjunction/negation/typeless cases and their validation errors
   (double structural, `x,-x`, duplicates), layering/shadowing,
   `--no-user-matchsets` + builtin selection, listing output, case-sensitive
   matching regardless of `-i`/pattern casing.
2. **M2 — Invariance net** *(landed 2026-07-03)*: config-isolation test, named
   base-case goldens (`test_invariant_*` in `tests/tests.rs`), and the optional
   fork-vs-upstream spot check (`devdocs/check-invariant.sh`; verified clean
   against `40d8eb3`).
3. **M3 — Mini-helps** *(landed 2026-07-03)*: `src/value_help.rs`, `OrHelp`
   `TypedValueParser` combinator (forwards `possible_values()`, so `-t`
   completions survive), exec first-token check in `Exec::from_arg_matches`,
   pre-parse checks for the `--bash` family in `run()` (`help` is a *valid*
   condexp — a non-empty-string test — so it must be intercepted before
   parsing). Tests: each topic exits 0 with expected stdout; `-x ./help`
   still execs; `fd help` still searches.
4. **M4 — `f` wrapper rewrite** *(fd half landed 2026-07-03; wrapper retired
   2026-07-05)*: the thin wrapper shipped as `contrib/scripts/f` (translation
   table above; ~290 lines), verified against the `f` repo's grammar tests,
   then retired once the `-m`/`-M` selection-undo grammar made the plain
   alias competitive — see the superseded note in Workstream 4 and
   `devdocs/F-AS-ALIAS.md` (alias + per-invocation undo + old-letter →
   native-flag table; includes the PowerShell function replacing `f.ps1`).
   Remaining, in the `f` repo: archive it (point its README at
   F-AS-ALIAS.md) rather than replacing `f`/`f.ps1` with a remap.
5. **M5 — Docs sweep** *(landed 2026-07-05)*: README (matchsets section with
   built-ins table, selection-undo grammar and the f alias, KDL clause
   grammar, location variables, layering; refreshed `fd -h` dump; 'help'
   value note; `-E` → matchsets pointer), man page (option entries, a
   MATCHSETS section, matchsets.kdl in FILES, 'help' convention in
   DESCRIPTION — plus a fix for roff dropping source lines that start with
   an apostrophe, two of them pre-existing), zsh completion (`_fd`: flag
   entries + set-name completion via `--list-matchsets` with a static
   fallback; bash/fish/powershell are clap-generated and pick the flags up
   automatically), CHANGELOG (matchsets, selection undo, mini-helps).

Validation for every fd milestone:

```sh
cargo fmt
cargo check
cargo test
```
