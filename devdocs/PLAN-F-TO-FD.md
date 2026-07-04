# Plan: Merge `f`'s Features into `fd`

## Goal

Finish absorbing `f` (`~/p/my/f/`, the bash/pwsh fd wrapper) into this fd fork so that
every capability `f` provides is available natively, while `fd`'s default behavior stays
untouched. When everything lands, `f` itself shrinks to a trivial wrapper that only
remaps its single-letter grammar onto native fd flags.

Naming note: this repo uses the one-word term **matchsets** (see
`PLAN-MATCHSETS.md`, which standardized the term and the `matchsets.kdl`
filename). This plan continues that convention.

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
| Default exclusion "seams" (vcs, metadata) | **Partially done** — matchsets exist (`src/matchsets.rs`, `-m`/`-M`) but no built-ins ship yet |
| `-w` AND patterns | **Already upstream** — `--and` |
| `-Q` single result | **Already upstream** — `-1`/`--max-one-result` |
| `-N` non-empty | **Already upstream** — `-S +1b` |
| Full-path / hidden / no-ignore / ignore-case defaults | **Already upstream as flags** — `-p`, `-H`, `-I` (`-uu`), `-i`; stays opt-in per the invariant |
| Postfix flags (`f pattern -f`) | **Already works** — clap parses interspersed options |
| `--arg help` mini-helps (`-t`, `-S`, `-A`, `-B`, `-R`, `-x`, `-X`) | **Not started** — Workstream 2 |

Remaining work is four streams: finish matchsets, add mini-helps, build the invariance
test net, then rewrite `f` as a thin wrapper.

---

## Workstream 1: Finish Matchsets

The registry, KDL parsing, `-m`/`-M` selection, and walk integration (include filter,
exclude filter with directory pruning) already work. What's missing is the part the
original design punted on: built-ins, extra files, and discoverability.

### Target CLI surface

