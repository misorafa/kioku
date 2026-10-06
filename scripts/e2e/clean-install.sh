#!/bin/sh
# Virtual "clean install on several machines" test of the PUBLIC release path (no local
# build): pristine machines install kioku from the GitHub release exactly as README
# "Install" / "Adding another machine" tells a user to, then a Claude Code session is
# simulated with the real hook binary on one client, and a session on the OTHER client must
# start with the first one's handoff (the product promise). See scripts/e2e/README.md.
#
#   sh scripts/e2e/clean-install.sh                   # Docker, latest release
#   KIOKU_RELEASE_TAG=v0.9.4 sh scripts/e2e/clean-install.sh
#   sh scripts/e2e/clean-install.sh --keep            # leave containers + network for inspection
#   sh scripts/e2e/clean-install.sh --only install    # or: --only image
#   sh scripts/e2e/clean-install.sh --host            # no Docker: "machines" are separate HOMEs
#                                                     # on this host (the macOS CI job)
#
# Docker mode (one user-defined network):
#   install  server = ubuntu:24.04 (curl, git, ca-certificates, non-root user, no Rust) running
#            the README one-liner with `--bind 0.0.0.0 --no-service --no-agents`, then
#            `kioku serve` in the background and `kioku invite --host <container> --uses 2`.
#   image    server = ghcr.io/misorafa/kioku:<tag> (`:latest` by default); skipped with a
#            notice when the image cannot be pulled.
#   Clients: debian:bookworm-slim (glibc) and alpine:3 (musl).
# Host mode (--host): server and two clients are directories used as HOME under a temp dir,
#   every command runs with `env -i` (nothing of the real HOME, ~/.kioku or KIOKU_* leaks
#   in); the server binds 127.0.0.1 on port KIOKU_E2E_PORT (default 17391, never 7391).
# Both: each client runs `mkdir ~/.claude` (Claude Code "installed"), pastes the printed
#   macOS / Linux invite line verbatim (`KIOKU_JOIN=… sh -c "$(curl -fsSL …/install.sh)"`),
#   then: SessionStart → UserPromptSubmit (Japanese) → PostToolUse → kioku_handoff_write
#   through the registered `kioku mcp` bridge → Stop on client A; SessionStart on client B
#   must show A's handoff; a Japanese `kioku search`, redaction, a short session on B and a
#   SessionStart back on A (rules handoff); `kioku doctor` exits 0 everywhere with only the
#   warnings listed in ALLOWED_WARN_* below.
#
# Prints PASS / FAIL / KNOWN / SKIP per step and exits 1 on any FAIL. KNOWN = a failure that
# matches a bug already fixed on main but not yet in the release under test (see known()).
# Never prints a token or a full invite code. Needs curl, awk, sed (+ docker without --host).
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
REPO=misorafa/kioku
INSTALL_SH_URL="https://raw.githubusercontent.com/$REPO/main/install.sh"
FIX="$ROOT/crates/kioku-cli/tests/fixtures"

# Doctor warnings expected on these machines (no agent besides the empty ~/.claude):
#  server (install.sh): `service` - no systemd --user in a container / no LaunchAgent with
#                       --no-service: the server is run by hand (the documented setup).
ALLOWED_WARN_SERVER="service"
ALLOWED_WARN_IMAGE=""
ALLOWED_WARN_CLIENT=""

KEEP=0
ONLY=""
MODE=docker
while [ $# -gt 0 ]; do
    case "$1" in
        --keep) KEEP=1; shift ;;
        --host) MODE=host; shift ;;
        --only) [ $# -ge 2 ] || { echo "--only needs install|image" >&2; exit 2; }; ONLY=$2; shift 2 ;;
        -h | --help) sed -n '2,36p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done
case "$ONLY" in "" | install | image) ;; *) echo "--only needs install|image" >&2; exit 2 ;; esac

RUN="ke2e$$"
NET="$RUN-net"
WORK=$(mktemp -d 2>/dev/null || mktemp -d -t kioku-e2e)
WORK=$(cd -P "$WORK" && pwd)
BOXES="$WORK/boxes"
IMG_TOKEN=""
if [ "$MODE" = docker ]; then
    PORT=7391
    command -v docker >/dev/null 2>&1 || { echo "docker is required (or use --host)" >&2; exit 2; }
    docker info >/dev/null 2>&1 || { echo "the Docker daemon does not answer" >&2; exit 2; }
