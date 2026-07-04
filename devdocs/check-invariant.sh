#!/usr/bin/env bash
# Spot-check the base-case invariant (devdocs/PLAN-F-TO-FD.md, "The
# Invariant"): build upstream fd at the merge base and the fork at HEAD, run
# both over a fixture tree for the five base invocations, and diff the
# (sorted) output. Sorting is needed because the parallel walk emits entries
# in nondeterministic order.
#
# Usage: devdocs/check-invariant.sh [<upstream-commit>]
set -euo pipefail

repo_root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
base_commit=${1:-40d8eb3}

workdir=$(mktemp -d)
cleanup() {
    git -C "$repo_root" worktree remove --force "$workdir/upstream" 2>/dev/null || true
    rm -rf "$workdir"
}
trap cleanup EXIT

echo "Building fork at HEAD..."
cargo build --quiet --manifest-path "$repo_root/Cargo.toml"
fork_bin=$repo_root/target/debug/fd

echo "Building upstream at $base_commit..."
git -C "$repo_root" worktree add --detach "$workdir/upstream" "$base_commit" >/dev/null
cargo build --quiet --manifest-path "$workdir/upstream/Cargo.toml"
upstream_bin=$workdir/upstream/target/debug/fd

# Replica of the integration-test default tree (tests/testenv/mod.rs).
tree=$workdir/tree
mkdir -p "$tree/one/two/three/directory_foo" "$tree/.git"
(
    cd "$tree"
    touch a.foo one/b.foo one/two/c.foo one/two/C.Foo2 one/two/three/d.foo \
        fdignored.foo gitignored.foo .hidden.foo "e1 e2"
    ln -s "$tree/one/two" symlink
    printf fdignored.foo >.fdignore
    printf gitignored.foo >.gitignore
)

status=0
run_case() {
    local desc=$1
    shift
    if diff \
        <(cd "$tree" && "$upstream_bin" "$@" | LC_ALL=C sort) \
        <(cd "$tree" && "$fork_bin" "$@" | LC_ALL=C sort); then
        echo "OK   fd $desc"
    else
        echo "DIFF fd $desc"
        status=1
    fi
}

run_case "" # (no arguments)
run_case "." .
run_case "-g '*'" -g '*'
run_case ". one" . one
run_case "-g '*' one" -g '*' one

exit $status
