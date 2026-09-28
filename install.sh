#!/bin/sh
# kioku installer (docs/SPEC-M2.md §13.2).
#
#   curl -fsSL https://raw.githubusercontent.com/misorafa/kioku/main/install.sh | sh
#   curl -fsSL …/install.sh | sh -s -- --version v0.2.0 --no-setup
#   curl -fsSL …/install.sh | sh -s -- --client-only http://home.lan:7391 <token>
#
# Downloads the release binary for this machine, verifies its SHA-256 against the
# release's SHA256SUMS, installs it to ~/.local/bin/kioku and runs `kioku setup`.
# Never uses sudo and never edits shell rc files. POSIX sh; the body lives in `main`,
# called on the last line, so a truncated download runs nothing.
#
# Options (a flag wins over its environment variable):
#   --version <tag>      KIOKU_VERSION      release tag (default: latest)
#   --install-dir <dir>  KIOKU_INSTALL_DIR  destination (default: $HOME/.local/bin)
#   --repo <owner/name>  KIOKU_REPO         GitHub repository (default: misorafa/kioku)
#   --from-source                           build with cargo instead of downloading
#   --no-setup                              install only, do not run `kioku setup`
#   anything else (and everything after `--`) is passed to `kioku setup`,
#   e.g. --client-only <url> <token>, --no-service, --agents codex,cursor, --dry-run.
# Test-only overrides: KIOKU_DOWNLOAD_BASE (replaces https://github.com/<repo>/releases),
# KIOKU_UNAME_S, KIOKU_UNAME_M.

set -eu

DEFAULT_REPO="misorafa/kioku"
MIN_CARGO_MINOR=91

say() {
    printf 'kioku-install: %s\n' "$*"
}

warn() {
    printf 'kioku-install: warning: %s\n' "$*" >&2
}

die() {
    printf 'kioku-install: error: %s\n' "$*" >&2
    exit 1
}

have() {
    command -v "$1" >/dev/null 2>&1
}

# Single-quotes $1 for a later `eval set --`.
quote() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

add_pass() {
    PASS="$PASS $(quote "$1")"
}

usage() {
    cat <<'EOF_USAGE'
usage: install.sh [--version <tag>] [--install-dir <dir>] [--repo <owner/name>]
                  [--from-source] [--no-setup] [kioku setup options...] [-- kioku setup options...]

  curl -fsSL https://raw.githubusercontent.com/misorafa/kioku/main/install.sh | sh
  curl -fsSL .../install.sh | sh -s -- --client-only http://home.lan:7391 <token>

Every option this installer does not know is passed to `kioku setup`.
EOF_USAGE
}

# fetch <url> <out>: download; non-zero on any HTTP error.
fetch() {
    if [ "$DL" = curl ]; then
        curl -fsSL --retry 2 --connect-timeout 20 -o "$2" "$1" 2>/dev/null
    else
        wget -q -O "$2" "$1" 2>/dev/null
    fi
}

# Final URL after redirects; non-zero on a network or HTTP error (nothing printed then).
final_url() {
    if [ "$DL" = curl ]; then
        FU_URL=$(curl -fsSLI -o /dev/null -w '%{url_effective}' --retry 2 --connect-timeout 20 "$1" 2>/dev/null) ||
            return 1
        printf '%s' "$FU_URL"
    else
        wget -S --spider -q "$1" >"$TMP/wget-headers" 2>&1 || return 1
        FU_LOC=$(sed -n 's/^ *[Ll]ocation: *//p' "$TMP/wget-headers" | tail -n 1 | tr -d '\r')
        if [ -n "$FU_LOC" ]; then printf '%s' "$FU_LOC"; else printf '%s' "$1"; fi
    fi
}

sha256_of() {
    if have sha256sum; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# Sets TAG from VERSION (resolving `latest`); empty TAG = no release published (the
# redirect does not land on /releases/tag/<tag>). Returns 1 when the lookup itself failed
# (network or HTTP error) — that is never mistaken for "no release".
resolve_tag() {
    TAG=""
    if [ "$VERSION" != latest ]; then
        TAG=$VERSION
        return 0
    fi
    RT_URL=$(final_url "$BASE/latest") || return 1
    RT_URL=${RT_URL%%\?*}
    RT_URL=${RT_URL%/}
    case "$RT_URL" in
        */releases/tag/*) TAG=${RT_URL##*/releases/tag/} ;;
        *) TAG="" ;;
    esac
}