else
    PORT=${KIOKU_E2E_PORT:-17391}
    [ "$PORT" != 7391 ] || { echo "--host never uses port 7391 (a real kioku server may own it)" >&2; exit 2; }
    if curl -fsS -m 2 "http://127.0.0.1:$PORT/api/v1/health" >/dev/null 2>&1; then
        echo "port $PORT is in use; set KIOKU_E2E_PORT" >&2
        exit 2
    fi
    # kioku warns about a binary under the temp dir or /tmp ("hooks break when it moves"):
    # the machines' HOMEs live outside both (the kioku processes get TMPDIR=$WORK/tmp).
    case "$WORK" in
        /tmp/* | /private/tmp/*)
            rm -rf "$WORK"
            WORK=$(mktemp -d "$HOME/.kioku-e2e.XXXXXX")
            BOXES="$WORK/boxes"
            ;;
    esac
    mkdir -p "$BOXES" "$WORK/tmp"
fi

cleanup() {
    rc=$?
    for f in "$WORK"/*.pid; do
        [ -f "$f" ] || continue
        if [ "$KEEP" = 1 ]; then
            printf '\n--keep: kioku serve still runs (pid %s); stop it with: kill %s\n' "$(cat "$f")" "$(cat "$f")"
        else
            kill "$(cat "$f")" 2>/dev/null || true
        fi
    done
    if [ "$MODE" = docker ] && [ "$KEEP" = 1 ]; then
        printf '\n--keep: containers and network left running:\n'
        docker ps -a --filter "label=kioku-e2e=$RUN" --format '  {{.Names}}  ({{.Image}})'
        printf '  network %s\n  remove with: docker rm -f $(docker ps -aq --filter label=kioku-e2e=%s); docker network rm %s; docker volume rm %s-data\n' \
            "$NET" "$RUN" "$NET" "$RUN"
        printf '  shell: docker exec -it -u dev -w /home/dev <name> sh -l\n'
    elif [ "$MODE" = docker ]; then
        ids=$(docker ps -aq --filter "label=kioku-e2e=$RUN" 2>/dev/null || true)
        # shellcheck disable=SC2086 # one word per container id
        [ -z "$ids" ] || docker rm -f $ids >/dev/null 2>&1 || true
        docker network rm "$NET" >/dev/null 2>&1 || true
        docker volume rm -f "$RUN-data" >/dev/null 2>&1 || true
    fi
    if [ "$KEEP" = 1 ] && [ "$MODE" = host ]; then
        printf '\n--keep: the machines (HOMEs) are in %s\n' "$BOXES"
    else
        rm -rf "$WORK"
    fi
    exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# ------------------------------------------------------------------------------ report

PASSED=0
FAILED=0
KNOWN=0
SKIPPED=0
OUT=""
RC=0
SCEN=""

# Masks the one-time invite code and the image's throwaway token in anything printed.
mask() {
    if [ -n "$IMG_TOKEN" ]; then
        sed -e "s#:$PORT/[A-Za-z0-9]\{4,\}#:$PORT/<code>#g" -e "s#$IMG_TOKEN#<token>#g"
    else
        sed -e "s#:$PORT/[A-Za-z0-9]\{4,\}#:$PORT/<code>#g"
    fi
}
detail() { printf '%s\n' "$OUT" | tail -n 40 | mask | sed 's/^/      | /'; }
pass() { PASSED=$((PASSED + 1)); printf 'PASS  [%s] %s\n' "$SCEN" "$1"; }
fail() { FAILED=$((FAILED + 1)); printf 'FAIL  [%s] %s\n' "$SCEN" "$1"; detail; }
skip() { SKIPPED=$((SKIPPED + 1)); printf 'SKIP  [%s] %s\n' "$SCEN" "$1"; }
check() { if [ "$2" = 0 ]; then pass "$1"; else fail "$1"; fi; }
has() { printf '%s\n' "$OUT" | grep -F -- "$1" >/dev/null 2>&1; }
hasnt() { if has "$1"; then return 1; else return 0; fi; }

# ver_le A B: version A <= version B (leading v, pre-release suffix ignored).
ver_le() {
    printf '%s %s\n' "${1#v}" "${2#v}" | awk '{
        split($1, a, "."); split($2, b, ".");
        for (i = 1; i <= 3; i++) { x = a[i] + 0; y = b[i] + 0; if (x < y) exit 0; if (x > y) exit 1 }
        exit 0 }'
}

# known <id> <step text>: a failure of a bug fixed after the release under test → KNOWN.
# Returns 1 (a real FAIL) when the release should already contain the fix.
#   setup-no-service  `kioku setup --no-service` exited 1 when no server ran yet (auth xx);
#                     fixed after v0.9.4 (auth is `--` then).
#   doctor-image      `docker exec … kioku doctor` in the image (no config.toml, token in
#                     KIOKU_AUTH_TOKEN) failed `config`; fixed after v0.9.4.
known() {
    case "$1" in
        setup-no-service | doctor-image) fixed_after=v0.9.4 ;;
        *) return 1 ;;
    esac
    if ver_le "$TAG" "$fixed_after"; then
        KNOWN=$((KNOWN + 1))
        printf 'KNOWN [%s] %s (bug fixed after %s; release under test is %s)\n' "$SCEN" "$2" "$fixed_after" "$TAG"
        return 0
    fi
    return 1
}

# ------------------------------------------------------------------------------ machines

# home_of <machine>: its HOME.
home_of() {
    if [ "$MODE" = docker ]; then printf '/home/dev'; else printf '%s/%s' "$BOXES" "$1"; fi
}

# host_of <server machine>: the address clients use for it.
host_of() {
    if [ "$MODE" = docker ]; then printf '%s' "$1"; else printf '127.0.0.1'; fi
}

LOCALE=C.UTF-8
[ "$(uname -s)" != Darwin ] || LOCALE=en_US.UTF-8

# inside <machine> <shell command>: run as the machine's user, in its HOME. Host mode: a
# clean environment (env -i), so nothing of the real user's kioku can be touched.
inside() {
    RC=0
    if [ "$MODE" = docker ]; then
        OUT=$(docker exec -u dev -w /home/dev -e "KIOKU_VERSION=$TAG" "$1" sh -c "$2" 2>&1) || RC=$?
    else
        OUT=$(env -i HOME="$(home_of "$1")" USER="$(id -un)" LOGNAME="$(id -un)" SHELL=/bin/sh \
            PATH=/usr/bin:/bin:/usr/sbin:/sbin LANG=$LOCALE TMPDIR="$WORK/tmp" KIOKU_VERSION="$TAG" \
            KIOKU_MACHINE="$1" KIOKU_PORT="$PORT" sh -c 'cd "$HOME" && eval "$1"' sh "$2" 2>&1) || RC=$?
    fi
    printf '%s\n' "$OUT" >>"$WORK/all.log"
}

# inside_in <machine> <file> <shell command>: same, with the file on stdin.
inside_in() {
    RC=0
    if [ "$MODE" = docker ]; then
        OUT=$(docker exec -i -u dev -w /home/dev -e "KIOKU_VERSION=$TAG" "$1" sh -c "$3" <"$2" 2>&1) || RC=$?
    else
        OUT=$(env -i HOME="$(home_of "$1")" USER="$(id -un)" LOGNAME="$(id -un)" SHELL=/bin/sh \
            PATH=/usr/bin:/bin:/usr/sbin:/sbin LANG=$LOCALE TMPDIR="$WORK/tmp" KIOKU_VERSION="$TAG" \
            KIOKU_MACHINE="$1" KIOKU_PORT="$PORT" sh -c 'cd "$HOME" && eval "$1"' sh "$3" <"$2" 2>&1) || RC=$?
    fi
    printf '%s\n' "$OUT" >>"$WORK/all.log"
}

# new_box <name> <image>: a pristine machine. Docker: a container with curl, git,
# ca-certificates and a non-root user `dev`. Host: an empty HOME.
new_box() {
    if [ "$MODE" = host ]; then
        mkdir -p "$BOXES/$1"
        return 0
    fi
    docker run -d --name "$1" --hostname "$1" --network "$NET" --label "kioku-e2e=$RUN" \
        "$2" sleep infinity >/dev/null
    case "$2" in
        alpine*)
            docker exec "$1" sh -c 'apk add -q --no-cache curl git ca-certificates >/dev/null &&
                adduser -D -s /bin/sh dev' >/dev/null 2>&1
            ;;
        *)
            docker exec -e DEBIAN_FRONTEND=noninteractive "$1" sh -c 'apt-get update -qq >/dev/null &&
                apt-get install -y -qq --no-install-recommends curl git ca-certificates >/dev/null 2>&1 &&
                useradd -m -s /bin/sh dev' >/dev/null 2>&1
            ;;
    esac
}

# start_serve <server machine>: `kioku serve` in the background, as the server's user.
# Auto-update off: a pinned older tag must not replace itself during the run.
start_serve() {
    if [ "$MODE" = docker ]; then
        docker exec -d -u dev -w /home/dev -e KIOKU_AUTO_UPDATE=0 "$1" \
            sh -c 'exec .local/bin/kioku serve --log-file .kioku/logs/serve.log' >/dev/null
    else
        env -i HOME="$(home_of "$1")" USER="$(id -un)" PATH=/usr/bin:/bin:/usr/sbin:/sbin \
            LANG=$LOCALE TMPDIR="$WORK/tmp" KIOKU_AUTO_UPDATE=0 KIOKU_PORT="$PORT" \
            sh -c 'cd "$HOME" && exec .local/bin/kioku serve --log-file .kioku/logs/serve.log' \
            >/dev/null 2>&1 &
        echo $! >"$WORK/serve.pid"
    fi
}

# wait_health <server machine>: 0 when /api/v1/health answers within 30 s.
wait_health() {
    i=0
    while [ "$i" -lt 60 ]; do
        if [ "$MODE" = docker ]; then
            docker exec "$1" curl -fsS "http://127.0.0.1:$PORT/api/v1/health" >/dev/null 2>&1 && return 0
        else
            curl -fsS -m 2 "http://127.0.0.1:$PORT/api/v1/health" >/dev/null 2>&1 && return 0
        fi
        sleep 1
        i=$((i + 1))
    done
    return 1
}

# doctor <machine> <allowed warn ids> [known-id]: `kioku doctor` in a shell that sourced
# ~/.profile (the installer's PATH line) exits 0 with only the allowed warnings.
doctor() {
    inside "$1" '. ./.profile; kioku doctor'
    judge_doctor "$@"
}

# judge_doctor <label> <allowed warn ids> [known-id]: judges the doctor output in OUT / RC.
judge_doctor() {
    warns=$(printf '%s\n' "$OUT" | sed -n 's/^\[WARN\] \([^:]*\):.*/\1/p' | tr '\n' ' ' | sed 's/ *$//')
    fails=$(printf '%s\n' "$OUT" | sed -n 's/^\[FAIL\] \([^:]*\):.*/\1/p' | tr '\n' ' ' | sed 's/ *$//')
    unexpected=""
    for w in $warns; do
        case " $2 " in *" $w "*) ;; *) unexpected="$unexpected $w" ;; esac
    done
    summary=$(printf '%s\n' "$OUT" | tail -n 1)
    if [ "$RC" = 0 ] && [ -z "$fails" ] && [ -z "$unexpected" ]; then
        pass "$1: kioku doctor exit 0 ($summary; allowed warnings: ${2:-none}${warns:+, seen: $warns})"
    elif [ -n "${3:-}" ] && known "$3" "$1: kioku doctor exit $RC (fail: $fails)"; then
        :
    else
        fail "$1: kioku doctor exit $RC (fail: ${fails:-none}; unexpected warnings:${unexpected:- none})"
    fi
}

