# fd — fork features

This is a fork of [sharkdp/fd](https://github.com/sharkdp/fd) that regularly
merges upstream `master`. Everything upstream fd does works here; this file
documents what the fork adds or changes.

## Matchsets — named, reusable filters

A *matchset* is a named collection of match clauses defined in
[KDL](https://kdl.dev/). Matchsets replace ad-hoc stacks of `-E` globs with
filters you can define once, name, layer, and share.

```sh
fd -M vcs_meta,os_meta    # exclude entries matching those sets
fd -m package             # only entries matching the 'package' set
fd --list-matchsets       # table of available sets: NAME / SOURCE / CLAUSES
```

Flags:

| Flag | Meaning |
|---|---|
| `-m`, `--include-matchsets <set[,set…]>` | include only entries matching any named set |
| `-M`, `--exclude-matchsets <set[,set…]>` | exclude entries matching any named set |
| `--matchset-file <path>` | load additional sets from a KDL file (repeatable) |
| `--no-user-matchsets` | skip the user matchset file only |
| `--list-matchsets` | list all sets with source and clause summary, then exit |

Sets are resolved in layers, later shadowing earlier by name:
**built-ins** < **user file** (`~/.config/fd/matchsets.kdl`) < **`--matchset-file`
arguments** (in order). `--list-matchsets` shows which definition won.

Built-in sets (embedded in the binary, `src/matchset_builtins.kdl`):

- `vcs_meta` — VCS metadata entries (`.git`, `.svn`, `.hg`, `.bzr` directories,
  plus `.git` pointer *files* used by worktrees/submodules). Named `vcs_meta`
  rather than `vcs` because in fd's flag vocabulary (`--no-ignore-vcs`) "vcs"
  refers to ignore *rules*, not metadata entries.
- `build_output` — build product directories, matched context-sensitively via
  bash clauses (`target` containing a `CACHEDIR.TAG`, `build` next to Gradle
  files) so a source directory that happens to be named `build` is not caught
- `cache` — cache directories (`__pycache__`, `.cache`)
- `package` — package/dependency directories (`node_modules`,
  `__pypackages__`, `.venv`)
- `os_meta` — OS-generated metadata files (`.DS_Store`)
- `trash` — platform trash locations (macOS, FreeDesktop, Windows); separate
  from `os_meta` so trash exclusion stays opt-in

Clause grammar: clauses are subject-named nodes (`name`, `path`, `bash`) with
an optional entry-type constraint as a KDL type annotation — e.g.
`(d) name literal full { ".git" }` or `(f,-e) name glob full`. The annotation
is a CSV of optionally negated type predicates ANDed together (`x` is the pure
exec-bit predicate); an unannotated clause matches every entry kind. Matchset
patterns always compile case-sensitively, independent of the search pattern's
smart-case.

### Location variables in path patterns

A path pattern may start with a location variable anchoring it to a semantic
location instead of the search root:

- `$<home>/…` — the user's home directory
- `$<vroot>/…` — any volume root (mount point, detected by the same
  device-number comparison `--one-file-system` uses)

This generalizes gitignore-style leading-slash anchoring: a directory merely
named `.Trashes` deep in a tree no longer looks like volume trash. Rules:
leading position only, path clauses only, full mode only, literal and glob
kinds only. `$` stays literal unless followed by `<` (so `$RECYCLE.BIN` needs
no escaping); a leading literal `$<` is written `$$<`; unknown variables are
load-time errors. The `trash` builtin uses both anchors.

## Bash conditional expression filtering

Search predicates can be written as bash conditional expressions (the
`[[ … ]]` language), parsed and evaluated natively via
[`bash-condexp`](https://crates.io/crates/bash-condexp) — no shell is spawned.

- `--bash` — treat the search pattern itself as a conditional expression
  (conflicts with `--glob`/`--regex`/`--fixed-strings`/`--exact`)
- `--prune-if <condexp>` — do not descend into directories satisfying the
  expression
- `--exclude-if <condexp>` — exclude entries satisfying the expression
  (evaluated in the entry's parent directory for files, in the directory
  itself for directories)

Expressions can use fd's placeholder variables: `${}` (path), `${/}`
(basename), `${//}` (parent), `${.}` (path without extension), `${/.}`
(basename without extension).

```sh
fd --bash '${/} == *.log && -s ${}'          # non-empty *.log files
fd --prune-if '-e ${}/.git'                  # don't descend into repos
fd --exclude-if '-x ${} && ${/} != *.sh'     # skip non-.sh executables
```

Simple predicates are detected and compiled to native matchers instead of
being interpreted per entry, so common expressions cost about the same as the
equivalent built-in flags.

## `--sort` / `-R` — metadata sorting

Results can be sorted by file metadata with a multi-key priority expression.
Lowercase is ascending, uppercase descending:

| Key | Field | Key | Field |
|---|---|---|---|
| `s`/`S` | size | `m`/`M` | modified time |
| `n`/`N` | basename | `c`/`C` | changed (ctime) |
| `p`/`P` | full path | `a`/`A` | accessed time |
| `e`/`E` | extension | `b`/`B` | born (creation) time |
| `t`/`T` | entry type | `i`/`I` | inode |

Text keys take collation modifiers: `z` (case-insensitive) and `Z` (natural
number collation).

```sh
fd -R M         # newest first
fd -R e nz      # by extension, then basename case-insensitively
```

## `--summarize` — aggregate output

`--summarize <spec>` prints a summary of the results instead of the results
themselves. The available summary is `fext`, which counts results per file
extension. Options after a colon: `i` case-fold extensions (default on
macOS/Windows), `d` include dotfiles (default on), `s`/`-s` sort by
ascending/descending count.

```sh
fd --summarize fext
fd -tf --summarize fext:@d-i-s
```

## `--list-details` is fully internal

Upstream shells out to `ls`/`gls` for `--list-details`. This fork always uses
a native long-listing implementation:

- works everywhere, including Windows and with an empty `PATH`
- timestamps formatted like `ls` (`%b %e %H:%M` recent, `%b %e  %Y` otherwise)
- composes with `--absolute-path` (upstream forbids the combination) and with
  `--sort`

## `help` as an option value

Options with non-obvious value syntax accept the literal value `help`, which
prints a focused cheat-sheet and exits — no digging through the full man page:

```sh
fd -S help          # size filter syntax
fd -R help          # sort expression syntax
```

Supported by `-t/--type`, `-S/--size`, `-R/--sort`, `--changed-within`,
`--changed-before`, `--summarize`, `-x/--exec`, `-X/--exec-batch`, `--bash`,
`--prune-if`, and `--exclude-if`. Only the bare word triggers it: `-x ./help`
still runs a program named `help`, and `fd help` still searches.

## `f` — a thin wrapper script

`contrib/scripts/f` is a bash wrapper providing "search everything" defaults
with a single-letter option grammar. Its historical feature set is now native
fd: default exclusions map to the built-in matchsets (`vcs_meta`, `package`,
`os_meta`), its predicate flag forwards to `--exclude-if`, and `help` values
forward to fd's mini-helps. It requires an fd with matchset support and finds
the binary via `$F_FD_BIN`, `fd`, or `fdfind`.
