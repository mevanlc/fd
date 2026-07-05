# The `f` experience as an alias

`f` was a standalone wrapper (bash + PowerShell, ~550 lines each plus a
vendored scanner) that gave fd "search everything" defaults and a
single-letter grammar. Everything it did is native to this fork now, so the
whole tool reduces to an alias.

## The alias

```sh
# bash / zsh
alias f='fd -H -I -i -p -M vcs_meta,package,noise'

# or as a function (plays nicer with completion wrappers)
f() { fd -H -I -i -p -M vcs_meta,package,noise "$@"; }
```

```powershell
# PowerShell (replaces f.ps1)
function f { fd -H -I -i -p -M vcs_meta,package,noise @args }
```

What the defaults mean:

| flag | effect |
|---|---|
| `-H` | search hidden files and directories |
| `-I` | ignore the ignore files (`.gitignore`, `.fdignore`, …) |
| `-i` | case-insensitive (fd's default is smart case) |
| `-p` | match the pattern against the full path, not just the basename |
| `-M vcs_meta,package,noise` | exclude the built-in matchsets: VCS metadata (`.git`, `.svn`, `.hg`, `.bzr`), package/env dirs (`node_modules`, `__pypackages__`, `.venv`), and OS noise (`.DS_Store`) |

`-H -I` can also be spelled `-uu`. Add `trash` to the exclusion list if you
want OS trash locations (`~/.Trash`, `$RECYCLE.BIN`, `.Trash-*`, …) hidden
too.

## Undoing the defaults per invocation

Every default in the alias can be countermanded later on the same command
line — no wrapper logic needed.

The boolean flags pair with clap-level overrides:

| to undo | append |
|---|---|
| `-i` | `-s` (case-sensitive) or `--case-sensitive` |
| `-H` | `--no-hidden` |
| `-I` | `--ignore` (respect ignore files again) |

The matchset selection folds left to right across all `-m`/`-M`
occurrences, so a later value edits the alias's list:

| to get | append |
|---|---|
| stop excluding one set | `-M package-` (trailing `-` removes; idempotent, like rg's `-T`) |
| exclude nothing — search absolutely everything | `-M-` (bare `-` clears the selection) |
| exclude *only* noise, whatever the alias says | `-M-,noise` (clear, then add) |
| also exclude another set | `-M trash` |

Long spellings of the clear: `--clear-matchsets` /
`--clear-exclude-matchsets`. Misspelled names still fail loudly — every
mentioned name, even one only removed, must be a known matchset.

Note the one clap parsing quirk: a *list* starting with `-` must use the
attached or `=` form (`-M-,noise` or `-M=-,noise`, not `-M -,noise`),
because a space-separated value may not begin with a dash. A lone `-` is
fine in any form.

## What replaced the rest of `f`'s grammar

`f`'s remaining value was single-letter remaps of longer fd flags. The
native spellings:

| old `f` | native fd |
|---|---|
| `-O` (hide dotfiles) | `--no-hidden` |
| `-G` (respect ignores) | `--ignore` |
| `-n` (basenames only) | drop `-p` → there is no `--no-full-path`; put `-p` in the alias only if you want it always, or define a second alias |
| `-C` | `-s` |
| `-V` (show VCS metadata) | `-M vcs_meta-` |
| `-M` (show package dirs) | `-M package-,noise-` |
| `-f` / `-r` / `-b` | `-tf` / `-td` / `-tx` |
| `-w <pat>` | `--and <pat>` |
| `-P <cond>` | `--exclude-if <cond>` |
| `-A` / `-B` | `--changed-within` / `--changed-before` |
| `-Q` | `-1` |
| `-N` (non-empty) | `-S +1b` |
| `-m` (mount) | `--one-file-system` |
| `-R <sort>` | `--sort <sort>` |
| `-z` | `-0` |

`f`'s option mini-helps are fd's now: pass `help` as the value
(`fd -S help`, `fd --exclude-if help`, `fd -t help`, …).

Not carried over: `f`'s postfix option placement (`f pattern -f`) — fd
wants options before the pattern, or after a `--` only for patterns/paths —
and `f`'s flag clustering of value-taking options (`-d3`-style attachment
is native clap, `-fQ`-style boolean clusters work too).

## History

A full thin-wrapper rewrite of `f` (same old grammar, remapped onto these
fd features) shipped briefly as `contrib/scripts/f` and was retired in
favor of this alias; recover it with:

```sh
git log --diff-filter=D -- contrib/scripts/f   # find the deletion commit
git show <deletion-commit>^:contrib/scripts/f
```

The design history — why `--exclude-if` and not `--prune-if` for `-P`, the
matchset undo grammar, the built-in set taxonomy — is in
`PLAN-F-TO-FD.md`.
