#!/usr/bin/env bash
# T5.1.2 H2 (2026-09-26 review) — J23b automation.
#
# `ai::ports::MODEL_ACCESS` is a compile-time constant: which of the two
# `domain-os*.yml` files got baked into a given `sin90` binary (official vs.
# `--features remote-allowed-manifest`, i.e. test package B) decides its
# value. `ai::ports::manifest_tests` already pins that fact WITHIN one build
# (`manifest_official_is_local_only` / `model_access_is_remote_allowed_when_
# the_test_feature_is_on`), but neither of those can catch a genuinely wrong
# PACKAGE — e.g. a `remote-allowed-manifest` binary shipped next to the
# OFFICIAL `domain-os.yml`, or vice versa — since that mismatch only exists
# BETWEEN two separately-built artifacts, not inside either one alone.
#
# This script builds both variants, runs `sin90 print-model-access` on each,
# and cross-checks the result against the `model_access` value parsed out of
# the CORRESPONDING `domain-os*.yml` file on disk. It also runs one
# deliberate CROSS mismatch (package B's binary compared against package A's
# yml) as a positive control — that comparison MUST fail, proving this
# script can actually detect a real mismatch, not just always report "ok".
#
# Usage: scripts/check-model-access.sh (no arguments; run from anywhere,
# it cds to the repo root itself).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Mirrors `ai::ports::parse_model_access`'s own rule exactly: the first
# `model_access:` line's trimmed value; absent entirely defaults to
# `local_only` (the kernel's own default, ME4-S2 §2.1).
parse_model_access() {
    local file="$1" value
    value=$(grep -m1 '^[[:space:]]*model_access:' "$file" 2>/dev/null \
        | sed -E 's/^[[:space:]]*model_access:[[:space:]]*//' \
        | tr -d '[:space:]')
    if [ -z "$value" ]; then
        echo "local_only"
    else
        echo "$value"
    fi
}

# Builds AND runs `sin90 print-model-access` via `cargo run` — not a
# hand-assembled `./target/debug/sin90` path (L-1, 2026-09-26 review round
# 2): `cargo run` resolves the binary itself, so this keeps working
# regardless of profile/target-dir overrides (a custom `CARGO_TARGET_DIR`,
# `--release`, cross-compilation, ...) instead of silently reading a stale or
# nonexistent binary at a hardcoded path. `"$@"` (an array, not a string) so
# an empty feature list is genuinely zero extra cargo arguments, not a stray
# empty-string token; the `--` before `print-model-access` is what stops
# cargo from trying to interpret it as one of ITS OWN flags.
build_and_print_model_access() {
    cargo run --quiet --bin sin90 "$@" -- print-model-access
}

echo "== package A (official, default features) ==" >&2
a_bin=$(build_and_print_model_access)
a_yml=$(parse_model_access domain-os.yml)
echo "binary: $a_bin | domain-os.yml: $a_yml" >&2
if [ "$a_bin" != "$a_yml" ]; then
    echo "FAIL: package A — binary says '$a_bin' but domain-os.yml says '$a_yml'" >&2
    exit 1
fi

echo "== package B (--features remote-allowed-manifest) ==" >&2
b_bin=$(build_and_print_model_access --features remote-allowed-manifest)
b_yml=$(parse_model_access domain-os.remote-allowed.yml)
echo "binary: $b_bin | domain-os.remote-allowed.yml: $b_yml" >&2
if [ "$b_bin" != "$b_yml" ]; then
    echo "FAIL: package B — binary says '$b_bin' but domain-os.remote-allowed.yml says '$b_yml'" >&2
    exit 1
fi

echo "== positive control: package B's binary against package A's yml (must mismatch) ==" >&2
if [ "$b_bin" == "$a_yml" ]; then
    echo "FAIL: positive control did not trip — package B's binary ('$b_bin') equals" >&2
    echo "package A's yml value ('$a_yml'); this script cannot actually detect a real" >&2
    echo "binary/manifest mismatch" >&2
    exit 1
fi
echo "positive control OK: package B binary ('$b_bin') != package A yml ('$a_yml')" >&2

echo "all package/manifest pairings agree; positive control confirmed sensitive" >&2