# Sets TARGETS (space-separated candidates, empty = no prebuilt binary for this machine).
detect_targets() {
    OS=${KIOKU_UNAME_S:-$(uname -s)}
    ARCH=${KIOKU_UNAME_M:-$(uname -m)}
    case "$ARCH" in
        x86_64 | amd64) ARCH=x86_64 ;;
        arm64 | aarch64) ARCH=aarch64 ;;
        *) ARCH="" ;;
    esac
    TARGETS=""
    case "$OS" in
        Linux)
            [ -n "$ARCH" ] && TARGETS="$ARCH-unknown-linux-musl $ARCH-unknown-linux-gnu"
            ;;
        Darwin)
            # An x86_64 shell under Rosetta on Apple silicon: prefer the native binary.
            if [ "$ARCH" = x86_64 ] && [ -z "${KIOKU_UNAME_M:-}" ] &&
                [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
                ARCH=aarch64
            fi
            [ -n "$ARCH" ] && TARGETS="$ARCH-apple-darwin"
            ;;
        MINGW* | MSYS* | CYGWIN* | Windows_NT)
            die "on Windows, use install.ps1 in PowerShell: & ([scriptblock]::Create((irm https://raw.githubusercontent.com/$DEFAULT_REPO/main/install.ps1))) -ClientOnly <url> <token> (inside WSL, this installer works as on Linux)"
            ;;
    esac
    if [ -z "$TARGETS" ]; then
        say "no prebuilt binary for $OS $(uname -m 2>/dev/null || true)${KIOKU_UNAME_M:+ ($KIOKU_UNAME_M)}"
    fi
    return 0
}

# Sets EXPECTED to the checksum of asset $1 of $TAG; dies when none can be found.
expected_sum() {
    EXPECTED=""
    if [ -z "$SUMS_TRIED" ]; then
        SUMS_TRIED=1
        fetch "$BASE/download/$TAG/SHA256SUMS" "$TMP/SHA256SUMS" || rm -f "$TMP/SHA256SUMS"
    fi
    if [ -f "$TMP/SHA256SUMS" ]; then
        EXPECTED=$(awk -v f="$1" '$2 == f || $2 == "*" f { print $1; exit }' "$TMP/SHA256SUMS")
    fi
    if [ -z "$EXPECTED" ] && fetch "$BASE/download/$TAG/$1.sha256" "$TMP/$1.sha256"; then
        EXPECTED=$(awk -v f="$1" '$2 == f || $2 == "*" f { print $1; exit }' "$TMP/$1.sha256")
    fi
    [ -n "$EXPECTED" ] || die "no checksum for $1 in the $TAG release (SHA256SUMS / $1.sha256); refusing to install an unverified binary"
}

# install_binary <path>: copy next to the destination, check that it runs, rename over it.
# Returns 1 (and removes the copy) when the binary does not run on this machine.
install_binary() {
    # Called from `if try_target …`, where `set -e` is off: every step is checked.
    mkdir -p "$DIR" || die "cannot create $DIR"
    IB_NEW="$DIR/.kioku.new.$$"
    cp "$1" "$IB_NEW" || die "cannot write to $DIR"
    chmod 755 "$IB_NEW" || { rm -f "$IB_NEW"; die "cannot make $IB_NEW executable"; }
    if ! IB_VER=$("$IB_NEW" --version 2>&1); then
        rm -f "$IB_NEW"
        warn "the binary does not run here: $(printf '%s' "$IB_VER" | head -n 1)"
        return 1
    fi
    # Same directory = same filesystem: the rename is atomic, and a running
    # `kioku serve` keeps the old inode until it restarts.
    # (A directory in the way would swallow the file: `mv` moves into it.)
    if [ -d "$DIR/kioku" ]; then
        rm -f "$IB_NEW"
        die "$DIR/kioku is a directory; move it away and re-run"
    fi
    mv -f "$IB_NEW" "$DIR/kioku" || { rm -f "$IB_NEW"; die "cannot replace $DIR/kioku"; }
    say "installed $IB_VER to $DIR/kioku"
    return 0
}