# project <machine> <origin url>: ~/src/demo, a git repo on main with that origin.
project() {
    inside "$1" "mkdir -p src/demo && cd src/demo && git init -q -b main &&
        git remote add origin '$2' && printf '# demo\n' >README.md && git add README.md &&
        git -c user.name=e2e -c user.email=e2e@example.invalid commit -q -m init"
    check "$1: test repository ~/src/demo (origin $2)" "$RC"
}

# hook <machine> <event> <payload file>: what Claude Code runs, in the project directory.
hook() {
    inside_in "$1" "$3" "cd src/demo && \"\$HOME/.local/bin/kioku\" hook $2 --agent claude-code"
}

# fixture <machine> <name> <session>: crates/kioku-cli/tests/fixtures/<name>.json (captured
# Claude Code shape) re-pointed at that machine's project and session.
fixture() {
    h=$(home_of "$1")
    slug=$(printf '%s' "$h/src/demo" | tr '/' '-')
    sed -e "s#8d3c1f0e-5b7a-4c2d-9e1f-0a2b3c4d5e6f#$3#g" \
        -e "s#/home/u/.claude/projects/-home-u-kioku#$h/.claude/projects/$slug#g" \
        -e "s#/home/u/kioku#$h/src/demo#g" "$FIX/$2.json" >"$WORK/$2-$3.json"
    printf '%s' "$WORK/$2-$3.json"
}

