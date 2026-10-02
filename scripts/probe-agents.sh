#!/bin/sh
# probe-agents.sh — capture real hook payloads from every installed coding agent.
#
# Runs one short, non-interactive session per agent (Codex CLI, Cursor CLI, Gemini CLI,
# Antigravity CLI)
# inside a throwaway git repo with `[client] hook_dump = true`, then restores the
# setting. Afterwards ~/.kioku/logs/hook-dump.jsonl holds the raw payloads
# (tokens and secret-looking env values are redacted at write time).
#
# Usage:  sh scripts/probe-agents.sh [codex|cursor|gemini|antigravity ...]   (default: all found)
#         sh scripts/probe-agents.sh --check [<fresh capture dir>]   (monthly; docs/INDEX.md)
# (gemini = Gemini CLI, legacy: kept working, no new fixtures; SPEC-M3.3 §4)
set -eu

# --check [<fresh dir>] (SPEC-M3.3 §4): compare the schema (top-level key set, `_meta`
# ignored) of every `*.captured.json` fixture with the same file in a fresh capture — what
# `kioku hook-dump extract <agent> <event>` writes to ~/.kioku/captures/<date>/<agent>/ —
# and print the added / removed keys. A fixture named `<event>_<variant>` (post_tool_use_bash)
# falls back to the fresh `<event>` file. Default fresh dir: the newest ~/.kioku/captures/*.
# Exit 0 = no schema change, 1 = a change (or a usage error). Needs python3; writes nothing.
# KIOKU_FIXTURES_DIR overrides the fixture root (tests).
if [ "${1:-}" = "--check" ]; then
  root=$(cd "$(dirname "$0")/.." && pwd)
  fixtures="${KIOKU_FIXTURES_DIR:-$root/crates/kioku-cli/tests/fixtures}"
  fresh="${2:-}"
  if [ -z "$fresh" ]; then
    # shellcheck disable=SC2012 # capture dirs are dates: plain names
    fresh=$(ls -1d "$HOME"/.kioku/captures/*/ 2>/dev/null | sort | tail -n 1)
  fi
  if [ -z "$fresh" ] || [ ! -d "$fresh" ]; then
    echo "probe-agents --check: no fresh capture directory (run the probe, then kioku hook-dump extract <agent> <event>; or pass the directory)" >&2
    exit 1
  fi
  command -v python3 >/dev/null 2>&1 || { echo "probe-agents --check: python3 is required" >&2; exit 1; }
  exec python3 - "$fixtures" "$fresh" <<'PY'
import json, os, sys

fixtures, fresh = sys.argv[1], sys.argv[2]

def keys(path):
    with open(path, encoding="utf-8") as f:
        v = json.load(f)
    return set(v) - {"_meta"} if isinstance(v, dict) else set()

def counterpart(rel):
    d, name = os.path.split(rel)
    stem = name[: -len(".captured.json")]
    while True:
        cand = os.path.join(fresh, d, stem + ".captured.json")
        if os.path.isfile(cand):
            return cand
        if "_" not in stem:
            return None
        stem = stem.rsplit("_", 1)[0]

changed = compared = 0
for base, _, files in sorted(os.walk(fixtures)):
    for name in sorted(files):
        if not name.endswith(".captured.json"):
            continue
        rel = os.path.relpath(os.path.join(base, name), fixtures)
        if rel.split(os.sep)[0] == "legacy":
            continue
        other = counterpart(rel)
        if other is None:
            print(f"--   {rel}: not in the fresh capture")
            continue
        compared += 1
        old, new = keys(os.path.join(fixtures, rel)), keys(other)
        if old == new:
            print(f"ok   {rel}")
            continue
        changed += 1
        print(f"DIFF {rel} (vs {os.path.relpath(other, fresh)})")
        for k in sorted(new - old):
            print(f"     + {k}")
        for k in sorted(old - new):
            print(f"     - {k}")
print(f"{compared} fixture(s) compared, {changed} with a changed key set")
sys.exit(1 if changed else 0)
PY
fi

CFG="$HOME/.kioku/config.toml"
LOG="$HOME/.kioku/logs/hook-dump.jsonl"
PROBE="${KIOKU_PROBE_DIR:-$HOME/.kioku/probe-repo}"
PROMPT='このリポジトリ直下に hello.txt を作って「kioku probe」と1行書き、notes/todo.md を作って「- [ ] 次にやること: なし」と書いてください。それ以外のファイルは触らず、終わったら一言だけ報告して終了してください。'

[ -f "$CFG" ] || { echo "no $CFG — run 'kioku setup' first" >&2; exit 1; }

set_dump() { # true|false
  if grep -q '^hook_dump' "$CFG"; then
    perl -0pi -e "s/^hook_dump\\s*=.*/hook_dump = $1/m" "$CFG"
  else
    perl -0pi -e "s/^\\[client\\]\\n/[client]\\nhook_dump = $1\\n/m" "$CFG"
  fi
}
restore() { set_dump false; }
trap restore EXIT INT TERM

mkdir -p "$PROBE" "$HOME/.kioku/logs"
cd "$PROBE"
[ -d .git ] || { git init -q; git remote add origin https://github.com/misorafa/kioku-probe.git; }
rm -f hello.txt; rm -rf notes

set_dump true
[ -f "$LOG" ] && before=$(wc -l < "$LOG") || before=0

want="${*:-codex cursor gemini antigravity}"
for a in $want; do
  case "$a" in
    codex)
      if command -v codex >/dev/null 2>&1; then
        echo "== codex"; codex exec --full-auto "$PROMPT" || echo "(codex exited $?)"
      else echo "-- codex: not installed"; fi ;;
    cursor)
      if command -v agent >/dev/null 2>&1; then
        echo "== cursor (agent)"; agent -p "$PROMPT" --force || agent -p "$PROMPT" || echo "(agent exited $?)"
      elif command -v cursor-agent >/dev/null 2>&1; then
        echo "== cursor (cursor-agent)"; cursor-agent -p "$PROMPT" --force || cursor-agent -p "$PROMPT" || echo "(cursor-agent exited $?)"
      else echo "-- cursor: CLI not installed (agent / cursor-agent)"; fi ;;
    gemini)
      if command -v gemini >/dev/null 2>&1; then
        echo "== gemini"; gemini --yolo -p "$PROMPT" || gemini -p "$PROMPT" || echo "(gemini exited $?)"
      else echo "-- gemini: not installed"; fi ;;
    antigravity|agy)
      # --add-dir: without a workspace agy sends empty workspacePaths (SPEC-M2.1 §3.3).
      if command -v agy >/dev/null 2>&1; then
        echo "== antigravity (agy)"; agy -p "$PROMPT" --add-dir "$PROBE" || echo "(agy exited $?)"
        echo "-- hooks agy loaded:"; agy -p "/hooks" --output-format json || true
      else echo "-- antigravity: agy not installed"; fi ;;
    *) echo "unknown agent: $a" >&2 ;;
  esac
  rm -f hello.txt; rm -rf notes
done

set_dump false
trap - EXIT INT TERM

echo
echo "== captured hook events (new lines in $LOG):"
if [ -f "$LOG" ]; then
  tail -n +"$((before + 1))" "$LOG" | grep -o '"agent":"[^"]*","event":"[^"]*"' | sort | uniq -c
else
  echo "(no dump written)"
fi
echo "done — hook_dump is off again."
