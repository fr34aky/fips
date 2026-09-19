#!/bin/bash
# Shared helpers for building test images and starting test containers.
#
# Source this file to get dump_output() and run_quiet():
#   source "$SCRIPT_DIR/../lib/image-build.sh"
#   echo "$dockerfile" | run_quiet "docker build -t $tag" \
#       docker build -t "$tag" -f - "$REPO_ROOT"
#
# A build or a container start that fails for a reason outside the project,
# such as a registry timeout, is indistinguishable from one the project caused
# unless its output survives. These helpers keep a command quiet when it
# succeeds and print everything it said when it fails.

# Emit a captured output file to stderr, delimited and labelled with the
# command it came from. Callers use this only on failure: a command that
# succeeds leaves no trace, so the suite stays quiet when it is green.
dump_output() {
    local label="$1" file="$2"
    {
        echo "  --- $label failed; captured output follows ---"
        if [ -s "$file" ]; then
            cat "$file"
        else
            echo "  (no output)"
        fi
        echo "  --- end captured output ---"
    } >&2
}

# Run a command with both streams captured. Discard the capture on success;
# on failure emit it, so the reason a build or a container start died is not
# thrown away. Stdin is inherited, so a caller may pipe into it.
run_quiet() {
    local label="$1"
    shift
    local out rc=0
    out=$(mktemp)
    "$@" >"$out" 2>&1 || rc=$?
    [ "$rc" -eq 0 ] || dump_output "$label" "$out"
    rm -f "$out"
    return "$rc"
}
