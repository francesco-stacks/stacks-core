#!/usr/bin/env bash
# Ratchet check for RuntimeCheckErrorKind::Unreachable boilerplate.
#
# Counts Unreachable(...) constructors in clarity/src/vm/functions/ production
# code (excluding args.rs, which is the sanctioned home for these errors, and
# excluding trailing #[cfg(test)] modules). The count must not exceed the
# checked-in baseline; new code should use the typed extraction primitives in
# clarity/src/vm/functions/args.rs instead. Ratchet the baseline DOWN as more
# call sites are converted; never up.
set -euo pipefail
cd "$(dirname "$0")/../.."

baseline_file="contrib/tools/unreachable-ratchet-baseline.txt"
baseline=$(<"$baseline_file")

count=0
for f in $(find clarity/src/vm/functions -name '*.rs' | sort); do
    # args.rs is the sanctioned home for these errors; *_differential.rs and
    # bitcoin_madhouse.rs are test-only modules (#[cfg(test)] on their mod
    # declarations), including verbatim legacy copies kept for review.
    case "$(basename "$f")" in
        args.rs|*_differential.rs|bitcoin_madhouse.rs) continue ;;
    esac
    # Stop counting only at a test MODULE BLOCK (`#[cfg(test)]` followed by
    # `mod name {`); `#[cfg(test)] mod name;` declarations must not end the scan.
    n=$(awk '/^#\[cfg\(test\)\]/{ if ((getline nl) > 0) { if (nl ~ /^mod .*\{/) exit; if (nl ~ /RuntimeCheckErrorKind::Unreachable\(/) c++ } next } /RuntimeCheckErrorKind::Unreachable\(/{c++} END{print c+0}' "$f")
    count=$((count + n))
done

echo "Unreachable constructors (production, outside args.rs): $count (baseline: $baseline)"
if (( count > baseline )); then
    echo "ERROR: count exceeds the baseline. Use the typed extraction primitives in" >&2
    echo "clarity/src/vm/functions/args.rs instead of raw Unreachable boilerplate." >&2
    exit 1
elif (( count < baseline )); then
    echo "ERROR: count is below the baseline — ratchet it down by writing $count to $baseline_file" >&2
    exit 1
fi
