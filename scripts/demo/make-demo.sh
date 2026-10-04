#!/bin/sh
# make-demo.sh — render docs/media/kioku-demo-{ja,en}.{gif,mp4} with VHS against a throwaway
# kioku server (a temp HOME; never touches ~/.kioku). Needs: kioku on PATH, vhs, ffmpeg, ttyd
# (brew install vhs ffmpeg), git, curl, python3. Usage: sh scripts/demo/make-demo.sh [out-dir]
set -eu
OUT=${1:-docs/media}; OUT=$(mkdir -p "$OUT" && cd "$OUT" && pwd)
HERE=$(cd "$(dirname "$0")" && pwd)
S=$(mktemp -d "${TMPDIR:-/tmp}/kioku-demo.XXXXXX"); PORT=47391
export PATH="$S/bin:$HOME/.cargo/bin:$HOME/.local/bin:/opt/homebrew/bin:$PATH"
cleanup() { pkill -f "kioku serve --bind 127.0.0.1 --port $PORT" 2>/dev/null || true; }
trap cleanup EXIT
mkdir -p "$S/bin" "$S/home" "$S/repo" "$S/repo-en"
for r in repo repo-en; do git -C "$S/$r" init -q && git -C "$S/$r" commit -q --allow-empty -m init; done
git -C "$S/repo" remote add origin https://github.com/misorafa/demo-app.git
git -C "$S/repo-en" remote add origin https://github.com/misorafa/demo-shop.git
CFG="$S/home/.kioku/config.toml"
HOME="$S/home" KIOKU_DATA_DIR="$S/home/.kioku" kioku init >/dev/null
sed -i '' -e "s/^port = .*/port = $PORT/" -e "s|^server_url = .*|server_url = \"http://127.0.0.1:$PORT\"|" "$CFG"
serve() { (HOME="$S/home" KIOKU_DATA_DIR="$S/home/.kioku" kioku serve --bind 127.0.0.1 --port $PORT --log-file "$S/serve.log" >/dev/null 2>&1 &); sleep 2; }
setlang() { if grep -q '^summary_lang' "$CFG"; then sed -i '' "s/^summary_lang = .*/summary_lang = \"$1\"/" "$CFG"; else sed -i '' "s/^\[server\]\$/[server]\\
summary_lang = \"$1\"/" "$CFG"; fi; }
# English client HOME: same server, lang = en, no data dir (a client-only machine).
cp -R "$S/home" "$S/home-en"; python3 - "$S/home-en/.kioku/config.toml" <<'PY'
import sys,re; p=sys.argv[1]; s=open(p).read(); s=re.sub(r'(?m)^lang = .*\n','',s); s=s.replace('[client]\n','[client]\nlang = "en"\n',1); open(p,'w').write(s)
PY
pid_of() { (cd "$1" && printf '{"session_id":"probe","hook_event_name":"SessionStart","cwd":"%s","source":"startup"}' "$1" | HOME="$2" kioku hook session-start --agent claude-code | sed -n 's/.*(id: \([a-z0-9-]*\)).*/\1/p' | head -1); }
setlang ja; serve
PJA=$(HOME="$S/home" KIOKU_DATA_DIR="$S/home/.kioku" pid_of "$S/repo" "$S/home")
sh "$HERE/seed-ja.sh" "$S" "$PJA" "$S/home" >/dev/null
cleanup; sleep 1; setlang en; serve
PEN=$(pid_of "$S/repo-en" "$S/home-en")
sh "$HERE/seed-en.sh" "$S" "$PEN" "$S/home-en" >/dev/null
cleanup; sleep 1; setlang ja; serve
printf '{"session_id":"s3","hook_event_name":"SessionStart","cwd":"%s","source":"startup"}\n' "$S/repo" > "$S/start.json"
printf '{"session_id":"e3","hook_event_name":"SessionStart","cwd":"%s","source":"startup"}\n' "$S/repo-en" > "$S/start-en.json"
printf '#!/bin/sh\nexec kioku hook session-start --agent claude-code < "%s"\n' "$S/start.json" > "$S/bin/next-session"
printf '#!/bin/sh\nexec kioku hook session-start --agent claude-code < "%s"\n' "$S/start-en.json" > "$S/bin/next-session-en"
chmod +x "$S/bin/next-session" "$S/bin/next-session-en"
for L in ja en; do
  sed -e "s|@SANDBOX@|$S|g" -e "s|@CARGOBIN@|$HOME/.cargo/bin|g" "$HERE/demo-$L.tape" > "$S/demo-$L.tape"
  (cd "$S" && vhs "demo-$L.tape" >/dev/null 2>&1)
  cp "$S/kioku-demo-$L.gif" "$S/kioku-demo-$L.mp4" "$OUT/"
done
ls -la "$OUT"/kioku-demo-*
