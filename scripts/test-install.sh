#!/bin/sh
# shellcheck disable=SC2016,SC2034  # checks are eval'd strings; RC is read inside them
# Tests for install.sh (docs/SPEC-M2.md §16.11, SPEC-M2.3 §6). POSIX sh; run under dash and macOS sh:
#
#   scripts/test-install.sh                 # install.sh run with `sh`
#   KIOKU_TEST_SH=dash scripts/test-install.sh
#
# Builds fixture releases (stub `kioku` shell scripts that echo their argv, packaged
# exactly like release.yml does), serves them with a small python3 http.server that
# also answers GitHub's `releases/latest` redirect, and runs install.sh against them
# with KIOKU_DOWNLOAD_BASE / KIOKU_UNAME_S / KIOKU_UNAME_M and a temp HOME.
# A fake `id` (uid 1000 unless FAKE_UID is set) and a fake `cargo` come first on PATH.
# Set KIOKU_TEST_REAL_BIN=<path> to also install a real kioku binary (Linux x86_64).

set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TEST_SH=${KIOKU_TEST_SH:-sh}
WORK=$(mktemp -d 2>/dev/null || mktemp -d -t kioku-test)
SERVER_PID=""
cleanup() {
    [ -z "$SERVER_PID" ] || kill "$SERVER_PID" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT
trap 'exit 1' INT TERM

# The fixture server is on 127.0.0.1: keep any proxy out of the way.
unset http_proxy HTTP_PROXY https_proxy HTTPS_PROXY all_proxy ALL_PROXY
export no_proxy=127.0.0.1,localhost NO_PROXY=127.0.0.1,localhost

PASSED=0
FAILED=0
pass() {
    PASSED=$((PASSED + 1))
    printf 'ok   %s\n' "$1"
}
fail() {
    FAILED=$((FAILED + 1))
    printf 'FAIL %s\n' "$1"
    printf '%s\n' "$OUT" | sed 's/^/     | /'
}
# check <name> <shell condition…>
check() {
    c_name=$1
    shift
    if "$@"; then pass "$c_name"; else fail "$c_name"; fi
}
has() { printf '%s\n' "$OUT" | grep -F -- "$1" >/dev/null; }
lacks() { ! has "$1"; }

# ---------------------------------------------------------------- fixtures

FIX="$WORK/fix"
REL="$FIX/releases/download"
mkdir -p "$REL"

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# asset <tag> <target> <ok|broken> [binary]: package like release.yml (+ .sha256).
asset() {
    a_name="kioku-$1-$2"
    a_dir="$WORK/pkg/$a_name"
    mkdir -p "$a_dir" "$REL/$1"
    if [ -n "${4:-}" ]; then
        cp "$4" "$a_dir/kioku"
    elif [ "$3" = ok ]; then
        cat >"$a_dir/kioku" <<EOF
#!/bin/sh
if [ "\${1:-}" = --version ]; then echo "kioku ${1#v} ($2)"; exit 0; fi
printf 'STUB-ARGV:'; for a in "\$@"; do printf ' [%s]' "\$a"; done; echo
EOF
    else
        cat >"$a_dir/kioku" <<'EOF'
#!/bin/sh
echo "kioku: /lib/libc.so.6: version GLIBC_2.39 not found" >&2
exit 1
EOF
    fi
    chmod 755 "$a_dir/kioku"
    echo readme >"$a_dir/README.md"
    (cd "$WORK/pkg" && tar -cf - "$a_name" | gzip -1 >"$REL/$1/$a_name.tar.gz")
    (cd "$REL/$1" && printf '%s  %s\n' "$(sha256_of "$a_name.tar.gz")" "$a_name.tar.gz" >"$a_name.tar.gz.sha256")
    rm -rf "$a_dir"
}
sums() { (cd "$REL/$1" && cat ./*.sha256 | sort -k2 >SHA256SUMS); }

# v9.9.9 (latest): musl for x86_64 only, gnu for both, both macOS targets.
for t in x86_64-unknown-linux-musl x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
    aarch64-apple-darwin x86_64-apple-darwin; do
    asset v9.9.9 "$t" ok
done
sums v9.9.9
# v9.9.8: the musl binary does not run here -> gnu.
asset v9.9.8 x86_64-unknown-linux-musl broken
asset v9.9.8 x86_64-unknown-linux-gnu ok
sums v9.9.8
# v9.9.7: SHA256SUMS lies.
asset v9.9.7 x86_64-unknown-linux-musl ok
printf '%s  %s\n' 0000000000000000000000000000000000000000000000000000000000000000 \
    kioku-v9.9.7-x86_64-unknown-linux-musl.tar.gz >"$REL/v9.9.7/SHA256SUMS"
# v9.9.6: predates SHA256SUMS (only <asset>.sha256).
asset v9.9.6 x86_64-unknown-linux-musl ok
# v9.9.5: no checksum at all.
asset v9.9.5 x86_64-unknown-linux-musl ok
rm -f "$REL"/v9.9.5/*.sha256

# ---------------------------------------------------------------- server

cat >"$WORK/server.py" <<'EOF'
import functools, http.server, os, socketserver, sys

root, portfile = sys.argv[1], sys.argv[2]

class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def route(self):
        path = self.path.split("?")[0]
        if path.startswith("/broken/"):
            # A server error (GitHub outage, captive portal, proxy): not "no release".
            self.send_response(500)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return True
        if path.endswith("/releases/latest"):
            prefix = path[: -len("/releases/latest")]
            # /empty/...: a repository without releases (GitHub redirects to /releases).
            loc = prefix + ("/releases" if prefix == "/empty" else "/releases/tag/v9.9.9")
            self.send_response(302)
            self.send_header("Location", loc)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return True
        if "/releases/tag/" in path or path == "/empty/releases":
            body = b"release page\n"
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if self.command == "GET":
                self.wfile.write(body)
            return True
        return False

    def do_GET(self):
        if not self.route():
            super().do_GET()

    def do_HEAD(self):
        if not self.route():
            super().do_HEAD()

class Server(http.server.ThreadingHTTPServer):
    # HTTPServer.server_bind calls socket.getfqdn(), a reverse DNS lookup that takes
    # ~35 s on macOS (and CI's macOS runners): skip it, the name is never used.
    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name = "127.0.0.1"
        self.server_port = self.server_address[1]

srv = Server(("127.0.0.1", 0), functools.partial(Handler, directory=root))
with open(portfile + ".tmp", "w") as f:
    f.write(str(srv.server_address[1]))
os.rename(portfile + ".tmp", portfile)
srv.serve_forever()
EOF
python3 "$WORK/server.py" "$FIX" "$WORK/port" &
SERVER_PID=$!
i=0
while [ ! -f "$WORK/port" ]; do
    i=$((i + 1))
    [ "$i" -lt 100 ] || { echo "fixture server did not start" >&2; exit 1; }
    sleep 0.1
done
PORT=$(cat "$WORK/port")
BASE="http://127.0.0.1:$PORT/releases"

# ---------------------------------------------------------------- fake tools

FAKEBIN="$WORK/fakebin"
mkdir -p "$FAKEBIN"
cat >"$FAKEBIN/id" <<'EOF'
#!/bin/sh
if [ "${1:-}" = -u ]; then echo "${FAKE_UID:-1000}"; exit 0; fi
exec /usr/bin/id "$@"
EOF
FAKECARGO="$WORK/fakecargo"
mkdir -p "$FAKECARGO"
cat >"$FAKECARGO/cargo" <<'EOF'
#!/bin/sh
if [ "${1:-}" = --version ]; then echo "cargo ${FAKE_CARGO_VERSION:-1.95.0} (fake)"; exit 0; fi
echo "$*" >>"$CARGO_LOG"
root=""
while [ $# -gt 0 ]; do
    if [ "$1" = --root ]; then root=$2; fi
    shift
done
mkdir -p "$root/bin"
printf '#!/bin/sh\necho "kioku 0.0.0-source"\n' >"$root/bin/kioku"
chmod 755 "$root/bin/kioku"
EOF
chmod 755 "$FAKEBIN/id" "$FAKECARGO/cargo"
export CARGO_LOG="$WORK/cargo.log"
# Base PATH without the developer's ~/.cargo/bin (so "no cargo" is really no cargo).
PATH0="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

HOMES=0
newhome() {
    HOMES=$((HOMES + 1))
    H="$WORK/home $HOMES"
    mkdir -p "$H"
}

# inst <uname_s> <uname_m> [install.sh args…] -> OUT, RC (runs in $H with HOME=$H).
# Knobs: T_PATH, T_BASE, T_SHELL, FAKE_UID, FAKE_CARGO_VERSION (cleared after each call).
inst() {
    i_s=$1
    i_m=$2
    shift 2
    RC=0
    OUT=$(cd "$H" && env HOME="$H" SHELL="${T_SHELL:-/bin/zsh}" PATH="${T_PATH:-$FAKEBIN:$PATH0}" \
        FAKE_UID="${FAKE_UID:-1000}" KIOKU_DOWNLOAD_BASE="${T_BASE:-$BASE}" \
        KIOKU_UNAME_S="$i_s" KIOKU_UNAME_M="$i_m" "$TEST_SH" "$ROOT/install.sh" "$@" 2>&1) || RC=$?
    # `KNOB=x inst …` is scoped to the call in dash, but bash in POSIX mode (macOS /bin/sh)
    # keeps the assignment afterwards: clear the knobs so they never leak into later tests.
    unset T_PATH T_BASE T_SHELL FAKE_UID FAKE_CARGO_VERSION
}
BIN() { printf '%s' "$H/.local/bin/kioku"; }
installed_is() { [ -x "$(BIN)" ] && "$(BIN)" --version | grep -F -- "$1" >/dev/null; }
nothing_installed() {
    [ ! -e "$(BIN)" ] && [ -z "$(find "$H" -name '.kioku.new.*' 2>/dev/null)" ]
}

echo "install.sh tests (sh: $TEST_SH, fixtures: $BASE)"

# ---------------------------------------------------------------- target matrix

newhome
inst Linux x86_64 --no-setup
check "latest resolves via redirect; Linux x86_64 -> musl" \
    eval '[ "$RC" = 0 ] && installed_is "9.9.9 (x86_64-unknown-linux-musl)" && has "installing kioku v9.9.9"'
check "--no-setup does not run kioku setup" eval 'lacks STUB-ARGV && has "done (--no-setup)"'
RCLINE='export PATH="$HOME/.local/bin:$PATH" # added by the kioku installer'
check "PATH added to ~/.zshrc (zsh)" \
    eval 'has "added \$HOME/.local/bin to PATH in ~/.zshrc" && [ "$(cat "$H/.zshrc")" = "$RCLINE" ]'
check "the rc line puts ~/.local/bin on PATH" \
    eval '[ "$(env HOME="$H" PATH=/usr/bin /bin/sh -c ". \"\$HOME/.zshrc\"; printf %s \"\$PATH\"")" = "$H/.local/bin:/usr/bin" ]'
check "no temp files left in the install dir" eval '[ -z "$(find "$H/.local/bin" -name ".kioku.new.*")" ]'

inst Linux x86_64 --no-setup --version 9.9.8
check "reinstall over an existing binary; musl does not run -> gnu" \
    eval '[ "$RC" = 0 ] && installed_is "9.9.8 (x86_64-unknown-linux-gnu)" && has "does not run here"'
check "PATH line written exactly once" \
    eval '[ "$(grep -c "added by the kioku installer" "$H/.zshrc")" = 1 ] && has "already on PATH in ~/.zshrc"'

newhome
inst Linux aarch64 --no-setup --version v9.9.9
check "Linux aarch64: no musl asset -> gnu" \
    eval '[ "$RC" = 0 ] && installed_is "aarch64-unknown-linux-gnu" && has "no kioku-v9.9.9-aarch64-unknown-linux-musl.tar.gz"'

newhome
inst Linux amd64 --no-setup --version v9.9.9
check "uname amd64 maps to x86_64" eval '[ "$RC" = 0 ] && installed_is x86_64-unknown-linux-musl'

newhome
inst Darwin arm64 --no-setup --version v9.9.9
check "Darwin arm64 -> aarch64-apple-darwin" eval '[ "$RC" = 0 ] && installed_is aarch64-apple-darwin'

newhome
T_SHELL=/bin/bash inst Darwin x86_64 --no-setup --version v9.9.9
check "Darwin x86_64 -> x86_64-apple-darwin, bash on macOS -> ~/.bash_profile" \
    eval '[ "$RC" = 0 ] && installed_is x86_64-apple-darwin && grep -F -x "$RCLINE" "$H/.bash_profile" >/dev/null && [ ! -e "$H/.bashrc" ]'

newhome
printf 'alias ll="ls -l"' >"$H/.bashrc"
T_SHELL=/bin/bash inst Linux x86_64 --no-setup --version v9.9.9
check "bash on Linux -> ~/.bashrc, appended after a last line without newline" \
    eval '[ "$RC" = 0 ] && [ "$(sed -n 1p "$H/.bashrc")" = "alias ll=\"ls -l\"" ] && [ "$(sed -n 2p "$H/.bashrc")" = "$RCLINE" ]'

newhome
inst Linux x86_64 --no-setup --version v9.9.9 --no-modify-path
check "--no-modify-path: only a hint, no rc file" \
    eval '[ "$RC" = 0 ] && has "not on your PATH" && has ">> ~/.zshrc" && [ ! -e "$H/.zshrc" ]'

newhome
T_SHELL=/usr/bin/fish inst Linux x86_64 --no-setup --version v9.9.9
check "fish -> conf.d/kioku.fish" \
    eval 'grep -F "set -gx PATH \"\$HOME/.local/bin\" \$PATH" "$H/.config/fish/conf.d/kioku.fish" >/dev/null'

newhome
T_PATH="$H/.local/bin:$FAKEBIN:$PATH0" inst Linux x86_64 --no-setup --version v9.9.9
check "PATH left alone when the dir is on PATH" \
    eval '[ "$RC" = 0 ] && lacks "not on your PATH" && lacks "to PATH in" && [ ! -e "$H/.zshrc" ]'

newhome
mkdir -p "$H/.local/bin/kioku"
inst Linux x86_64 --no-setup --version v9.9.9
check "a failing final step inside try_target aborts (exit 1), no temp file left" \
    eval '[ "$RC" = 1 ] && has "is a directory" && lacks "installed kioku" &&
        [ -z "$(find "$H/.local/bin" -name ".kioku.new.*")" ]'

# ---------------------------------------------------------------- checksums

newhome
inst Linux x86_64 --no-setup --version v9.9.7
check "checksum mismatch aborts, nothing installed" \
    eval '[ "$RC" != 0 ] && has "checksum mismatch" && nothing_installed'

newhome
inst Linux x86_64 --no-setup --version v9.9.6
check "missing SHA256SUMS falls back to <asset>.sha256" \
    eval '[ "$RC" = 0 ] && installed_is "9.9.6" && has "checksum ok"'

newhome
inst Linux x86_64 --no-setup --version v9.9.5
check "no checksum at all aborts, nothing installed" \
    eval '[ "$RC" != 0 ] && has "refusing to install an unverified binary" && nothing_installed'

newhome
# Git Bash on Windows (SPEC-M2.3 §9): hand over to install.ps1 run by PowerShell from a local
# file, with KIOKU_JOIN carrying the invite; never `-ExecutionPolicy Bypass` / `irm … | iex`.
printf '# fixture install.ps1\n' >"$FIX/install.ps1"
cat >"$FAKEBIN/powershell.exe" <<'PS'
#!/bin/sh
echo "PS-ARGV: $*"
echo "PS-JOIN: [${KIOKU_JOIN:-}]"
f=$(printf '%s' "$*" | sed -n "s/.*ReadAllText('\([^']*\)').*/\1/p")
[ -f "$f" ] && echo "PS-FILE: $(cat "$f")"
exit 0
PS
chmod 755 "$FAKEBIN/powershell.exe"
newhome
RC=0
OUT=$(cd "$H" && env HOME="$H" SHELL=/bin/bash PATH="$FAKEBIN:$PATH0" KIOKU_UNAME_S=MINGW64_NT-10.0 \
    KIOKU_UNAME_M=x86_64 KIOKU_PS1_URL="http://127.0.0.1:$PORT/install.ps1" \
    KIOKU_JOIN_URL=http://192.168.1.240:7391 KIOKU_JOIN_CODE=K7Q2M9XD \
    "$TEST_SH" "$ROOT/install.sh" 2>&1) || RC=$?
check "Git Bash on Windows -> install.ps1 via PowerShell from a file, invite passed as KIOKU_JOIN" \
    eval '[ "$RC" = 0 ] && has "PS-JOIN: [http://192.168.1.240:7391/K7Q2M9XD]" && has "PS-FILE: # fixture install.ps1" &&
        has "-NoProfile -Command" && lacks "Bypass" && lacks "iex" && nothing_installed'
rm -f "$FAKEBIN/powershell.exe"


# ---------------------------------------------------------------- source fallback

newhome
: >"$CARGO_LOG"
T_PATH="$FAKEBIN:$FAKECARGO:$PATH0" inst FreeBSD amd64 --no-setup
check "FreeBSD -> source fallback with cargo install --tag <latest>" \
    eval '[ "$RC" = 0 ] && has "no prebuilt binary for FreeBSD" && has "falling back to --from-source" &&
        grep -F "install --locked --git https://github.com/misorafa/kioku --tag v9.9.9 kioku-cli --root" "$CARGO_LOG" >/dev/null &&
        installed_is 0.0.0-source'

newhome
if [ -z "$(PATH=$PATH0 command -v cargo 2>/dev/null || true)" ]; then
    inst Linux riscv64 --no-setup --version v9.9.9
    check "unknown arch without cargo -> exact commands, exit 1" \
        eval '[ "$RC" = 1 ] && has "no prebuilt binary" && has "sh.rustup.rs" &&
            has "cargo install --locked --git https://github.com/misorafa/kioku --tag v9.9.9 kioku-cli" && nothing_installed'
else
    echo "skip unknown arch without cargo (cargo is in $PATH0)"
fi

newhome
FAKE_CARGO_VERSION=1.80.0 T_PATH="$FAKEBIN:$FAKECARGO:$PATH0" inst Linux riscv64 --no-setup --version v9.9.9
check "cargo older than 1.91 is refused" eval '[ "$RC" = 1 ] && has "cargo 1.91 or newer is required" && nothing_installed'

newhome
: >"$CARGO_LOG"
T_BASE="http://127.0.0.1:$PORT/empty/releases" T_PATH="$FAKEBIN:$FAKECARGO:$PATH0" inst Linux x86_64 --no-setup
check "no release published -> source fallback without --tag" \
    eval '[ "$RC" = 0 ] && has "no published release" && ! grep -F -- "--tag" "$CARGO_LOG" >/dev/null && installed_is 0.0.0-source'

newhome
: >"$CARGO_LOG"
T_BASE="http://127.0.0.1:$PORT/broken/releases" T_PATH="$FAKEBIN:$FAKECARGO:$PATH0" inst Linux x86_64 --no-setup
check "HTTP 500 on releases/latest -> network error, exit 1, no source fallback" \
    eval '[ "$RC" = 1 ] && has "network or HTTP error" && lacks "falling back" && [ ! -s "$CARGO_LOG" ] && nothing_installed'

newhome
: >"$CARGO_LOG"
DEAD_PORT=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
T_BASE="http://127.0.0.1:$DEAD_PORT/releases" T_PATH="$FAKEBIN:$FAKECARGO:$PATH0" inst Linux x86_64 --no-setup
check "unreachable host -> network error, exit 1, no source fallback" \
    eval '[ "$RC" = 1 ] && has "network or HTTP error" && [ ! -s "$CARGO_LOG" ] && nothing_installed'

newhome
: >"$CARGO_LOG"
T_PATH="$FAKEBIN:$FAKECARGO:$PATH0" inst Linux x86_64 --no-setup --from-source --version v9.9.9
check "--from-source skips the binaries" \
    eval '[ "$RC" = 0 ] && lacks "downloading" && grep -F -- "--tag v9.9.9" "$CARGO_LOG" >/dev/null && installed_is 0.0.0-source'

# ---------------------------------------------------------------- setup hand-off

newhome
inst Linux x86_64 --version v9.9.9 --client-only http://home.lan:7391 SECRET-TOKEN-42 --dry-run
check "--client-only args reach kioku setup" \
    eval '[ "$RC" = 0 ] && has "STUB-ARGV: [setup] [--client-only] [http://home.lan:7391] [SECRET-TOKEN-42] [--dry-run]"'
check "the token is never echoed by the installer" \
    eval '! printf "%s\n" "$OUT" | grep "^kioku-install:" | grep -F SECRET-TOKEN-42 >/dev/null'

newhome
inst Linux x86_64 --version v9.9.9 --no-service -- --no-setup "it's a b" --version x
check "args after -- go to setup verbatim (incl. install.sh flags and quotes)" \
    eval '[ "$RC" = 0 ] && has "STUB-ARGV: [setup] [--no-service] [--no-setup] [it'"'"'s a b] [--version] [x]"'

newhome
inst Linux x86_64 --version v9.9.9 --client-only http://h:7391 --no-setup
check "--client-only takes the next two args verbatim" \
    eval '[ "$RC" = 0 ] && has "STUB-ARGV: [setup] [--client-only] [http://h:7391] [--no-setup]"'

# ---------------------------------------------------------------- join mode (SPEC-M2.3 §4.1)

# The script `GET /i/<code>` serves: install.sh with the two variables after the shebang.
{
    head -n 1 "$ROOT/install.sh"
    printf "KIOKU_JOIN_URL='%s'\nKIOKU_JOIN_CODE='%s'\n" http://192.168.1.240:7391 K7Q2M9XD
    tail -n +2 "$ROOT/install.sh"
} >"$WORK/served.sh"
newhome
RC=0
OUT=$(cd "$H" && env HOME="$H" SHELL=/bin/zsh PATH="$FAKEBIN:$PATH0" KIOKU_DOWNLOAD_BASE="$BASE" \
    KIOKU_UNAME_S=Linux KIOKU_UNAME_M=x86_64 "$TEST_SH" <"$WORK/served.sh" 2>&1) || RC=$?
check "served script piped to sh (curl … | sh) runs kioku join with the url and code" \
    eval '[ "$RC" = 0 ] && has "STUB-ARGV: [join] [http://192.168.1.240:7391] [K7Q2M9XD]" && lacks "[setup]"'
check "join mode puts kioku on PATH and says so in ja + en" \
    eval 'grep -F -x "$RCLINE" "$H/.zshrc" >/dev/null && has "新しいターミナルを開くと kioku コマンドが使えます" && has "Open a new terminal"'

newhome
RC=0
OUT=$(cd "$H" && env HOME="$H" SHELL=/bin/zsh PATH="$FAKEBIN:$PATH0" KIOKU_DOWNLOAD_BASE="$BASE" \
    KIOKU_UNAME_S=Linux KIOKU_UNAME_M=x86_64 KIOKU_VERSION=v9.9.9 \
    KIOKU_JOIN_URL=http://mini.local:7391 KIOKU_JOIN_CODE=ABCD2345 \
    "$TEST_SH" "$ROOT/install.sh" --no-modify-path --agents codex 2>&1) || RC=$?
check "KIOKU_JOIN_URL / KIOKU_JOIN_CODE env; extra args go to kioku join; --no-modify-path" \
    eval '[ "$RC" = 0 ] && has "STUB-ARGV: [join] [http://mini.local:7391] [ABCD2345] [--agents] [codex]" &&
        [ ! -e "$H/.zshrc" ] && lacks "新しいターミナル"'

newhome
RC=0
OUT=$(cd "$H" && env HOME="$H" SHELL=/bin/zsh PATH="$FAKEBIN:$PATH0" KIOKU_DOWNLOAD_BASE="$BASE" \
    KIOKU_UNAME_S=Linux KIOKU_UNAME_M=x86_64 KIOKU_VERSION=v9.9.9 KIOKU_JOIN=mini.local:7391/ABCD2345 \
    "$TEST_SH" "$ROOT/install.sh" --no-modify-path 2>&1) || RC=$?
check "KIOKU_JOIN=<server>:<port>/<code> (what kioku invite prints)" \
    eval '[ "$RC" = 0 ] && has "STUB-ARGV: [join] [http://mini.local:7391] [ABCD2345]"'
RC=0
OUT=$(cd "$H" && env HOME="$H" SHELL=/bin/zsh PATH="$FAKEBIN:$PATH0" KIOKU_DOWNLOAD_BASE="$BASE" \
    KIOKU_UNAME_S=Linux KIOKU_UNAME_M=x86_64 KIOKU_VERSION=v9.9.9 KIOKU_JOIN=no-code-here \
    "$TEST_SH" "$ROOT/install.sh" --no-modify-path 2>&1) || RC=$?
check "a malformed KIOKU_JOIN is refused" eval '[ "$RC" = 1 ] && has "KIOKU_JOIN must look like"'

newhome
inst Linux x86_64 --version v9.9.9 --join http://h:7391 CODE2345
check "--join <url> <code>" eval '[ "$RC" = 0 ] && has "STUB-ARGV: [join] [http://h:7391] [CODE2345]"'
inst Linux x86_64 --version v9.9.9 --join http://h:7391
check "--join without a code is refused" eval '[ "$RC" = 1 ] && has "--join needs <url> <code>"'

# ---------------------------------------------------------------- root, env, downloader

newhome
FAKE_UID=0 inst Linux x86_64 --no-setup --version v9.9.9
check "root without --install-dir is refused" eval '[ "$RC" = 1 ] && has "refusing to run as root" && nothing_installed'
FAKE_UID=0 inst Linux x86_64 --no-setup --version v9.9.9 --install-dir "$H/opt bin"
check "root with an explicit --install-dir is allowed" \
    eval '[ "$RC" = 0 ] && [ -x "$H/opt bin/kioku" ] && has "added \$HOME/opt bin to PATH in ~/.zshrc"'

newhome
RC=0
OUT=$(cd "$H" && env HOME="$H" SHELL=/bin/sh PATH="$FAKEBIN:$PATH0" KIOKU_DOWNLOAD_BASE="$BASE" \
    KIOKU_UNAME_S=Linux KIOKU_UNAME_M=x86_64 KIOKU_VERSION=v9.9.6 KIOKU_INSTALL_DIR="$H/envdir" \
    "$TEST_SH" "$ROOT/install.sh" --no-setup 2>&1) || RC=$?
check "KIOKU_VERSION / KIOKU_INSTALL_DIR env, sh -> ~/.profile" \
    eval '[ "$RC" = 0 ] && "$H/envdir/kioku" --version | grep -F 9.9.6 >/dev/null && has "to PATH in ~/.profile" &&
        grep -F -x "export PATH=\"\$HOME/envdir:\$PATH\" # added by the kioku installer" "$H/.profile" >/dev/null'

if command -v wget >/dev/null 2>&1; then
    NOCURL="$WORK/nocurl"
    mkdir -p "$NOCURL"
    for d in /usr/local/bin /usr/bin /bin /usr/sbin /sbin; do
        [ -d "$d" ] || continue
        for f in "$d"/*; do
            b=$(basename "$f")
            [ "$b" = curl ] || [ -e "$NOCURL/$b" ] || ln -s "$f" "$NOCURL/$b"
        done
    done
    # wget may live outside those dirs (Homebrew on Apple silicon: /opt/homebrew/bin).
    [ -e "$NOCURL/wget" ] || ln -s "$(command -v wget)" "$NOCURL/wget"
    newhome
    T_PATH="$FAKEBIN:$NOCURL" inst Linux x86_64 --no-setup
    check "wget only: latest + download + verify" eval '[ "$RC" = 0 ] && installed_is "9.9.9 (x86_64-unknown-linux-musl)"'
    newhome
    T_BASE="http://127.0.0.1:$PORT/broken/releases" T_PATH="$FAKEBIN:$NOCURL" inst Linux x86_64 --no-setup
    check "wget only: HTTP 500 -> network error, exit 1" \
        eval '[ "$RC" = 1 ] && has "network or HTTP error" && nothing_installed'
else
    echo "skip wget-only (no wget)"
fi

# ---------------------------------------------------------------- real binary (optional)

if [ -n "${KIOKU_TEST_REAL_BIN:-}" ]; then
    asset v0.0.1 x86_64-unknown-linux-gnu ok "$KIOKU_TEST_REAL_BIN"
    sums v0.0.1
    newhome
    inst Linux x86_64 --version v0.0.1 --no-service --no-agents --dry-run
    check "real binary installed and 'kioku setup --dry-run' ran" \
        eval '[ "$RC" = 0 ] && has "kioku setup (v" && [ ! -e "$H/.kioku/config.toml" ]'
fi

echo "passed: $PASSED, failed: $FAILED"
[ "$FAILED" = 0 ]
