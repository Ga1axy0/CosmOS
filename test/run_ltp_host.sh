#!/bin/sh

set -eu

LOGLEVEL="ERROR"
MODE="fast-run"
LIBC="musl"
TEST_KIND="all"
GROUPS=""
CASE_NAME=""

usage() {
    cat <<EOF
Usage: ${0##*/} [options]

Options:
  --loglevel LEVEL   Set kernel log level: ERROR, WARN, INFO, DEBUG
  --mode MODE        QEMU launch mode: fast-run, run
  --libc LIBC        Guest libc root: musl, glibc
  --group NAME ...   Run one or more LTP group names via test_ltp.sh
  --case NAME        Run one testcase binary directly from LTPROOT/testcases/bin
  --all              Run all LTP groups (default when no testcase is selected)
  -h, --help         Show this help text

Examples:
  ${0##*/} --loglevel ERROR --mode fast-run --libc musl
  ${0##*/} --group syscalls
  ${0##*/} --case readv01
EOF
}

die() {
    printf '%s\n' "$*" >&2
    exit 2
}

validate_loglevel() {
    case "$1" in
        ERROR|WARN|INFO|DEBUG) ;;
        *) die "ERROR: invalid loglevel '$1' (expected ERROR/WARN/INFO/DEBUG)" ;;
    esac
}

validate_mode() {
    case "$1" in
        fast-run|run) ;;
        *) die "ERROR: invalid mode '$1' (expected fast-run/run)" ;;
    esac
}

validate_libc() {
    case "$1" in
        musl|glibc) ;;
        *) die "ERROR: invalid libc '$1' (expected musl/glibc)" ;;
    esac
}

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
OS_DIR="$REPO_ROOT/os"

if [ ! -d "$OS_DIR" ]; then
    die "ERROR: cannot find os directory under $REPO_ROOT"
fi

while [ $# -gt 0 ]; do
    case "$1" in
        --loglevel)
            [ $# -ge 2 ] || die "ERROR: --loglevel expects a value"
            LOGLEVEL="$2"
            validate_loglevel "$LOGLEVEL"
            shift 2
            ;;
        --mode)
            [ $# -ge 2 ] || die "ERROR: --mode expects a value"
            MODE="$2"
            validate_mode "$MODE"
            shift 2
            ;;
        --libc)
            [ $# -ge 2 ] || die "ERROR: --libc expects a value"
            LIBC="$2"
            validate_libc "$LIBC"
            shift 2
            ;;
        --group)
            TEST_KIND="group"
            shift
            while [ $# -gt 0 ]; do
                case "$1" in
                    -*) break ;;
                    *)
                        GROUPS="${GROUPS:+$GROUPS }$1"
                        shift
                        ;;
                esac
            done
            ;;
        --case)
            [ $# -ge 2 ] || die "ERROR: --case expects a testcase name"
            [ -z "$GROUPS" ] || die "ERROR: --group and --case are mutually exclusive"
            TEST_KIND="case"
            CASE_NAME="$2"
            shift 2
            ;;
        --all)
            [ -z "$GROUPS" ] || die "ERROR: --all cannot be combined with --group"
            [ -z "$CASE_NAME" ] || die "ERROR: --all cannot be combined with --case"
            TEST_KIND="all"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "ERROR: unknown option '$1'"
            ;;
    esac
done

validate_loglevel "$LOGLEVEL"
validate_mode "$MODE"
validate_libc "$LIBC"

if [ "$TEST_KIND" = "group" ] && [ -z "$GROUPS" ]; then
    die "ERROR: --group requires at least one group name"
fi

if [ "$TEST_KIND" = "case" ] && [ -z "$CASE_NAME" ]; then
    die "ERROR: --case requires a testcase name"
fi

TMP_INPUT=$(mktemp "${TMPDIR:-/tmp}/run_ltp_host.XXXXXX")
cleanup() {
    rm -f "$TMP_INPUT"
}
trap cleanup EXIT INT TERM HUP

LTP_DIR="/$LIBC"
LTPROOT="$LTP_DIR/ltp"

{
    printf '%s\n' '/bin/sh'
    case "$TEST_KIND" in
        all)
            printf 'cd %s\n' "$LTP_DIR"
            printf '%s\n' './test_ltp.sh'
            ;;
        group)
            printf 'cd %s\n' "$LTP_DIR"
            printf './test_ltp.sh %s\n' "$GROUPS"
            ;;
        case)
            printf 'export LTPROOT=%s\n' "$LTPROOT"
            printf 'export PATH=$LTPROOT/testcases/bin:$LTPROOT/bin:$PATH\n'
            printf 'cd $LTPROOT/testcases/bin\n'
            printf './%s\n' "$CASE_NAME"
            ;;
    esac
    printf '%s\n' 'quit'
} > "$TMP_INPUT"

printf 'Launching LTP with mode=%s loglevel=%s libc=%s\n' "$MODE" "$LOGLEVEL" "$LIBC"

(
    cd "$OS_DIR"
    LOG="$LOGLEVEL" make "$MODE"
) < "$TMP_INPUT"

