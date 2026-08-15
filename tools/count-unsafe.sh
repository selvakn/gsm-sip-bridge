#!/usr/bin/env bash
set -uo pipefail

count_unsafe() {
    local dir="$1"
    local count
    # Matches an actual `unsafe` keyword use (a block, or an fn/impl/trait
    # marked unsafe), not the bare word appearing in prose — a doc comment
    # or string mentioning "unsafe" must not fail the crate.
    count=$(grep -rnE '\bunsafe\s*[{(]|\bunsafe\s+(fn|impl|trait)\b' "$dir" 2>/dev/null | wc -l)
    echo "${count// /}"
}

# Crates required to be zero-`unsafe`. siptest joins gsm-sip-bridge here
# (specs/037-siptest-softphone) — this script used to hardcode only the
# latter, so a new crate was never actually checked.
ZERO_UNSAFE_DIRS=("gsm-sip-bridge/src/" "siptest/src/")
SAFE_UNSAFE=$(count_unsafe "pjsua-safe/src/")
SAFE_TOTAL=$(find pjsua-safe/src/ -name '*.rs' -exec cat {} + 2>/dev/null | wc -l)
SAFE_TOTAL="${SAFE_TOTAL// /}"
: "${SAFE_TOTAL:=1}"

echo "=== Unsafe Block Count ==="
FAILED=0
for dir in "${ZERO_UNSAFE_DIRS[@]}"; do
    count=$(count_unsafe "$dir")
    echo "  ${dir}: ${count} unsafe blocks"
    if [ "${count}" -gt 0 ]; then
        echo "FAIL: ${dir} must contain zero unsafe blocks"
        FAILED=1
    fi
done
echo "  pjsua-safe/src:     ${SAFE_UNSAFE} unsafe blocks (${SAFE_TOTAL} total lines)"

if [ "${FAILED}" -eq 1 ]; then
    exit 1
fi

if [ "${SAFE_TOTAL}" -gt 0 ]; then
    RATIO=$(echo "scale=2; ${SAFE_UNSAFE} * 100 / ${SAFE_TOTAL}" | bc 2>/dev/null || echo "0")
    echo "  pjsua-safe ratio: ${RATIO}% (threshold: <5%)"
fi

echo "PASS"
exit 0
