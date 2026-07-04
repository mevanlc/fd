# Match Sets Plan

## Goal

Add named filesystem-entry match sets to `fd`, loaded from `~/.config/fd/match-sets.kdl`, and make each set usable as either an inclusion filter or an exclusion filter.

The user-facing model is:

```text
-m, --match <set[,set...]>
-M, --exclude-match <set[,set...]>
    Select named match sets from the loaded match-set registry.

--match-sets
--no-match-sets
    Enable or disable loading the well-known match-set file.
```

`-m` keeps only filesystem entries that match at least one selected include set. `-M` removes filesystem entries that match any selected exclude set. Multiple `-m`/`-M` occurrences and comma-separated set lists should compose naturally.

## Naming

Use "match sets", not "pattern sets".

The config file is:

```text
~/.config/fd/match-sets.kdl
```

The planning sketch previously named `pattern-sets.kdl` should become `match-sets.kdl` wherever it is carried forward into `fd` docs, examples, tests, or sample config.

## Proposed KDL Shape

Keep the current compact grouping syntax:

```kdl
"vcs" {
    dir name literal full {
        ".git"
        ".svn"
        ".hg"
    }
}

"metadata" {
    dir name literal full {
        "node_modules"
        ".venv"
    }

    file name literal full {
        ".DS_Store"
    }
}

"outputs" {
    dir bash {
        "${/} == target && -f CACHEDIR.TAG"
    }
}
```

The fields mean:

```text
type:    file, dir, symlink, executable, empty, socket, pipe, block-device, char-device
subject: name, path
mode:    full, sub
pattern: literal, glob, regex, bash
```

Keep short aliases available internally where they fit existing `fd` vocabulary (`f`, `d`, `l`, `x`, `e`, `s`, `p`, `b`, `c`), but document the long names in examples unless the compact form is intentionally useful.

## CLI Work

Add fields to `src/cli.rs`:

```rust
#[arg(long = "match", short = 'm', value_name = "set[,set...]", value_delimiter = ',')]
pub match_sets: Vec<String>,

#[arg(long = "exclude-match", short = 'M', value_name = "set[,set...]", value_delimiter = ',')]
pub exclude_match_sets: Vec<String>,

#[arg(long = "match-sets", overrides_with = "no_match_sets")]
pub load_match_sets: bool,

#[arg(long = "no-match-sets", overrides_with = "load_match_sets")]
pub no_match_sets: bool,
```

The initial implementation uses `--match` and `--exclude-match`; `fd --help` should keep `-m`, `-M`, `--match-sets`, and `--no-match-sets` visible.

Expected parsing behavior:

- `fd -m vcs,metadata foo`
- `fd -m vcs -m metadata foo`
- `fd -M outputs,target foo`
- Unknown set names are hard errors.
- Empty set names from malformed commas are hard errors.
- using `-m` or `-M` loads the well-known config unless `--no-match-sets` is present.
- `--match-sets` loads and validates the well-known config even if no set is selected.
- `--no-match-sets` disables loading the well-known config and makes any user-defined set unavailable.
- Built-in sets, if added later, should remain available unless an explicit future flag disables built-ins too.

## Loading

Add a new module, likely `src/match_sets.rs`, responsible for:

- locating `~/.config/fd/match-sets.kdl` via `etcetera::choose_base_strategy().config_dir().join("fd").join("match-sets.kdl")`;
- returning an empty registry if the file is absent;
- parsing KDL into a registry keyed by set name;
- validating duplicate set names, invalid type/subject/mode/pattern atoms, empty pattern groups, and unsupported node structure;
- compiling regex, glob, literal, and bash matchers once during startup;
- producing precise parse errors that include set name and enough KDL context to fix the file.

`Cargo.toml` uses the `kdl` crate. The currently compatible version is constrained by this repo's Rust MSRV.

## Runtime Model

Represent selected sets as two compiled collections in `Config`:

```rust
pub include_match_sets: Vec<CompiledMatchSet>;
pub exclude_match_sets: Vec<CompiledMatchSet>;
```

Each `CompiledMatchSet` contains one or more match clauses. A filesystem entry matches a set if any clause in that set matches. Inclusion/exclusion semantics are:

- no include sets selected: pass this stage;
- include sets selected: entry passes if it matches any include set;
- exclude sets selected: entry fails if it matches any exclude set;
- if both are selected: include check runs first, then exclude check removes entries from the included population.

This OR-across-selected-sets behavior makes `-m vcs,metadata` usable as "show entries in either class" instead of accidentally requiring impossible intersections.

## Matching Semantics

Clause evaluation should happen in `src/walk.rs` near the existing name/path, bash, and `exclude_if` filters.

Use existing behavior where possible:

- `name` subject should use the entry basename, like the default search path at `src/walk.rs`.
- `path` subject should use the normalized path form chosen for match-set semantics, preferably relative to the search root for config portability.
- `literal full` is exact equality.
- `literal sub` is substring containment.
- `glob` should use `globset`.
- `regex` should use `regex::bytes::Regex` and respect `fd` case sensitivity unless the match-set syntax later grows a per-clause override.
- `bash` should reuse `bash_cond::parse_expr` and `bash_cond::evaluate`.

For bash clauses, keep the same context rules already used by `--bash`, `--exclude-if`, and `--prune-if`: directories evaluate in their own context, files evaluate in the parent directory context.

Directory exclusion should prune descendants when a selected `-M` set matches a directory, matching `exclude_if` behavior in `src/walk.rs`. Inclusion sets should not prune non-matching directories unless a later optimization can prove no descendant could match.

## Integration Points

Implementation order:

1. Add CLI fields and help text in `src/cli.rs`; confirm `cargo run -- --help` displays `-m` and `-M` clearly.
2. Add `src/match_sets.rs` with data types, KDL loading, validation, and compile-time matcher construction.
3. Resolve selected set names in `main.rs` after `Opts::parse()` and before `construct_config`.
4. Add compiled include/exclude match sets to `Config`.
5. Evaluate include/exclude match sets in `walk.rs` before extension/type/size/time filters.
6. Add documentation examples to README/manpage after behavior settles.

## Tests

Add focused unit tests for `src/match_sets.rs`:

- parses multiple sets;
- supports comma-delimited CLI selection after Clap parsing;
- rejects duplicate set names;
- rejects unknown selected set names;
- rejects invalid atoms and empty names;
- compiles literal/glob/regex/bash clauses;
- absent well-known file is not an error.

Add integration tests in `tests/tests.rs` or the existing test harness for:

- `-m vcs` includes `.git`-class entries when hidden/ignore settings allow traversal;
- `-M metadata` excludes matching entries;
- `-m a,b` is OR, not AND;
- `-m a -M b` includes `a` then removes `b`;
- matching directory with `-M` prunes descendants;
- `--no-match-sets -m user_set` reports an unknown set;
- malformed `match-sets.kdl` reports a useful error.

Run at minimum:

```sh
cargo fmt
cargo check
cargo test
```

## Open Decisions

- Whether to keep `--match`/`--exclude-match` or rename to the more explicit `--match-set`/`--exclude-match-set` after help copy review.
- Whether to load `match-sets.kdl` for every invocation. Initial implementation loads on `-m`, `-M`, or explicit `--match-sets` to avoid breaking ordinary searches on malformed local config.
- Whether built-in sets ship in the binary now or wait until the user config mechanism lands.
- Whether `path` means relative to each search root or absolute normalized path. Recommended: relative-to-root for portable config.
- Whether regex clauses inherit global case sensitivity. Recommended: yes for consistency with normal search patterns.
