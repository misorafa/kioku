#!/bin/sh
# Checks docs/INDEX.md (SPEC-M3.2 §4): every `SPEC-Mx.y §N[.M…]` reference must name an
# existing docs/SPEC-Mx.y.md that has a heading `#… N. …` / `#… N.M …`, and every SPEC
# file the index mentions must exist. Prints each broken reference; exit 1 if any.
# POSIX sh, grep and sed only. Run from anywhere: `sh scripts/check-docs.sh`.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
docs="$root/docs"
index="${KIOKU_DOCS_INDEX:-$docs/INDEX.md}"
status=0

if [ ! -f "$index" ]; then
    echo "check-docs: $index does not exist" >&2
    exit 1
fi

# Every SPEC file mentioned at all.
for spec in $(grep -o 'SPEC-M[0-9][0-9]*\(\.[0-9][0-9]*\)\{0,1\}' "$index" | sort -u); do
    if [ ! -f "$docs/$spec.md" ]; then
        echo "check-docs: $spec: docs/$spec.md does not exist" >&2
        status=1
    fi
done

# Every section reference. `SPEC-M2.2 §7.3a` → file SPEC-M2.2, section 7.3a.
refs=$(grep -o 'SPEC-M[0-9][0-9]*\(\.[0-9][0-9]*\)\{0,1\} §[0-9][0-9]*\(\.[0-9][0-9]*\)*[a-z]\{0,1\}' "$index" \
    | sed 's/ §/|/' | sort -u)
count=0
for ref in $refs; do
    count=$((count + 1))
    spec=${ref%%|*}
    sec=${ref#*|}
    file="$docs/$spec.md"
    [ -f "$file" ] || continue # reported above
    esc=$(printf '%s' "$sec" | sed 's/\./\\./g')
    # `## 3. Title` for §3, `### 3.1 Title` for §3.1, `### 7.3a Title` for §7.3a.
    if ! grep -Eq "^#+ ${esc}\.? " "$file"; then
        echo "check-docs: $spec §$sec: no such section heading in docs/$spec.md" >&2
        status=1
    fi
done

if [ "$status" -eq 0 ]; then
    echo "check-docs: $count section references in $(basename "$index") are valid"
fi
exit "$status"
