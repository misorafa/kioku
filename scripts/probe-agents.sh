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
set -eu

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
