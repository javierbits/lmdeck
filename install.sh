#!/bin/sh
# Install lmdeck from a GitHub release.
#
#   curl --proto '=https' --tlsv1.2 -fsSL https://github.com/javierbits/lmdeck/releases/latest/download/install.sh | sh
#
# Options:
#   --version X.Y.Z     release to install (default: latest)
#   --dir DIR           install directory (default: $HOME/.local/bin)
#   -h, --help          show this help
#
# Environment: LMDECK_VERSION, LMDECK_INSTALL_DIR, LMDECK_BASE_URL.
# LMDECK_BASE_URL is the release download root; it exists for mirrors and
# local testing, e.g. http://127.0.0.1:8000.
set -eu

REPO="javierbits/lmdeck"
BASE_URL="${LMDECK_BASE_URL:-https://github.com/$REPO/releases}"
VERSION="${LMDECK_VERSION:-}"
INSTALL_DIR="${LMDECK_INSTALL_DIR:-${HOME:-}/.local/bin}"

die() {
    printf 'lmdeck: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Install lmdeck from a GitHub release.

  curl --proto '=https' --tlsv1.2 -fsSL https://github.com/javierbits/lmdeck/releases/latest/download/install.sh | sh

Options:
  --version X.Y.Z     release to install (default: latest)
  --dir DIR           install directory (default: $HOME/.local/bin)
  -h, --help          show this help

Environment: LMDECK_VERSION, LMDECK_INSTALL_DIR, LMDECK_BASE_URL.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || die "--version needs a value"
            VERSION="$2"
            shift 2
            ;;
        --dir)
            [ "$#" -ge 2 ] || die "--dir needs a value"
            INSTALL_DIR="$2"
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1 (try --help)"
            ;;
    esac
done

[ -n "${HOME:-}" ] || die "HOME is not set"
[ -n "$INSTALL_DIR" ] || die "install directory is empty"

if [ -z "$VERSION" ]; then
    command -v curl >/dev/null 2>&1 || die "curl is required to find the latest release; pass --version"
    VERSION=$(curl -fsSL -o /dev/null -w '%{url_effective}' "$BASE_URL/latest") ||
        die "cannot resolve the latest release from $BASE_URL/latest"
    VERSION="${VERSION##*/}"
    [ -n "$VERSION" ] || die "cannot resolve the latest release tag"
fi

case "$VERSION" in
    v*) TAG="$VERSION" ;;
    *) TAG="v$VERSION" ;;
esac
NUM="${TAG#v}"

os=$(uname -s 2>/dev/null || echo unknown)
cpu=$(uname -m 2>/dev/null || echo unknown)
case "$os" in
    Linux) os=linux ;;
    Darwin)
        os=darwin
        # A terminal under Rosetta reports x86_64; prefer the native build.
        if [ "$cpu" = x86_64 ] && [ "$(sysctl -in sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
            cpu=arm64
        fi
        ;;
    *) die "unsupported operating system: $os (lmdeck runs on Linux and macOS)" ;;
esac
case "$cpu" in
    x86_64 | amd64) cpu=x86_64 ;;
    arm64 | aarch64) cpu=aarch64 ;;
    *) die "unsupported CPU: $cpu" ;;
esac
case "$os" in
    linux) TARGET="$cpu-unknown-linux-gnu" ;;
    darwin) TARGET="$cpu-apple-darwin" ;;
esac

if [ "$os" = linux ] && command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
    die "this system uses musl libc (Alpine?); no prebuilt binary yet, use: cargo install lmdeck"
fi

ASSET="lmdeck-$NUM-$TARGET.tar.gz"
URL="$BASE_URL/download/$TAG/$ASSET"
SUMS_URL="$BASE_URL/download/$TAG/checksums.txt"

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL --retry 3 "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -t 3 -O "$2" "$1"; }
else
    die "curl or wget is required"
fi

TMP=$(mktemp -d "${TMPDIR:-/tmp}/lmdeck.XXXXXX") || die "cannot create a temporary directory"
trap 'rm -rf "$TMP"' 0 1 2 15

printf 'downloading %s\n' "$URL"
fetch "$URL" "$TMP/$ASSET" || die "download failed: $URL"
fetch "$SUMS_URL" "$TMP/checksums.txt" || die "cannot download $SUMS_URL"

if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$TMP/$ASSET" | awk '{print $1}')
else
    actual=$(shasum -a 256 "$TMP/$ASSET" | awk '{print $1}')
fi
expected=$(awk -v name="$ASSET" '$2 == name { print $1; exit }' "$TMP/checksums.txt")
[ -n "$expected" ] || die "checksums.txt has no entry for $ASSET"
[ "$actual" = "$expected" ] || die "checksum mismatch for $ASSET"

mkdir -p "$TMP/x"
tar -xzf "$TMP/$ASSET" -C "$TMP/x" lmdeck || die "cannot extract $ASSET"
[ -f "$TMP/x/lmdeck" ] || die "$ASSET does not contain lmdeck"

mkdir -p "$INSTALL_DIR" || die "cannot create $INSTALL_DIR"
dest="$INSTALL_DIR/lmdeck"
new="$INSTALL_DIR/.lmdeck.$$"
cp "$TMP/x/lmdeck" "$new" || die "cannot write to $INSTALL_DIR"
chmod 0755 "$new"
mv -f "$new" "$dest" || {
    rm -f "$new"
    die "cannot install to $dest"
}

if ! "$dest" --version >/dev/null 2>"$TMP/err"; then
    cat "$TMP/err" >&2
    printf 'lmdeck: warning: %s does not run on this system (glibc too old?)\n' "$dest" >&2
fi

case ":${PATH:-}:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        printf '\nInstalled %s.\nAdd it to your PATH:\n  export PATH="%s:$PATH"\n' "$dest" "$INSTALL_DIR"
        ;;
esac
printf 'installed %s\n' "$dest"