# try_target <target>: 0 = installed, 1 = not available / does not run. Dies on a bad checksum.
try_target() {
    TT_ASSET="kioku-$TAG-$1.tar.gz"
    say "downloading $TT_ASSET"
    if ! fetch "$BASE/download/$TAG/$TT_ASSET" "$TMP/$TT_ASSET"; then
        say "no $TT_ASSET in release $TAG"
        return 1
    fi
    expected_sum "$TT_ASSET"
    TT_ACTUAL=$(sha256_of "$TMP/$TT_ASSET") || die "cannot compute the SHA-256 of $TT_ASSET"
    [ -n "$TT_ACTUAL" ] || die "cannot compute the SHA-256 of $TT_ASSET"
    if [ "$TT_ACTUAL" != "$EXPECTED" ]; then
        rm -f "$TMP/$TT_ASSET"
        die "checksum mismatch for $TT_ASSET (expected $EXPECTED, got $TT_ACTUAL); nothing was installed"
    fi
    say "checksum ok ($TT_ACTUAL)"
    rm -rf "$TMP/x" || die "cannot clean $TMP/x"
    mkdir "$TMP/x" || die "cannot create $TMP/x"
    tar -xzf "$TMP/$TT_ASSET" -C "$TMP/x" || die "cannot extract $TT_ASSET"
    TT_BIN="$TMP/x/kioku-$TAG-$1/kioku"
    [ -f "$TT_BIN" ] || TT_BIN="$TMP/x/kioku"
    [ -f "$TT_BIN" ] || die "$TT_ASSET does not contain a kioku binary"
    install_binary "$TT_BIN"
}

from_source() {
    FS_CMD="cargo install --locked --git https://github.com/$REPO${TAG:+ --tag $TAG} kioku-cli"
    FS_OK=1
    if ! have cargo; then
        FS_OK=0
    else
        FS_MINOR=$(cargo --version 2>/dev/null | awk '{ split($2, v, "."); if (v[1] > 1) print 999; else print v[2] + 0 }')
        if [ "${FS_MINOR:-0}" -lt "$MIN_CARGO_MINOR" ]; then
            warn "cargo 1.$MIN_CARGO_MINOR or newer is required (found: $(cargo --version 2>/dev/null || echo none))"
            FS_OK=0
        fi
    fi
    if ! have git; then
        warn "git is required to build kioku from source"
        FS_OK=0
    fi
    if [ "$FS_OK" = 0 ]; then
        say "to build kioku from source, install git and Rust (https://rustup.rs):"
        say "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
        say "then run:"
        say "  $FS_CMD"
        say "(or, once Rust is installed, re-run this installer with --from-source)"
        exit 1
    fi
    say "building kioku from source; the first build takes several minutes (lindera dictionary download)"
    if [ -f Cargo.toml ] && grep -q 'kioku-cli' Cargo.toml 2>/dev/null && [ -d crates/kioku-cli ]; then
        say "running: cargo build --release --locked -p kioku-cli (in $(pwd))"
        cargo build --release --locked -p kioku-cli || die "cargo build failed"
        FS_BIN="$(pwd)/target/release/kioku"
    else
        say "running: $FS_CMD --root $TMP/cargo"
        if [ -n "$TAG" ]; then
            cargo install --locked --git "https://github.com/$REPO" --tag "$TAG" kioku-cli --root "$TMP/cargo" ||
                die "cargo install failed"
        else
            cargo install --locked --git "https://github.com/$REPO" kioku-cli --root "$TMP/cargo" ||
                die "cargo install failed"
        fi
        FS_BIN="$TMP/cargo/bin/kioku"
    fi
    [ -f "$FS_BIN" ] || die "the build produced no binary at $FS_BIN"
    install_binary "$FS_BIN" || die "the freshly built binary does not run"
}

