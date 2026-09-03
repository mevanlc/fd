# an fd fork

This is a fork of fd. fd is a program for finding entries in your filesystem
with regular-expression and glob-based matching. See the
[upstream repository](https://github.com/sharkdp/fd) for full documentation.

## about this fork

This fork regularly merges upstream `master` and adds a few larger features:

- named, reusable matchsets for inclusion and exclusion filtering;
- native evaluation of bash conditional expressions while searching;
- an opt-in PCRE2 regex engine with look-around and backreferences;
- multi-key sorting by file metadata;
- aggregate file-extension summaries; and
- an internal long-listing implementation that composes with the other output
  features.

It also adds syntax-focused mini-help, alias-friendly override flags and a few
smaller terminal-output fixes. Everything else behaves like upstream fd unless
noted below.

### matchsets

A matchset is a named collection of matching rules. Matchsets replace repeated
stacks of `-E/--exclude` globs with filters that can be named, layered and
shared.

```console
$ fd -M vcs_meta,os_meta
$ fd -m package
$ fd --list-matchsets
```

The main flags are:

| Flag | Meaning |
|---|---|
| `-m, --include-matchsets <set[,set...]>` | include entries matching any selected set |
| `-M, --exclude-matchsets <set[,set...]>` | exclude matching entries and prune matching directories |
| `--matchset-file <path>` | load another KDL matchset file; repeatable |
| `--no-user-matchsets` | skip the user matchset file |
| `--list-matchsets` | list available sets, their sources and clause summaries |

No matchsets are selected by default. Selections are folded from left to right,
which makes defaults embedded in aliases easy to edit:

```console
$ fd -M vcs_meta,package -M package-   # remove package from the selection
$ fd -M vcs_meta,package -M-           # clear the exclusion selection
$ fd -M vcs_meta,package -M-,os_meta   # clear it, then select only os_meta
```

A trailing `-` removes one name. A bare `-` clears the selection; the long
forms are `--clear-include-matchsets` and `--clear-exclude-matchsets`.

Built-in matchsets are embedded in the binary:

| Set | Matches |
|---|---|
| `vcs_meta` | `.git`, `.svn`, `.hg` and `.bzr` metadata, including `.git` pointer files |
| `build_output` | context-sensitive Rust and Gradle build directories |
| `cache` | `__pycache__`, `.cache` and directories containing `CACHEDIR.TAG` |
| `package` | `node_modules`, `__pypackages__` and `.venv` |
| `os_meta` | `.DS_Store` and `System Volume Information` at volume roots |
| `trash` | macOS, FreeDesktop and Windows trash locations |

Custom sets use [KDL](https://kdl.dev/) and normally live in
`~/.config/fd/matchsets.kdl` on macOS and Linux or
`%APPDATA%\fd\matchsets.kdl` on Windows:

```kdl
"scratch" {
    (d) name literal full { (temporary) "tmp"; "scratch" }
    (f) name glob full { "*.bak"; "*.orig" }
    (d) bash { "-f .skip-me" }
}
```

Clauses can match `name` or `path` using `literal`, `glob` or `regex` patterns
in `full` or `partial` mode. A clause annotation such as `(d)`, `(f)` or
`(f,x)` constrains the entry type. An annotation on an individual pattern,
such as `(temporary)` above, is accepted as an organizational tag and does not
currently affect matching. Matchset patterns are always case-sensitive.

Path patterns can begin with `$<home>/` or `$<vroot>/` to anchor them to the
user's home directory or a volume root. Later definitions shadow earlier ones
in this order:

```text
built-ins < user matchset file < --matchset-file arguments in command-line order
```

### bash conditional expressions

Search predicates can use the bash `[[ ... ]]` conditional-expression
language. Expressions are parsed and evaluated natively through
`bash-condexp`; no shell is spawned.

- `--bash` treats the main search pattern as a conditional expression;
- `--prune-if <expression>` prevents descent into matching directories; and
- `--exclude-if <expression>` excludes matching entries.

Expressions can use fd's path placeholders: `${}`, `${/}`, `${//}`, `${.}` and
`${/.}`.

```console
$ fd --bash '${/} == *.log && -s ${}'
$ fd --prune-if '-e ${}/.git'
$ fd --exclude-if '-x ${} && ${/} != *.sh'
```

Common simple expressions are compiled into native matchers instead of being
interpreted separately for every entry.

### pcre2

fd's default regex engine has no look-around and no backreferences. `--pcre2`
switches to PCRE2, which supports both:

```console
$ fd --pcre2 '(?<!test_)main\.rs$'
$ fd --pcre2 '(\w+)_\1'
```

PCRE2 runs in Unicode mode, so `.` matches a whole codepoint and `\w`, `\d` and
`\s` are Unicode-aware, matching the default engine. Filenames that are not
valid UTF-8 are still searched rather than raising an error, but their
ill-formed bytes never match and no pattern can match across them — the default
engine can target those bytes with `(?-u)`, and PCRE2 has no equivalent. `--pcre2`
cannot be combined with `--glob`, `--fixed-strings`, `--exact` or `--bash`, all
of which bypass the regex engine.

Because PCRE2 is a C library, it is behind a non-default cargo feature — builds
without it reject `--pcre2` rather than ignoring it:

```console
$ cargo build --release --features pcre2
```

### metadata sorting

`-R/--sort` accepts a compact priority sequence. Lowercase keys sort ascending;
uppercase keys sort descending.

| Keys | Field | Keys | Field |
|---|---|---|---|
| `s/S` | size | `m/M` | modified time |
| `n/N` | basename | `c/C` | changed time |
| `p/P` | full path | `a/A` | accessed time |
| `e/E` | extension | `b/B` | born/creation time |
| `t/T` | entry type | `i/I` | inode |

The modifiers `z` and `Z` enable case-insensitive and natural-number text
collation, respectively.

```console
$ fd -R M          # newest first
$ fd -R enz        # extension, then basename, case-insensitively
$ fd -R pZ         # full path with natural-number collation
```

Sorting buffers the result set until traversal finishes.

### file-extension summaries

`--summarize fext` prints counts grouped by file extension instead of printing
the matching paths:

```console
$ fd --summarize fext
$ fd -tf --summarize fext:@d-i-s
```

The option letters after `:` are `i` for case-folded extensions, `d` for
including dotfiles and `s` for ascending count. Prefix a letter with `-` to
disable it or `@` to use the platform default. Dotfiles and ascending counts
are enabled by default; case folding defaults on for macOS and Windows.

### job numbers for `-X`

`-X/--exec-batch` runs one process per batch of results. The `{#}` placeholder
expands to that process's job number, counting from 1, which gives each batch a
name of its own:

```console
$ fd -e rs --batch-size 100 -X sh -c 'check "$@" > report{#}.txt' --
```

A single `-X` usually runs one process, so `{#}` only varies once the results
are split into batches — by `--batch-size` or by the command line length limit
the operating system imposes. Numbers are unique across every process one `fd`
run spawns, so repeated `-X` options never write to the same `report1.txt`.

Unlike the path placeholders, `{#}` may appear in any number of arguments, and
it does not count as the batch's path placeholder: `-X echo {#}` still gets the
implicit `{}` appended. `-x` and `--format` reject it, since neither has batches
to number.

### output and alias conveniences

`-l/--list-details` is implemented entirely inside fd instead of invoking
`ls` or `gls`. It works with an empty `PATH`, formats timestamps like `ls`, and
can be combined with `--absolute-path` and `--sort`.

`-P`/`--no-full-path` can undo an earlier `-p`/`--full-path`, which is
useful when the fork is wrapped in a search-everything alias:

```sh
alias f='fd -H -I -i -p -M vcs_meta,package,os_meta'
```

The alias can then be adjusted per invocation with flags such as
`-P`/`--no-full-path`, `--no-hidden`, `--ignore`, `-M package-` or `-M-`.
As a convenience, a separator-free `--exact` pattern always matches against the
filename, so `f --exact Cargo.toml` does not need an explicit
`-P`/`--no-full-path`. An exact pattern containing a path separator retains
full-path matching.

Options with compact value grammars accept the literal value `help` for a
focused syntax reference:

```console
$ fd -S help
$ fd -R help
$ fd --bash help
```

This works with `-t`, `-S`, `-R`, `-x`, `-X`, `--changed-within`,
`--changed-before`, `--summarize`, `--bash`, `--prune-if` and `--exclude-if`.

Multiline diagnostics preserve their intended line breaks while unsafe control
characters remain escaped.

## building

This fork requires Rust 1.90 or newer.

```console
$ cargo build --release
```

The resulting binary is `target/release/fd`.

## license

Like upstream fd, this fork is dual-licensed under the Apache License 2.0 or
the MIT license.
