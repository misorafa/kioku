#!/bin/sh
# Signature / notarization check of the kioku binary that install.sh installs on macOS (the
# public release path), run by .github/workflows/clean-install.yml on macos-latest. Installs
# into a throwaway HOME (env -i; the real ~/.local/bin and ~/.kioku are never touched):
#
#   sh scripts/e2e/macos-signature.sh             # latest release
#   KIOKU_RELEASE_TAG=v0.9.4 sh scripts/e2e/macos-signature.sh
#
# Checks: codesign --verify --strict; Developer ID Application authority, team 7F6HLTW75D,
# hardened runtime; Gatekeeper's notarization verdict (`spctl --assess --type open --context
# context:primary-signature` → "source=Notarized Developer ID"); no quarantine attribute on
# the installed file. `spctl --assess --type execute` is printed
# for the record only: it rejects every bare command-line tool ("does not seem to be an app"),
# notarized or not.
set -eu

[ "$(uname -s)" = Darwin ] || { echo "macOS only" >&2; exit 2; }
TEAM=7F6HLTW75D
WORK=$(mktemp -d)
WORK=$(cd -P "$WORK" && pwd)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/home"

PASSED=0
FAILED=0
check() { # <name> <0 = pass> [details]
    if [ "$2" = 0 ]; then
        PASSED=$((PASSED + 1))
        printf 'PASS  [macos] %s\n' "$1"
    else
        FAILED=$((FAILED + 1))
        printf 'FAIL  [macos] %s\n' "$1"
        printf '%s\n' "${3:-}" | sed 's/^/      | /'
    fi
}

ver=${KIOKU_RELEASE_TAG:-latest}
rc=0
out=$(env -i HOME="$WORK/home" PATH=/usr/bin:/bin:/usr/sbin:/sbin KIOKU_VERSION="$ver" \
    sh -c 'curl -fsSL https://raw.githubusercontent.com/misorafa/kioku/main/install.sh | sh -s -- --no-setup --no-modify-path' 2>&1) || rc=$?
BIN="$WORK/home/.local/bin/kioku"
r=1; if [ "$rc" = 0 ] && [ -x "$BIN" ]; then r=0; fi
check "install.sh --no-setup installed $("$BIN" --version 2>/dev/null || echo nothing) ($(uname -m))" "$r" "$out"
[ "$r" = 0 ] || { printf '\n%s passed, %s failed\n' "$PASSED" "$FAILED"; exit 1; }

rc=0
out=$(codesign --verify --strict --verbose=2 "$BIN" 2>&1) || rc=$?
check "codesign --verify --strict" "$rc" "$out"

out=$(codesign -dv --verbose=2 "$BIN" 2>&1 || true)
r=0
printf '%s\n' "$out" | grep -q '^Authority=Developer ID Application: .*('"$TEAM"')$' || r=1
printf '%s\n' "$out" | grep -q "^TeamIdentifier=$TEAM\$" || r=1
printf '%s\n' "$out" | grep -q 'flags=0x10000(runtime)' || r=1
printf '%s\n' "$out" | grep -q '^Timestamp=' || r=1
check "Developer ID Application ($TEAM), hardened runtime, secure timestamp" "$r" "$out"

rc=0
out=$(spctl --assess --type open --context context:primary-signature -vv "$BIN" 2>&1) || rc=$?
r=1; if [ "$rc" = 0 ] && printf '%s' "$out" | grep -q 'source=Notarized Developer ID'; then r=0; fi
check "Gatekeeper: notarized (spctl … context:primary-signature -> Notarized Developer ID)" "$r" "$out"

rc=0
out=$(spctl --assess --type execute -vv "$BIN" 2>&1) || rc=$?
printf 'INFO  [macos] spctl --assess --type execute (exit %s, not judged; bare CLI tools are never "apps"): %s\n' \
    "$rc" "$(printf '%s' "$out" | tr '\n' ' ')"

r=0
xattr -p com.apple.quarantine "$BIN" >/dev/null 2>&1 && r=1
check "install.sh leaves no quarantine attribute (curl download: no Gatekeeper first-run prompt)" "$r"
# Not tested: running a *quarantined* copy. On a desktop session Gatekeeper then waits for
# the user's "Open" confirmation (it blocked when this script was first run on a Mac), which
# no unattended test can answer.

printf '\n%s passed, %s failed\n' "$PASSED" "$FAILED"
[ "$FAILED" = 0 ]