```text
Selection (existing, unchanged):
  -m, --match <set[,set...]>          keep only entries matching any named set
  -M, --exclude-match <set[,set...]>  drop entries matching any named set;
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

`--matchsets` (the current explicit "load and validate" toggle) is retired:
`--list-matchsets` covers its only real use (validating the config), and keeping both
invites confusion between `--matchsets` and `--matchset-file`. This is a
fork-internal rename; nothing has shipped.

### Registry layering

Three sources, later shadows earlier **by set name**:

1. **Built-ins** (compiled into the binary)
2. **User matchset file** `~/.config/fd/matchsets.kdl` (via `etcetera`
   `choose_base_strategy`, so `$XDG_CONFIG_HOME` works on every non-Windows platform)
3. **`--matchset-file` files**, in command-line order

Shadowing rather than collision errors: it mirrors gitconfig/PATH layering, and it lets
a user redefine `vcs` without needing an escape hatch. `--list-matchsets` prints
provenance so shadowing is visible:

```text
NAME       SOURCE                                    CLAUSES
vcs        builtin (shadowed by user matchsets.kdl) 4 dir-name literals
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
`fd --no-user-matchsets -M vcs` works even when the user matchset file is broken —
that's the escape hatch a wrapper script wants. This replaces the current
`--no-matchsets` flag and its hard error in `matchsets::load_selected` ("matchsets
were requested, but --no-matchsets disabled matchset loading"), plus its test in
`tests/tests.rs`.

### Built-in sets

The built-in registry is the taxonomy in
`tests/fixtures/matchsets/matchsets-sketch.kdl` (decided 2026-07-03):

```kdl
"vcs"          { dir name literal full { ".git"; ".svn"; ".hg"; ".bzr" } }
"build_output" { dir bash { /* target + CACHEDIR.TAG, gradle build dirs */ } }
"cache"        { dir name literal full { "__pycache__"; ".cache" } }
"package"      { dir name literal full { "node_modules"; "__pypackages__" } }
"noise"        { file name literal full { ".DS_Store" } }
```

One gap versus `f`'s seams: `.venv` appears nowhere in the sketch (`f` excluded it by
default). Recommendation: add `".venv"` to `package` — a virtualenv is installed
packages, the direct analog of `__pypackages__`. With that, `f`'s default exclusions
map to `--exclude-match vcs,package,noise` with zero config (the extra
`__pypackages__`/`.bzr` coverage is in `f`'s spirit).

Umbrella sets (e.g. an f-style `metadata` grouping) wait for set composition
(a `use "vcs"`-style clause) rather than duplicating pattern lists — deferred, see
Open Decisions.

Representation: embed the KDL source via `include_str!` and parse it through the same
`Registry::parse` path as user files (one parser, one validator; a unit test asserts
the embedded source always parses). The sketch fixture then ceases to be a fixture and
becomes the shipped source of truth (move it under `src/` or reference it from there).

### Implementation notes

- `src/matchsets.rs`: split `load_selected` into "assemble registry from
  (builtins, user file, `--matchset-file` files)" and "select names"; add provenance
  to each set for `--list-matchsets`; keep per-name shadowing at insert time.
- `src/cli.rs`: add `matchset_files: Vec<PathBuf>` and `list_matchsets: bool`;
  remove `load_matchsets`; rename `no_matchsets` to `no_user_matchsets`.
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
| (default) | `-uu -p -i --exclude-match vcs,package,noise` |
| `-O` | drop `-H` from defaults |
| `-G` | drop `-I` from defaults |
| `-n` | drop `-p` from defaults |
| `-C` | `-s` instead of `-i` |
| `-V` | drop `vcs` from default `--exclude-match` |
| `-M` | drop `package,noise` from default `--exclude-match` |
| `-f` / `-r` / `-b` | `-tf` / `-td` / `-tx` |
| `-w <pat>` | `--and <pat>` |
| `-P <cond>` | `--prune-if <cond>` (delete vendored scanner, `tools/`, `vendor/`) |
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
f() { fd -uu -p -i --exclude-match vcs,package,noise "$@"; }
```

for users who want the defaults but not `f`'s single-letter grammar. The full wrapper
and the documented alias are not exclusive; do both.

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
  f-style alias / shell-function suggestion (Workstream 4).
- Man page (`doc/fd.1`): new flags and the `help`-value convention.
- Shell completions: regenerate; consider completing built-in set names for `-m`/`-M`.
- CHANGELOG: entries for matchset built-ins/files/listing, mini-helps, and the
  renames `--matchsets` → `--list-matchsets`, `--no-matchsets` →
  `--no-user-matchsets`.
- Delete or fold `PLAN-MATCHSETS.md`'s open-decisions section into this doc once
  Workstream 1 resolves them.

---

## Decision Log (2026-07-03)

- Terminology is **matchsets**; `~/.config/fd/*` is the **user configuration space**.
- `-m/--match`, `-M/--exclude-match`, registry layering with name shadowing: as
  specified above.
- `--no-matchsets` becomes `--no-user-matchsets`.
- `--list-matchsets` subsumes `--matchsets`'s validate role; `--matchsets` retired.
- Built-ins = the `matchsets-sketch.kdl` taxonomy (not the minimal f-exclusions
  pair).
- `-m help` / `-M help`: **will not implement** — `help` is a plausible set name.
- The f experience stays out of fd core: contrib script and/or documented alias.

## Open Decisions

1. **`.venv` placement**: the sketch omits it, but `f` excluded it by default.
   Recommendation: add `".venv"` to the built-in `package` set (see Workstream 1).
2. **Case sensitivity of matchset patterns** currently follows global `-s`/`-i`/smart
   case at compile time — revisit soon (parked 2026-07-03). Current recommendation:
   keep, consistent with search patterns.
3. **Set composition syntax** (`use "vcs"` inside a set, enabling umbrella sets like
   an f-style `metadata`) — revisit soon (parked 2026-07-03).

## Milestones

1. **M1 — Matchsets complete**: built-ins (sketch taxonomy), `--matchset-file`,
   `--list-matchsets`, `--no-user-matchsets` rename/rescope, provenance, shadowing.
   Tests: embedded-KDL parse, layering/shadowing, `--no-user-matchsets` + builtin
   selection, listing output.
2. **M2 — Invariance net** (small; can precede or interleave with M1): config-isolation
   test, named base-case goldens.
3. **M3 — Mini-helps**: `src/value_help.rs`, `or_help` combinator, post-parse checks.
   Tests: each topic exits 0 with expected stdout; `-x ./help` still execs.
4. **M4 — `f` wrapper rewrite** (contrib script and/or `f` repo): translation table
   above, delete vendored scanner, update `test/run.sh` suite, re-run
   `tests/fd_compat` against this fork; add the alias suggestion to the docs.
5. **M5 — Docs sweep**: README, man page, completions, CHANGELOG.

Validation for every fd milestone:

```sh
cargo fmt
cargo check
cargo test
```
