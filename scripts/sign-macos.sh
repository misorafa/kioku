#!/bin/sh
# sign-macos.sh — sign (and notarize) the macOS kioku binary in the release workflow.
#
# SPEC-M2 §10.3.1: with a Developer ID the signature carries the team identity, so firewalls
# (Little Snitch, LuLu) and macOS Local Network privacy keep recognising kioku across
# updates. Without the secrets (forks, first setup) it falls back to an ad-hoc signature
# with the stable identifier dev.kioku.kioku, as before.
#
# Usage: scripts/sign-macos.sh <binary>
# Env (GitHub secrets, all optional):
#   MACOS_CERT_P12       base64 of the "Developer ID Application" certificate (.p12)
#   MACOS_CERT_PASSWORD  its export password
#   NOTARY_KEY_P8        base64 of an App Store Connect API key (.p8)
#   NOTARY_KEY_ID        the key's id
#   NOTARY_ISSUER_ID     the issuer id
set -eu

BIN=$1
IDENT=dev.kioku.kioku

if [ -z "${MACOS_CERT_P12:-}" ] || [ -z "${MACOS_CERT_PASSWORD:-}" ]; then
    echo "sign-macos: no Developer ID secrets; ad-hoc signature ($IDENT)"
    codesign -s - --force --identifier "$IDENT" "$BIN"
    codesign -dv "$BIN" 2>&1 | grep -q "Identifier=$IDENT"
    exit 0
fi

WORK=$(mktemp -d)
KEYCHAIN="$WORK/kioku-signing.keychain-db"
KEYCHAIN_PASS=$(openssl rand -hex 16)
cleanup() {
    security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
trap cleanup EXIT

printf '%s' "$MACOS_CERT_P12" | base64 --decode >"$WORK/cert.p12"
security create-keychain -p "$KEYCHAIN_PASS" "$KEYCHAIN"
security set-keychain-settings -lut 3600 "$KEYCHAIN"
security unlock-keychain -p "$KEYCHAIN_PASS" "$KEYCHAIN"
security import "$WORK/cert.p12" -k "$KEYCHAIN" -P "$MACOS_CERT_PASSWORD" -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple: -s -k "$KEYCHAIN_PASS" "$KEYCHAIN" >/dev/null
# Make the temporary keychain searchable (keep the existing ones).
security list-keychains -d user -s "$KEYCHAIN" $(security list-keychains -d user | tr -d '"')

SIGNER=$(security find-identity -v -p codesigning "$KEYCHAIN" |
    sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -n 1)
[ -n "$SIGNER" ] || { echo "sign-macos: no Developer ID Application identity in the .p12" >&2; exit 1; }

# Hardened runtime + secure timestamp: required for notarization.
codesign --force --options runtime --timestamp --identifier "$IDENT" \
    --keychain "$KEYCHAIN" --sign "$SIGNER" "$BIN"
codesign --verify --strict --verbose=2 "$BIN"
codesign -dv "$BIN" 2>&1 | grep -E '^(Identifier|TeamIdentifier|Authority=Developer ID)'

if [ -n "${NOTARY_KEY_P8:-}" ] && [ -n "${NOTARY_KEY_ID:-}" ] && [ -n "${NOTARY_ISSUER_ID:-}" ]; then
    printf '%s' "$NOTARY_KEY_P8" | base64 --decode >"$WORK/key.p8"
    ditto -c -k --keepParent "$BIN" "$WORK/kioku.zip"
    # A bare binary cannot be stapled; Gatekeeper looks the ticket up online.
    xcrun notarytool submit "$WORK/kioku.zip" \
        --key "$WORK/key.p8" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID" \
        --wait --timeout 20m --output-format json | tee "$WORK/notary.json"
    grep -q '"status" *: *"Accepted"' "$WORK/notary.json" ||
        { echo "sign-macos: notarization was not accepted" >&2; exit 1; }
else
    echo "sign-macos: signed with $SIGNER (not notarized: no NOTARY_* secrets)"
fi
