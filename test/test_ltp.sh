#!/bin/sh

set -u

LTPROOT="${LTPROOT:-/musl/ltp}"
RUNTST_DIR="$LTPROOT/runtest"
BINDIR="$LTPROOT/testcases/bin"
PANBIN="$LTPROOT/bin"

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0
TOTAL_COUNT=0

usage() {
    cat <<EOF
Usage: ${0##*/} [--list-groups] [--all] [group ...]

Options:
  --list-groups   Print available LTP runtest group names and exit
  --all           Run all runtest groups
  -h, --help      Show this help text

Arguments:
  group           One or more group names from $RUNTST_DIR

Behavior:
  - With no group arguments, all groups are executed.
  - Exit code 32 from an LTP testcase is reported as SKIP/TCONF.

Examples:
  ${0##*/} --list-groups
  ${0##*/} controllers fs mm
  ${0##*/} --all
EOF
}

list_groups() {
    for group_path in "$RUNTST_DIR"/*; do
        [ -f "$group_path" ] || continue
        basename "$group_path"
    done | sort
}

require_layout() {
    if [ ! -d "$RUNTST_DIR" ] || [ ! -d "$BINDIR" ]; then
        echo "FATAL: LTP layout not found under $LTPROOT" >&2
        exit 1
    fi
}

run_case() {
    line="$1"

    set -- $line
    tcid="$1"
    shift

    echo "RUN LTP CASE $tcid"
    (
        cd "$BINDIR" || exit 127
        "$@"
    )
    ret=$?

    TOTAL_COUNT=$((TOTAL_COUNT + 1))

    case "$ret" in
        0)
            PASS_COUNT=$((PASS_COUNT + 1))
            echo "PASS LTP CASE $tcid"
            ;;
        32)
            SKIP_COUNT=$((SKIP_COUNT + 1))
            echo "SKIP LTP CASE $tcid : $ret"
            ;;
        *)
            FAIL_COUNT=$((FAIL_COUNT + 1))
            echo "FAIL LTP CASE $tcid : $ret"
            ;;
    esac
}

run_group() {
    group_name="$1"
    group_path="$RUNTST_DIR/$group_name"

    if [ ! -f "$group_path" ]; then
        echo "ERROR: unknown LTP group '$group_name'" >&2
        return 1
    fi

    echo
    echo "==== RUN LTP GROUP $group_name ===="

    while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in
            ""|\#*)
                continue
                ;;
        esac
        run_case "$line"
    done < "$group_path"
}

print_summary() {
    echo
    echo "==== LTP RUNNER SUMMARY ===="
    echo "total  $TOTAL_COUNT"
    echo "pass   $PASS_COUNT"
    echo "skip   $SKIP_COUNT"
    echo "fail   $FAIL_COUNT"
}

require_layout

export LTPROOT
export PATH="$BINDIR:$PANBIN:$PATH"

LIST_ONLY=0
RUN_ALL=0
SELECTED_GROUPS=""

while [ $# -gt 0 ]; do
    case "$1" in
        --list-groups)
            LIST_ONLY=1
            shift
            ;;
        --all)
            RUN_ALL=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            break
            ;;
        -*)
            echo "ERROR: unknown option '$1'" >&2
            usage >&2
            exit 2
            ;;
        *)
            if [ -z "$SELECTED_GROUPS" ]; then
                SELECTED_GROUPS="$1"
            else
                SELECTED_GROUPS="$SELECTED_GROUPS $1"
            fi
            shift
            ;;
    esac
done

if [ $# -gt 0 ]; then
    if [ -z "$SELECTED_GROUPS" ]; then
        SELECTED_GROUPS="$*"
    else
        SELECTED_GROUPS="$SELECTED_GROUPS $*"
    fi
fi

if [ "$LIST_ONLY" -eq 1 ]; then
    list_groups
    exit 0
fi

if [ "$RUN_ALL" -eq 1 ] || [ -z "$SELECTED_GROUPS" ]; then
    SELECTED_GROUPS="$(list_groups | tr '\n' ' ')"
fi

echo "#### OS COMP TEST GROUP START ltp ####"

RUNNER_RC=0
for group_name in $SELECTED_GROUPS; do
    if ! run_group "$group_name"; then
        RUNNER_RC=1
    fi
done

print_summary
echo "#### OS COMP TEST GROUP END ltp ####"

if [ "$RUNNER_RC" -ne 0 ] || [ "$FAIL_COUNT" -ne 0 ]; then
    exit 1
fi

exit 0