# payload <machine> <session> <event> <extra json members>: a Claude Code payload.
payload() {
    h=$(home_of "$1")
    slug=$(printf '%s' "$h/src/demo" | tr '/' '-')
    f="$WORK/$2-$3.json"
    printf '{"session_id":"%s","transcript_path":"%s/.claude/projects/%s/%s.jsonl","cwd":"%s/src/demo","permission_mode":"default","hook_event_name":"%s"%s}\n' \
        "$2" "$h" "$slug" "$2" "$h" "$3" "$4" >"$f"
    printf '%s' "$f"
}

# The `<kioku>` block's project id ("project: name (id: <id>)").
project_id() { printf '%s\n' "$OUT" | sed -n 's/^project: .*(id: \([^)]*\)).*/\1/p' | head -n 1; }

# mcp_handoff <machine> <project> <session>: kioku_handoff_write through the MCP server
# registered in ~/.claude.json (the `kioku mcp` stdio bridge), as Claude Code would call it.
mcp_handoff() {
    cat >"$WORK/mcp.sh" <<'EOF_MCP'
set -u
cmd=$(sed -n 's/^ *"command": *"\(.*kioku\)",*$/\1/p' "$HOME/.claude.json" | head -n 1)
[ -n "$cmd" ] || { echo "no kioku MCP server in ~/.claude.json"; exit 3; }
grep -q '"mcp"' "$HOME/.claude.json" || { echo "the kioku MCP entry does not run 'mcp'"; exit 3; }
cd "$HOME/src/demo"
out="$HOME/.mcp-e2e.out"
: >"$out"
{
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"claude-code-e2e","version":"0"}}}'
    printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
    printf '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"kioku_handoff_write","arguments":{"project":"%s","session":"%s","summary":"認証ミドルウェアのトークン検証を auth.rs に集約した（E2E-HANDOFF-MARKER）","next_steps":["auth.rs の単体テストを追加する"],"open_questions":["期限切れトークンの扱い"],"decisions":["トークン検証は auth.rs に一本化する"]}}}\n' "$1" "$2"
    i=0
    while [ "$i" -lt 30 ] && ! grep -q '"id":2' "$out"; do sleep 1; i=$((i + 1)); done
} | "$cmd" mcp >"$out" 2>/dev/null
cat "$out"
rm -f "$out"
EOF_MCP
    inside_in "$1" "$WORK/mcp.sh" "sh -s '$2' '$3'"
}

