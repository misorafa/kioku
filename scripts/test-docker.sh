#!/bin/sh
# Smoke test of the Docker image (SPEC-M3.3 §2), run by CI on ubuntu with a locally built
# linux binary (never a release asset):
#
#   sh scripts/test-docker.sh target/debug/kioku
#
# Builds the image from Dockerfile with the binary as `linux/<arch>/kioku`, then checks:
# `kioku --version`; non-root user, /data volume, KIOKU_DATA_DIR, port 7391; `kioku init`
# in the container prints the token it generates (once) and honours KIOKU_AUTH_TOKEN; a
# `serve` container answers /api/v1/health and Docker reports it healthy. Removes its
# containers, volumes and image afterwards.
set -eu

[ $# -ge 1 ] || { echo "usage: test-docker.sh <linux kioku binary>" >&2; exit 2; }
BIN=$1
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d 2>/dev/null || mktemp -d -t kioku-docker)
IMG="kioku-smoke:$$"
NAME="kioku-smoke-$$"
VOL="kioku-smoke-data-$$"
cleanup() {
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    docker volume rm -f "$VOL" "$VOL-2" >/dev/null 2>&1 || true
    docker image rm -f "$IMG" >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
trap cleanup EXIT

PASSED=0
FAILED=0
check() { # <name> <0 = pass> [details]
    if [ "$2" = 0 ]; then
        PASSED=$((PASSED + 1))
        printf 'ok   %s\n' "$1"
    else
        FAILED=$((FAILED + 1))
        printf 'FAIL %s\n' "$1"
        [ -z "${3:-}" ] || printf '%s\n' "$3" | sed 's/^/     | /'
    fi
}

case "$(uname -m)" in
    x86_64 | amd64) ARCH=amd64 ;;
    aarch64 | arm64) ARCH=arm64 ;;
    *) echo "unsupported architecture $(uname -m)" >&2; exit 2 ;;
esac
mkdir -p "$WORK/ctx/linux/$ARCH"
cp "$BIN" "$WORK/ctx/linux/$ARCH/kioku"
docker build -q -f "$ROOT/Dockerfile" -t "$IMG" "$WORK/ctx" >/dev/null

rc=0
out=$(docker run --rm "$IMG" --version 2>&1) || rc=$?
case "$out" in kioku\ *) r=$rc ;; *) r=1 ;; esac
check "docker run … kioku --version ($out)" "$r" "$out"

cfg=$(docker image inspect "$IMG" --format '{{.Config.User}}|{{json .Config.Volumes}}|{{json .Config.ExposedPorts}}|{{json .Config.Env}}|{{json .Config.Healthcheck.Test}}')
r=0
for want in 'kioku|' '"/data":{}' '"7391/tcp":{}' 'KIOKU_DATA_DIR=/data' 'KIOKU_CONTAINER=1' '/api/v1/health'; do
    case "$cfg" in *"$want"*) ;; *) r=1 ;; esac
done
check 'image: non-root user, VOLUME /data, KIOKU_DATA_DIR=/data, EXPOSE 7391, health check' "$r" "$cfg"

uid=$(docker run --rm --entrypoint id "$IMG" -u)
r=1; if [ "$uid" = 10001 ]; then r=0; fi
check "runs as uid 10001 (got $uid)" "$r"

rc=0
out=$(docker run --rm -v "$VOL:/data" "$IMG" init 2>&1) || rc=$?
tok=$(printf '%s\n' "$out" | sed -n 's/^  token    : \([0-9a-f]\{16,\}\)$/\1/p')
if [ "$rc" = 0 ] && [ -n "$tok" ] && printf '%s' "$out" | grep -q 'shown only this once'; then r=0; else r=1; fi
check 'kioku init in a container prints the generated token once' "$r" "$out"
rc=0
out2=$(docker run --rm -v "$VOL:/data" "$IMG" init 2>&1) || rc=$?
if [ "$rc" = 0 ] && [ -n "$tok" ] && ! printf '%s' "$out2" | grep -q "$tok"; then r=0; else r=1; fi
check 'a second init keeps the token and does not print it' "$r" "$out2"
rc=0
out=$(docker run --rm -e KIOKU_AUTH_TOKEN=provisioned-0123456789abcdef -v "$VOL-2:/data" "$IMG" init 2>&1) || rc=$?
if [ "$rc" = 0 ] && printf '%s' "$out" | grep -q 'token    : from KIOKU_AUTH_TOKEN' &&
    ! printf '%s' "$out" | grep -q 'provisioned-0123456789abcdef'; then r=0; else r=1; fi
check 'KIOKU_AUTH_TOKEN provisions the token non-interactively (never echoed)' "$r" "$out"

docker run -d --name "$NAME" -p 127.0.0.1::7391 -e KIOKU_AUTH_TOKEN=smoke-0123456789abcdef \
    -v "$VOL:/data" "$IMG" >/dev/null
port=$(docker port "$NAME" 7391/tcp | head -n 1 | sed 's/.*://')
ok=1
i=0
while [ "$i" -lt 60 ]; do
    if curl -fsS "http://127.0.0.1:$port/api/v1/health" >/dev/null 2>&1; then ok=0; break; fi
    i=$((i + 1))
    sleep 1
done
check "serve answers /api/v1/health on the published port" "$ok" "$(docker logs "$NAME" 2>&1 | tail -n 20)"
health=starting
i=0
while [ "$i" -lt 60 ] && [ "$health" = starting ]; do
    health=$(docker inspect --format '{{.State.Health.Status}}' "$NAME")
    [ "$health" = starting ] || break
    i=$((i + 1))
    sleep 1
done
r=1; if [ "$health" = healthy ]; then r=0; fi
check "Docker reports the container healthy (got $health)" "$r" "$(docker inspect --format '{{json .State.Health}}' "$NAME")"
logs=$(docker logs "$NAME" 2>&1)
if printf '%s' "$logs" | grep -q 'smoke-0123456789abcdef'; then r=1; else r=0; fi
check 'the token is not in the server log' "$r"

printf '\npassed: %s, failed: %s\n' "$PASSED" "$FAILED"
[ "$FAILED" = 0 ]
