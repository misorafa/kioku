#!/bin/sh
# Tests for the release and maintenance scripts (SPEC-M3.3 §1, §4). POSIX sh:
#
#   sh scripts/test-scripts.sh
#
# - render-homebrew-formula.sh: golden test against scripts/fixtures/homebrew/ (a fake
#   SHA256SUMS of a v1.2.3 release), plus its refusals.
# - probe-agents.sh --check: identical fixtures report no diff; a changed key set is
#   reported (added and removed keys, exit 1); `_meta` is ignored; a `<event>_<variant>`
#   fixture falls back to the fresh `<event>` capture. Runs on copies in a temp dir and
#   never touches ~/.kioku.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d 2>/dev/null || mktemp -d -t kioku-test)
trap 'rm -rf "$WORK"' EXIT

PASSED=0
FAILED=0
OUT=""
RC=0
check() { # <name> <condition result 0/1>
    if [ "$2" = 0 ]; then
        PASSED=$((PASSED + 1))
        printf 'ok   %s\n' "$1"
    else
        FAILED=$((FAILED + 1))
        printf 'FAIL %s\n' "$1"
        printf '%s\n' "$OUT" | sed 's/^/     | /'
    fi
}
run() { # command... → OUT, RC
    RC=0
    OUT=$("$@" 2>&1) || RC=$?
}
has() { printf '%s\n' "$OUT" | grep -F -- "$1" >/dev/null; }

# ---------------------------------------------------------------- Homebrew formula

FIX="$ROOT/scripts/fixtures/homebrew"
run sh "$ROOT/scripts/render-homebrew-formula.sh" v1.2.3 "$FIX/SHA256SUMS"
printf '%s\n' "$OUT" >"$WORK/kioku.rb"
if [ "$RC" = 0 ] && cmp -s "$WORK/kioku.rb" "$FIX/kioku.rb.golden"; then r=0; else r=1; fi
check 'formula: renders the golden file (tag, version, four musl/darwin checksums)' "$r"
if has '@'; then r=1; else r=0; fi
check 'formula: no placeholder left' "$r"

grep -v 'x86_64-unknown-linux-musl' "$FIX/SHA256SUMS" >"$WORK/SUMS-missing"
run sh "$ROOT/scripts/render-homebrew-formula.sh" v1.2.3 "$WORK/SUMS-missing"
if [ "$RC" != 0 ] && has 'no checksum for kioku-v1.2.3-x86_64-unknown-linux-musl.tar.gz'; then r=0; else r=1; fi
check 'formula: a missing checksum fails' "$r"

sed 's/^1111/zzzz/' "$FIX/SHA256SUMS" >"$WORK/SUMS-bad"
run sh "$ROOT/scripts/render-homebrew-formula.sh" v1.2.3 "$WORK/SUMS-bad"
if [ "$RC" != 0 ] && has 'not lowercase hex'; then r=0; else r=1; fi
check 'formula: a malformed checksum fails' "$r"

run sh "$ROOT/scripts/render-homebrew-formula.sh" 'main' "$FIX/SHA256SUMS"
if [ "$RC" != 0 ] && has 'tag must look like v1.2.3'; then r=0; else r=1; fi
check 'formula: a non-release tag fails' "$r"

if command -v ruby >/dev/null 2>&1; then
    run ruby -c "$WORK/kioku.rb"
    if [ "$RC" = 0 ]; then r=0; else r=1; fi
    check 'formula: valid Ruby (ruby -c)' "$r"
fi

# ---------------------------------------------------------------- probe-agents.sh --check

FX="$WORK/fixtures"
FRESH="$WORK/fresh"
mkdir -p "$FX/codex" "$FX/windows/codex" "$FX/legacy/gemini-cli" "$FRESH/codex"
cat >"$FX/codex/stop.captured.json" <<'EOF'
{"_meta": {"captured_with": "codex-cli 0.46.0"}, "session_id": "s", "cwd": "/r", "hook_event_name": "Stop", "last_assistant_message": "完了しました"}
EOF
cat >"$FX/codex/post_tool_use_bash.captured.json" <<'EOF'
{"_meta": {"captured_with": "unknown"}, "session_id": "s", "tool_name": "Bash", "tool_input": {"command": "ls"}}
EOF
cat >"$FX/windows/codex/stop.captured.json" <<'EOF'
{"session_id": "s"}
EOF
cat >"$FX/legacy/gemini-cli/stop.captured.json" <<'EOF'
{"session_id": "s"}
EOF
# Fresh captures as `kioku hook-dump extract` writes them: no _meta, other values.
cat >"$FRESH/codex/stop.captured.json" <<'EOF'
{"session_id": "x", "cwd": "/other", "hook_event_name": "Stop", "last_assistant_message": "別の返答"}
EOF
cat >"$FRESH/codex/post_tool_use.captured.json" <<'EOF'
{"session_id": "x", "tool_name": "Bash", "tool_input": {"command": "pwd", "timeout": 5}}
EOF

run env KIOKU_FIXTURES_DIR="$FX" sh "$ROOT/scripts/probe-agents.sh" --check "$FRESH"
if [ "$RC" = 0 ] && has 'ok   codex/stop.captured.json' && has 'ok   codex/post_tool_use_bash.captured.json' &&
    has '2 fixture(s) compared, 0 with a changed key set' && ! has 'DIFF'; then r=0; else r=1; fi
check 'probe --check: identical key sets (values, _meta, nested keys ignored) report no diff' "$r"
if has 'windows/codex/stop.captured.json: not in the fresh capture' && ! has 'legacy'; then r=0; else r=1; fi
check 'probe --check: uncaptured fixtures are listed, legacy ones skipped' "$r"

cat >"$FRESH/codex/stop.captured.json" <<'EOF'
{"session_id": "x", "cwd": "/other", "hook_event_name": "Stop", "turn_id": "t1", "stop_hook_active": false}
EOF
run env KIOKU_FIXTURES_DIR="$FX" sh "$ROOT/scripts/probe-agents.sh" --check "$FRESH"
if [ "$RC" = 1 ] && has 'DIFF codex/stop.captured.json' && has '     + stop_hook_active' &&
    has '     + turn_id' && has '     - last_assistant_message' &&
    has '2 fixture(s) compared, 1 with a changed key set'; then r=0; else r=1; fi
check 'probe --check: a changed key set is reported (added / removed keys, exit 1)' "$r"

run env KIOKU_FIXTURES_DIR="$FX" HOME="$WORK/nohome" sh "$ROOT/scripts/probe-agents.sh" --check
if [ "$RC" = 1 ] && has 'no fresh capture directory'; then r=0; else r=1; fi
check 'probe --check: no capture directory is a clear error' "$r"

printf '\npassed: %s, failed: %s\n' "$PASSED" "$FAILED"
[ "$FAILED" = 0 ]