path_hint() {
    case ":${PATH:-}:" in
        *":$DIR:"* | *":$DIR/:"*) return 0 ;;
    esac
    PH_DIR=$DIR
    case "$DIR" in
        "$HOME"/*) PH_DIR="\$HOME/${DIR#"$HOME"/}" ;;
    esac
    say "$DIR is not on your PATH; add it (this installer never edits shell files):"
    case "$(basename "${SHELL:-sh}")" in
        zsh) PH_RC=.zshrc ;;
        bash) if [ "${OS:-}" = Darwin ]; then PH_RC=.bash_profile; else PH_RC=.bashrc; fi ;;
        fish) PH_RC="" ;;
        *) PH_RC=.profile ;;
    esac
    if [ -n "$PH_RC" ]; then
        say "  echo 'export PATH=\"$PH_DIR:\$PATH\"' >> ~/$PH_RC"
    else
        say "  fish_add_path $PH_DIR"
    fi
    say "(kioku setup and the agent hooks use the absolute path, so they work either way)"
}

main() {
    VERSION=${KIOKU_VERSION:-latest}
    DIR=${KIOKU_INSTALL_DIR:-}
    REPO=${KIOKU_REPO:-$DEFAULT_REPO}
    FROM_SOURCE=0
    SETUP=1
    PASS=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --version | --install-dir | --repo)
                [ $# -ge 2 ] || die "$1 needs a value"
                case "$1" in
                    --version) VERSION=$2 ;;
                    --install-dir) DIR=$2 ;;
                    --repo) REPO=$2 ;;
                esac
                shift 2
                ;;
            --version=*) VERSION=${1#*=}; shift ;;
            --install-dir=*) DIR=${1#*=}; shift ;;
            --repo=*) REPO=${1#*=}; shift ;;
            --from-source) FROM_SOURCE=1; shift ;;
            --no-setup) SETUP=0; shift ;;
            -h | --help) usage; exit 0 ;;
            --)
                shift
                for a in "$@"; do add_pass "$a"; done
                break
                ;;
            --client-only)
                # URL and token go to `kioku setup` verbatim, whatever they look like.
                add_pass "$1"
                shift
                for _ in 1 2; do
                    if [ $# -ge 1 ]; then
                        add_pass "$1"
                        shift
                    fi
                done
                ;;
            *) add_pass "$1"; shift ;;
        esac
    done

    [ -n "${HOME:-}" ] || die "HOME is not set"
    if [ "$(id -u)" = 0 ] && [ -z "$DIR" ]; then
        die "refusing to run as root: kioku is a per-user install (config, service and hooks live in the user's home). Run as your normal user, or pass --install-dir explicitly."
    fi
    DIR=${DIR:-$HOME/.local/bin}
    case "$DIR" in
        /*) ;;
        *) DIR="$(pwd)/$DIR" ;;
    esac
    [ "$VERSION" = latest ] || case "$VERSION" in v*) ;; *) VERSION="v$VERSION" ;; esac
    BASE=${KIOKU_DOWNLOAD_BASE:-https://github.com/$REPO/releases}
    BASE=${BASE%/}

    TMP=$(mktemp -d 2>/dev/null || mktemp -d -t kioku)
    trap 'rm -rf "$TMP"' EXIT
    trap 'exit 130' INT TERM
    SUMS_TRIED=""
    TAG=""
    OS=""

    DL=""
    if have curl; then DL=curl; elif have wget; then DL=wget; fi
    INSTALLED=0
    if [ "$FROM_SOURCE" = 0 ]; then
        [ -n "$DL" ] || die "curl or wget is required"
        have tar || die "tar is required"
        have sha256sum || have shasum || die "sha256sum or shasum is required to verify the download"
        detect_targets
        resolve_tag ||
            die "could not look up the latest release at $BASE/latest (network or HTTP error); check the connection and retry, or pass --version <tag>"
        if [ -z "$TAG" ]; then
            say "no published release found for $REPO"
        elif [ -n "$TARGETS" ]; then
            say "installing kioku $TAG"
            for t in $TARGETS; do
                if try_target "$t"; then
                    INSTALLED=1
                    break
                fi
            done
        fi
        if [ "$INSTALLED" = 0 ]; then
            say "no usable prebuilt binary; falling back to --from-source"
        fi
    elif [ -n "$DL" ]; then
        # Building from source: an unreachable release index only means "no --tag".
        resolve_tag || warn "could not look up the latest release (network or HTTP error); building the default branch"
    elif [ "$VERSION" != latest ]; then
        TAG=$VERSION
    fi
    [ "$INSTALLED" = 1 ] || from_source

    path_hint

    if [ "$SETUP" = 0 ]; then
        say "done (--no-setup). Next: $DIR/kioku setup"
        return 0
    fi
    rm -rf "$TMP"
    trap - EXIT
    eval "set -- $PASS"
    # The arguments may hold the token: never echo them.
    say "running: $DIR/kioku setup"
    exec "$DIR/kioku" setup "$@" </dev/null
}

main "$@"
