#!/bin/sh
# render-homebrew-formula.sh — render the Homebrew formula for a release (SPEC-M3.3 §1).
#
#   sh scripts/render-homebrew-formula.sh <tag> <SHA256SUMS> [<template>] > Formula/kioku.rb
#
# Fills packaging/homebrew/kioku.rb.tmpl with the tag, the version (tag without `v`) and the
# SHA-256 of the two macOS and two Linux (musl) tarballs, taken from the release's
# SHA256SUMS (`<hex>  <asset>` lines, as release.yml writes it). Fails — printing nothing on
# stdout — when the tag is malformed or a checksum is missing or not 64 hex digits.
# POSIX sh, awk and sed only. Golden test: scripts/test-scripts.sh.
set -eu

die() {
    printf 'render-homebrew-formula: %s\n' "$*" >&2
    exit 1
}

[ $# -ge 2 ] || die "usage: render-homebrew-formula.sh <tag> <SHA256SUMS> [<template>]"
tag=$1
sums=$2
root=$(cd "$(dirname "$0")/.." && pwd)
tmpl=${3:-$root/packaging/homebrew/kioku.rb.tmpl}

case "$tag" in
    v[0-9]*.[0-9]*.[0-9]*) ;;
    *) die "tag must look like v1.2.3, got: $tag" ;;
esac
case "$tag" in
    *[!A-Za-z0-9.+-]*) die "tag has unexpected characters: $tag" ;;
esac
[ -f "$sums" ] || die "no such file: $sums"
[ -f "$tmpl" ] || die "no such template: $tmpl"
version=${tag#v}

# sum_of <target>: the checksum of kioku-<tag>-<target>.tar.gz in $sums.
sum_of() {
    asset="kioku-$tag-$1.tar.gz"
    s=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1; exit }' "$sums")
    case "$s" in
        "") die "no checksum for $asset in $sums" ;;
    esac
    case "$s" in
        *[!0-9a-f]*) die "checksum of $asset is not lowercase hex: $s" ;;
    esac
    [ "${#s}" -eq 64 ] || die "checksum of $asset is not 64 hex digits: $s"
    printf '%s' "$s"
}

mac_arm=$(sum_of aarch64-apple-darwin)
mac_intel=$(sum_of x86_64-apple-darwin)
linux_arm=$(sum_of aarch64-unknown-linux-musl)
linux_intel=$(sum_of x86_64-unknown-linux-musl)

sed -e "s/@TAG@/$tag/g" \
    -e "s/@VERSION@/$version/g" \
    -e "s/@SHA256_AARCH64_APPLE_DARWIN@/$mac_arm/g" \
    -e "s/@SHA256_X86_64_APPLE_DARWIN@/$mac_intel/g" \
    -e "s/@SHA256_AARCH64_UNKNOWN_LINUX_MUSL@/$linux_arm/g" \
    -e "s/@SHA256_X86_64_UNKNOWN_LINUX_MUSL@/$linux_intel/g" \
    "$tmpl"
