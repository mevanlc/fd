#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

COMMAND="all"
BUILD=1
CORPUS_DIR="${FD_BASH_BENCH_CORPUS:-$REPO_ROOT/target/fd-bash-bench-corpus}"
CONFIG_DIR="${FD_BASH_BENCH_CONFIG:-$REPO_ROOT/target/fd-bash-bench-config}"
FD_BIN="${FD_BASH_BENCH_FD:-$REPO_ROOT/target/release/fd}"
RUNS="${FD_BASH_BENCH_RUNS:-10}"
WARMUP="${FD_BASH_BENCH_WARMUP:-3}"
FANOUT="${FD_BASH_BENCH_FANOUT:-80}"
FILES_PER_DIR="${FD_BASH_BENCH_FILES_PER_DIR:-8}"

usage() {
    cat <<'USAGE'
Usage: devdocs/bench-fd-bash.sh [options] [all|generate|hash|bench|help]

Generate a deterministic fd --bash benchmark corpus and run the initial
optimization benchmark matrix from devdocs/PLAN-OPTIMIZE-FD--BASH.md.

Options:
  --no-build              Do not run cargo build --release --locked.
  --corpus PATH           Corpus directory. Defaults to target/fd-bash-bench-corpus.
  --config PATH           XDG config directory for generated matchsets.
                          Defaults to target/fd-bash-bench-config.
  --fd PATH               fd binary. Defaults to target/release/fd.
  --runs N                Benchmark runs. Defaults to 10.
  --warmup N              Hyperfine warmup runs. Defaults to 3.
  --fanout N              Number of generated project directories. Defaults to 80.
  --files-per-dir N       Number of files per generated source directory. Defaults to 8.
  -h, --help              Show this help.

Environment defaults:
  FD_BASH_BENCH_CORPUS
  FD_BASH_BENCH_CONFIG
  FD_BASH_BENCH_FD
  FD_BASH_BENCH_RUNS
  FD_BASH_BENCH_WARMUP
  FD_BASH_BENCH_FANOUT
  FD_BASH_BENCH_FILES_PER_DIR
USAGE
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            all|generate|hash|bench|help)
                COMMAND="$1"
                shift
                ;;
            --no-build)
                BUILD=0
                shift
                ;;
            --corpus)
                [[ $# -ge 2 ]] || die "--corpus requires a path"
                CORPUS_DIR="$2"
                shift 2
                ;;
            --fd)
                [[ $# -ge 2 ]] || die "--fd requires a path"
                FD_BIN="$2"
                shift 2
                ;;
            --config)
                [[ $# -ge 2 ]] || die "--config requires a path"
                CONFIG_DIR="$2"
                shift 2
                ;;
            --runs)
                [[ $# -ge 2 ]] || die "--runs requires a value"
                RUNS="$2"
                shift 2
                ;;
            --warmup)
                [[ $# -ge 2 ]] || die "--warmup requires a value"
                WARMUP="$2"
                shift 2
                ;;
            --fanout)
                [[ $# -ge 2 ]] || die "--fanout requires a value"
                FANOUT="$2"
                shift 2
                ;;
            --files-per-dir)
                [[ $# -ge 2 ]] || die "--files-per-dir requires a value"
                FILES_PER_DIR="$2"
                shift 2
                ;;
            -h|--help)
                COMMAND="help"
                shift
                ;;
            *)
                die "unknown argument: $1"
                ;;
        esac
    done
}

require_positive_int() {
    local name="$1"
    local value="$2"
    [[ "$value" =~ ^[0-9]+$ ]] || die "$name must be a non-negative integer"
}

validate_numbers() {
    require_positive_int "--runs" "$RUNS"
    require_positive_int "--warmup" "$WARMUP"
    require_positive_int "--fanout" "$FANOUT"
    require_positive_int "--files-per-dir" "$FILES_PER_DIR"
    (( RUNS > 0 )) || die "--runs must be greater than zero"
    (( FANOUT > 0 )) || die "--fanout must be greater than zero"
    (( FILES_PER_DIR > 0 )) || die "--files-per-dir must be greater than zero"
}

ensure_fd() {
    if (( BUILD )); then
        (cd "$REPO_ROOT" && cargo build --release --locked)
    fi

    [[ -x "$FD_BIN" ]] || die "fd binary is not executable: $FD_BIN"
}

generate_corpus() {
    rm -rf "$CORPUS_DIR"
    rm -rf "$CONFIG_DIR"
    mkdir -p "$CORPUS_DIR"
    mkdir -p "$CONFIG_DIR/fd"

    local i j top previous
    for ((i = 0; i < FANOUT; i++)); do
        top="$CORPUS_DIR/project-$i"
        mkdir -p \
            "$top/src" \
            "$top/lib/src" \
            "$top/docs" \
            "$top/target" \
            "$top/.hidden/src" \
            "$top/vendor/package-$i"

        printf 'Signature: 8a477f597d28d172789f06886806bc55\n' > "$top/target/CACHEDIR.TAG"
        printf '# project %s\n' "$i" > "$top/docs/readme-$i.md"
        printf 'hidden %s\n' "$i" > "$top/.hidden/src/hidden-$i.rs"

        for ((j = 0; j < FILES_PER_DIR; j++)); do
            printf 'pub fn item_%s_%s() {}\n' "$i" "$j" > "$top/src/file-$j.rs"
            printf 'pub fn lib_%s_%s() {}\n' "$i" "$j" > "$top/lib/src/lib-$j.rs"
            printf 'artifact %s %s\n' "$i" "$j" > "$top/target/artifact-$j.bin"
            printf 'vendored %s %s\n' "$i" "$j" > "$top/vendor/package-$i/file-$j.txt"
        done

        if (( i > 0 )); then
            previous="../project-$((i - 1))/src"
            ln -s "$previous" "$top/linked-src" 2>/dev/null || true
        fi
    done

    cat > "$CONFIG_DIR/fd/matchsets.kdl" <<'KDL'
"bench-src" {
    dir bash {
        "${/} = src"
    }
}
KDL

    printf 'generated corpus: %s\n' "$CORPUS_DIR"
    printf 'generated config: %s\n' "$CONFIG_DIR"
}

hasher_name() {
    if which md5sum >/dev/null 2>&1; then
        printf 'md5sum\n'
    elif which md5 >/dev/null 2>&1; then
        printf 'md5\n'
    elif which shasum >/dev/null 2>&1; then
        printf 'shasum -a 256\n'
    else
        die "need md5sum, md5, or shasum on PATH"
    fi
}

hash_stream() {
    local tool="$1"
    case "$tool" in
        md5sum)
            md5sum | awk '{ print $1 }'
            ;;
        md5)
            md5 | awk '{ print $NF }'
            ;;
        "shasum -a 256")
            shasum -a 256 | awk '{ print $1 }'
            ;;
        *)
            die "unsupported hash tool: $tool"
            ;;
    esac
}

hash_sorted_output() {
    local tool="$1"
    shift
    "$@" | sort | hash_stream "$tool"
}

run_hashes() {
    [[ -d "$CORPUS_DIR" ]] || die "corpus does not exist: $CORPUS_DIR"

    local tool native bash_eq bash_eq2 bash_glob bash_regex file_test exclude_if matchset
    tool=$(hasher_name)

    native=$(hash_sorted_output "$tool" "$FD_BIN" -g src "$CORPUS_DIR")
    bash_eq=$(hash_sorted_output "$tool" "$FD_BIN" --bash -- '${/} = src' "$CORPUS_DIR")
    bash_eq2=$(hash_sorted_output "$tool" "$FD_BIN" --bash -- '${/} == src' "$CORPUS_DIR")
    bash_glob=$(hash_sorted_output "$tool" "$FD_BIN" --bash -- '${/} == *.rs' "$CORPUS_DIR")
    bash_regex=$(hash_sorted_output "$tool" "$FD_BIN" --bash -- '${} =~ project-[0-9]+/src/' "$CORPUS_DIR")
    file_test=$(hash_sorted_output "$tool" "$FD_BIN" --bash -- '-f ${}' "$CORPUS_DIR")
    exclude_if=$(hash_sorted_output "$tool" "$FD_BIN" . "$CORPUS_DIR" --exclude-if '${/} = target')
    matchset=$(hash_sorted_output "$tool" env XDG_CONFIG_HOME="$CONFIG_DIR" "$FD_BIN" --include-matchsets bench-src . "$CORPUS_DIR")

    printf '%-24s %s\n' "native -g src" "$native"
    printf '%-24s %s\n' "bash name =" "$bash_eq"
    printf '%-24s %s\n' "bash name ==" "$bash_eq2"
    printf '%-24s %s\n' "bash glob" "$bash_glob"
    printf '%-24s %s\n' "bash regex" "$bash_regex"
    printf '%-24s %s\n' "bash -f current" "$file_test"
    printf '%-24s %s\n' "exclude-if target" "$exclude_if"
    printf '%-24s %s\n' "matchset bash" "$matchset"

    [[ "$native" == "$bash_eq" ]] || die "native -g src and bash '${/} = src' hashes differ"
    [[ "$native" == "$bash_eq2" ]] || die "native -g src and bash '${/} == src' hashes differ"
    [[ "$native" == "$matchset" ]] || die "native -g src and matchset bash hashes differ"
}

quote_arg() {
    printf '%q' "$1"
}

build_bench_commands() {
    local fd_q corpus_q config_q
    fd_q=$(quote_arg "$FD_BIN")
    corpus_q=$(quote_arg "$CORPUS_DIR")
    config_q=$(quote_arg "$CONFIG_DIR")

    BENCH_LABELS=(
        "native-glob"
        "bash-name-eq"
        "bash-name-eqeq"
        "bash-name-glob"
        "bash-path-regex"
        "bash-file-test"
        "bash-mixed"
        "exclude-if-target"
        "matchset-bash"
        "native-glob-t1"
        "bash-name-eq-t1"
    )

    BENCH_COMMANDS=(
        "$fd_q -g src $corpus_q > /dev/null"
        "$fd_q --bash -- '\${/} = src' $corpus_q > /dev/null"
        "$fd_q --bash -- '\${/} == src' $corpus_q > /dev/null"
        "$fd_q --bash -- '\${/} == *.rs' $corpus_q > /dev/null"
        "$fd_q --bash -- '\${} =~ project-[0-9]+/src/' $corpus_q > /dev/null"
        "$fd_q --bash -- '-f \${}' $corpus_q > /dev/null"
        "$fd_q --bash -- '\${/} == *.rs && -f \${}' $corpus_q > /dev/null"
        "$fd_q . $corpus_q --exclude-if '\${/} = target' > /dev/null"
        "env XDG_CONFIG_HOME=$config_q $fd_q --include-matchsets bench-src . $corpus_q > /dev/null"
        "$fd_q --threads 1 -g src $corpus_q > /dev/null"
        "$fd_q --threads 1 --bash -- '\${/} = src' $corpus_q > /dev/null"
    )
}

median() {
    local count="$#"
    printf '%s\n' "$@" |
        sort -n |
        awk -v n="$count" '
            { values[NR] = $1 }
            END {
                if (n % 2 == 1) {
                    print values[(n + 1) / 2]
                } else {
                    print (values[n / 2] + values[n / 2 + 1]) / 2
                }
            }
        '
}

run_with_time() {
    printf 'hyperfine not found; using /usr/bin/time -p fallback\n'

    local index label command output real user sys
    local reals users syss cpus

    for index in "${!BENCH_LABELS[@]}"; do
        label="${BENCH_LABELS[$index]}"
        command="${BENCH_COMMANDS[$index]}"
        reals=()
        users=()
        syss=()
        cpus=()

        for ((run = 1; run <= RUNS; run++)); do
            output=$(/usr/bin/time -p bash -lc "$command" 2>&1)
            real=$(awk '$1 == "real" { print $2 }' <<<"$output")
            user=$(awk '$1 == "user" { print $2 }' <<<"$output")
            sys=$(awk '$1 == "sys" { print $2 }' <<<"$output")
            [[ -n "$real" && -n "$user" && -n "$sys" ]] || die "could not parse time output for $label"

            reals+=("$real")
            users+=("$user")
            syss+=("$sys")
            cpus+=("$(awk -v u="$user" -v s="$sys" 'BEGIN { print u + s }')")
        done

        printf '%-24s real=%ss user=%ss sys=%ss user+sys=%ss runs=%s\n' \
            "$label" \
            "$(median "${reals[@]}")" \
            "$(median "${users[@]}")" \
            "$(median "${syss[@]}")" \
            "$(median "${cpus[@]}")" \
            "$RUNS"
    done
}

run_benchmarks() {
    [[ -d "$CORPUS_DIR" ]] || die "corpus does not exist: $CORPUS_DIR"
    build_bench_commands

    local hyperfine_path
    hyperfine_path=$(which hyperfine 2>/dev/null || true)

    if [[ -n "$hyperfine_path" ]]; then
        "$hyperfine_path" --warmup "$WARMUP" --runs "$RUNS" "${BENCH_COMMANDS[@]}"
    else
        run_with_time
    fi
}

main() {
    parse_args "$@"
    validate_numbers

    case "$COMMAND" in
        help)
            usage
            ;;
        generate)
            generate_corpus
            ;;
        hash)
            ensure_fd
            run_hashes
            ;;
        bench)
            ensure_fd
            run_benchmarks
            ;;
        all)
            ensure_fd
            generate_corpus
            run_hashes
            run_benchmarks
            ;;
        *)
            die "unknown command: $COMMAND"
            ;;
    esac
}

main "$@"