# ------------------------------------------------------------------------------ release

if [ -n "${KIOKU_RELEASE_TAG:-}" ]; then
    TAG=$KIOKU_RELEASE_TAG
    case "$TAG" in v*) ;; *) TAG="v$TAG" ;; esac
    IMAGE="ghcr.io/$REPO:$TAG"
else
    url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") ||
        { echo "cannot resolve the latest release of $REPO" >&2; exit 2; }
    TAG=${url##*/tag/}
    IMAGE="ghcr.io/$REPO:latest"
fi
VERSION=${TAG#v}
if [ "$MODE" = docker ]; then
    printf 'kioku clean-install e2e (docker %s/%s): release %s, image %s, run %s\n\n' \
        "$(docker version --format '{{.Server.Os}}' 2>/dev/null)" \
        "$(docker version --format '{{.Server.Arch}}' 2>/dev/null)" "$TAG" "$IMAGE" "$RUN"
    docker network create --label "kioku-e2e=$RUN" "$NET" >/dev/null
else
    printf 'kioku clean-install e2e (host %s %s, HOMEs under %s, port %s): release %s, run %s\n\n' \
        "$(uname -s)" "$(uname -m)" "$BOXES" "$PORT" "$TAG" "$RUN"
fi

# ------------------------------------------------------------------------------ clients

# join_client <machine> <image> <invite line>
join_client() {
    new_box "$1" "$2"
    h=$(home_of "$1")
    inside "$1" 'mkdir -p .claude && test ! -e .kioku && ! command -v kioku && ! command -v cargo'
    check "$1 (${MODE_LABEL:-$2}): pristine (no kioku, no Rust on PATH), Claude Code dir ~/.claude present" "$RC"
    # The line exactly as printed by `kioku invite`, pasted into the user's shell.
    printf '%s\n' "$3" >"$WORK/line"
    inside_in "$1" "$WORK/line" 'sh -s'
    if [ "$RC" = 0 ] && has "kioku is ready - restart Claude Code." && has "installed kioku $VERSION to $h/.local/bin/kioku" &&
        has "ok  claude-code hooks ~/.claude/settings.json (6), MCP ~/.claude.json"; then
        pass "$1: pasted the invite line -> kioku $VERSION installed, joined, Claude Code hooks + MCP set up"
    else
        fail "$1: pasted the invite line (exit $RC)"
    fi
    inside "$1" 'sh -lc "command -v kioku && kioku --version"'
    if [ "$RC" = 0 ] && has "$h/.local/bin/kioku" && has "kioku $VERSION"; then r=0; else r=1; fi
    check "$1: a new login shell finds kioku on PATH (installer's ~/.profile line)" "$r"
    inside "$1" 'cat .claude/settings.json'
    r=0
    for ev in session-start user-prompt-submit post-tool-use stop pre-compact session-end; do
        has "$h/.local/bin/kioku hook $ev" || r=1
    done
    check "$1: ~/.claude/settings.json runs \`kioku hook <event>\` for all 6 events" "$r"
}

# invite_line <server machine>: LINE = the macOS / Linux line of the `kioku invite` output in
# OUT (RC = its exit code); 0 when it has the documented shape.
invite_line() {
    LINE=$(printf '%s\n' "$OUT" | sed -n 's/^  macOS \/ Linux \/ Git Bash:  //p' | head -n 1)
    if [ "$RC" = 0 ] && [ -n "$LINE" ] &&
        printf '%s' "$LINE" | grep -q "^KIOKU_JOIN='$(host_of "$1"):$PORT/[A-Za-z0-9]*' sh -c \"\$(curl -fsSL $INSTALL_SH_URL)\"\$"; then
        pass "$1: kioku invite --host $(host_of "$1") --uses 2 printed the macOS / Linux line: $(printf '%s' "$LINE" | mask)"
        return 0
    fi
    fail "$1: kioku invite printed no usable macOS / Linux line (exit $RC)"
    return 1
}

# client_flow <server machine> <invite line> <client A> <client B>
client_flow() {
    srv=$1
    line=$2
    a=$3
    b=$4
    url="http://$(host_of "$srv"):$PORT"

    join_client "$a" debian:bookworm-slim "$line"
    join_client "$b" alpine:3 "$line"

    # Two clones of one repository: HTTPS on A, SSH on B → one project id.
    project "$a" "https://github.com/example/kioku-e2e-demo.git"
    project "$b" "git@github.com:example/kioku-e2e-demo.git"

    # --- client A: a Claude Code session (repo fixtures; Japanese prompt with a fake secret)
    sa="e2e-$a-1"
    hook "$a" session-start "$(fixture "$a" session_start "$sa")"
    pid=$(project_id)
    if [ "$RC" = 0 ] && has "<kioku>" && has "session: $sa" && has "server: $url" && [ -n "$pid" ]; then
        pass "$a: SessionStart -> <kioku> block (project $pid, session $sa, server $url)"
    else
        fail "$a: SessionStart (exit $RC)"
    fi
    hook "$a" user-prompt-submit "$(fixture "$a" user_prompt_submit "$sa")"
    check "$a: UserPromptSubmit (Japanese prompt) exit 0" "$RC"
    hook "$a" post-tool-use "$(fixture "$a" post_tool_use "$sa")"
    check "$a: PostToolUse (Edit) exit 0" "$RC"
    hook "$a" post-tool-use "$(payload "$a" "$sa" PostToolUse ',"tool_name":"Bash","tool_input":{"command":"cargo test -p kioku-core auth","description":"認証のテスト"},"tool_response":{"stdout":"test result: ok. 3 passed","stderr":"","interrupted":false,"isImage":false},"tool_use_id":"toolu_e2e_bash"')"
    check "$a: PostToolUse (Bash) exit 0" "$RC"
    mcp_handoff "$a" "$pid" "$sa"
    if [ "$RC" = 0 ] && has '"id":2' && has "handoff recorded for $pid" && hasnt '"isError":true'; then r=0; else r=1; fi
    check "$a: kioku_handoff_write through the registered \`kioku mcp\` bridge (Japanese summary)" "$r"
    hook "$a" stop "$(fixture "$a" stop "$sa")"
    check "$a: Stop exit 0 (finalized; no nudge after the handoff)" "$RC"

    # --- client B: the next session, on the other machine, gets A's handoff
    sb="e2e-$b-1"
    hook "$b" session-start "$(payload "$b" "$sb" SessionStart ',"source":"startup"')"
    if [ "$RC" = 0 ] && has "<kioku>" && has "(id: $pid)" && has "## 前回からの引き継ぎ" &&
        has "認証ミドルウェアのトークン検証を auth.rs に集約した（E2E-HANDOFF-MARKER）" &&
        has "auth.rs の単体テストを追加する" && has "@$a"; then
        pass "$b: SessionStart shows $a's handoff (summary, next step, same project via SSH remote, @$a in recent sessions)"
    else
        fail "$b: SessionStart does not show $a's handoff (exit $RC)"
    fi
    inside "$b" '. ./.profile; kioku search 引き継ぎ書 --project '"$pid"
    if [ "$RC" = 0 ] && has "引き継ぎ書" && has "@$a" && hasnt "sk-live_abcdefghijklmnop1234"; then r=0; else r=1; fi
    check "$b: Japanese kioku search (引き継ぎ書) finds $a's session; the fake secret is redacted" "$r"
    hook "$b" user-prompt-submit "$(payload "$b" "$sb" UserPromptSubmit ',"prompt":"期限切れトークンのテストを追加して（E2E-RULES-MARKER）"')"
    check "$b: UserPromptSubmit (Japanese) exit 0" "$RC"
    hb=$(home_of "$b")
    hook "$b" post-tool-use "$(payload "$b" "$sb" PostToolUse ',"tool_name":"Write","tool_input":{"file_path":"'"$hb"'/src/demo/tests/auth_expiry.rs","content":"// 期限切れ\n"},"tool_response":{"filePath":"'"$hb"'/src/demo/tests/auth_expiry.rs","type":"create"},"tool_use_id":"toolu_e2e_write"')"
    check "$b: PostToolUse (Write) exit 0" "$RC"
    hook "$b" stop "$(payload "$b" "$sb" Stop ',"stop_hook_active":false,"last_assistant_message":"期限切れトークンのテストを追加しました。"')"
    check "$b: Stop exit 0 (no agent handoff: kioku writes the rules handoff)" "$RC"

    # --- back on client A: the automatic handoff of B's session
    hook "$a" session-start "$(payload "$a" "e2e-$a-2" SessionStart ',"source":"startup"')"
    if [ "$RC" = 0 ] && has "## 前回からの引き継ぎ" && has "E2E-RULES-MARKER" && has "@$b"; then r=0; else r=1; fi
    check "$a: next SessionStart shows $b's automatic handoff (last prompt, @$b)" "$r"

    doctor "$a" "$ALLOWED_WARN_CLIENT"
    doctor "$b" "$ALLOWED_WARN_CLIENT"
}

# ------------------------------------------------------------------------------ install

scenario_install() {
    SCEN=install
    srv="$RUN-server"
    new_box "$srv" ubuntu:24.04
    h=$(home_of "$srv")
    inside "$srv" 'test ! -e .kioku && ! command -v kioku && ! command -v cargo && test "$(id -u)" != 0'
    check "$srv (${MODE_LABEL:-ubuntu:24.04}): pristine, non-root user, no kioku, no Rust on PATH" "$RC"

    # README "Home server": the one-liner without a service (no systemd in a container; no
    # LaunchAgent for a throwaway HOME). Docker: reachable from the other containers.
    bind=0.0.0.0
    [ "$MODE" = docker ] || bind=127.0.0.1
    inside "$srv" "curl -fsSL $INSTALL_SH_URL | sh -s -- --bind $bind --no-service --no-agents"
    if [ "$RC" = 0 ] && has "installed kioku $VERSION to $h/.local/bin/kioku" && has "--  service     skipped (--no-service)"; then
        pass "$srv: one-liner installed kioku $VERSION (--bind $bind --no-service --no-agents), setup exit 0"
    elif has "installed kioku $VERSION to $h/.local/bin/kioku" && has "xx  auth" &&
        known setup-no-service "$srv: one-liner installed kioku $VERSION, but setup exited $RC (auth: no server yet)"; then
        :
    else
        fail "$srv: one-liner (exit $RC)"
        return 0
    fi
    inside "$srv" "grep -c '^bind = \"$bind\"' .kioku/config.toml"
    check "$srv: config.toml binds $bind (only that line is read; the token is never printed)" "$RC"

    start_serve "$srv"
    if wait_health "$srv"; then r=0; else r=1; fi
    inside "$srv" 'tail -n 20 .kioku/logs/serve.log'
    check "$srv: kioku serve answers /api/v1/health" "$r"
    inside "$srv" '. ./.profile; kioku setup --no-service --no-agents'
    if [ "$RC" = 0 ] && has "ok  auth        token accepted by http://127.0.0.1:$PORT"; then r=0; else r=1; fi
    check "$srv: kioku setup again (idempotent): token accepted" "$r"

    inside "$srv" ". ./.profile; kioku invite --host $(host_of "$srv") --uses 2"
    invite_line "$srv" || return 0
    if [ "$MODE" = docker ]; then
        client_flow "$srv" "$LINE" "$RUN-deb" "$RUN-alpine"
    else
        client_flow "$srv" "$LINE" "$RUN-a" "$RUN-b"
    fi
    doctor "$srv" "$ALLOWED_WARN_SERVER"
}

# ------------------------------------------------------------------------------ image

scenario_image() {
    SCEN=image
    if ! docker pull -q "$IMAGE" >/dev/null 2>&1; then
        skip "cannot pull $IMAGE (offline, rate-limited or not published): image scenario skipped"
        return 0
    fi
    srv="$RUN-image"
    IMG_TOKEN=$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')
    docker run -d --name "$srv" --hostname "$srv" --network "$NET" --label "kioku-e2e=$RUN" \
        -e "KIOKU_AUTH_TOKEN=$IMG_TOKEN" -v "$RUN-data:/data" "$IMAGE" >/dev/null
    if wait_health "$srv"; then r=0; else r=1; fi
    OUT=$(docker logs "$srv" 2>&1 | tail -n 20)
    check "$srv ($IMAGE): kioku serve answers /api/v1/health" "$r"
    RC=0
    OUT=$(docker exec "$srv" kioku --version 2>&1) || RC=$?
    if [ "$OUT" = "kioku $VERSION" ]; then
        pass "$srv: image runs kioku $VERSION"
    else
        fail "$srv: image runs '$OUT', expected kioku $VERSION (:latest not yet pushed for $TAG?)"
    fi
    # README "Docker": `docker exec kioku kioku invite` (as the image's own user).
    RC=0
    OUT=$(docker exec "$srv" kioku invite --host "$srv" --uses 2 2>&1) || RC=$?
    invite_line "$srv" || return 0
    client_flow "$srv" "$LINE" "$RUN-img-deb" "$RUN-img-alpine"
    RC=0
    OUT=$(docker exec "$srv" kioku doctor 2>&1) || RC=$?
    judge_doctor "$srv" "$ALLOWED_WARN_IMAGE" doctor-image
    # Nothing the image or the clients printed may contain the server's token.
    docker logs "$srv" >>"$WORK/all.log" 2>&1 || true
    if grep -F -- "$IMG_TOKEN" "$WORK/all.log" >/dev/null; then r=1; else r=0; fi
    OUT=""
    check "$srv: the token appears nowhere (server log, install / join / hook / doctor output)" "$r"
}

if [ "$MODE" = host ]; then
    MODE_LABEL="host $(uname -s), separate HOME"
    [ "$ONLY" = image ] || scenario_install
    [ "$ONLY" != image ] || { SCEN=image; skip "--host: the Docker image scenario needs Docker"; }
else
    [ "$ONLY" = image ] || scenario_install
    [ "$ONLY" = install ] || scenario_image
fi

printf '\nrelease %s: %s passed, %s failed, %s known (fixed after the release), %s skipped\n' \
    "$TAG" "$PASSED" "$FAILED" "$KNOWN" "$SKIPPED"
[ "$FAILED" = 0 ]
