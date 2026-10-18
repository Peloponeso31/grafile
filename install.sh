#!/bin/sh
# Installer for grafile (https://github.com/Peloponeso31/grafile).
#
# Installs the `grafile` CLI under PREFIX/bin and, when the release tarball
# carries them, the FFI library under PREFIX/lib, the C header under
# PREFIX/include and pkg-config metadata under PREFIX/lib/pkgconfig.
#
# Usage:
#   curl -LsSf https://github.com/Peloponeso31/grafile/releases/latest/download/install.sh | sh
set -eu

repo="Peloponeso31/grafile"
bin_name="grafile"
version="${GRAFILE_VERSION:-}"
prefix="${GRAFILE_PREFIX:-$HOME/.local}"

usage() {
    cat <<'EOF'
Install the grafile CLI and FFI library from GitHub releases.

Usage: install.sh [OPTIONS]

Options:
  --version VERSION   Install a specific version (e.g. 0.1.0 or v0.1.0)
  --prefix DIR        Install under DIR (default: ~/.local); the binary
                      lands in DIR/bin, the FFI library in DIR/lib, the C
                      header in DIR/include and pkg-config metadata in
                      DIR/lib/pkgconfig
  -h, --help          Show this help

Environment:
  GRAFILE_VERSION  Same as --version
  GRAFILE_PREFIX   Same as --prefix
EOF
}

err() {
    printf 'error: %s\n' "$1" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || err "--version requires an argument"
            version="$2"
            shift 2
            ;;
        --prefix)
            [ $# -ge 2 ] || err "--prefix requires an argument"
            prefix="$2"
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            err "unknown option: $1 (see --help)"
            ;;
    esac
done

command -v curl >/dev/null 2>&1 || err "curl is required"

case "$(uname -s)" in
    Darwin)
        asset="macos-universal"
        lib_name="libgrafile.dylib"
        ;;
    Linux)
        case "$(uname -m)" in
            x86_64 | amd64) asset="x86_64-linux-musl" ;;
            aarch64 | arm64) asset="aarch64-linux-musl" ;;
            *) err "unsupported Linux architecture: $(uname -m)" ;;
        esac
        lib_name="libgrafile.so"
        ;;
    *)
        err "unsupported OS: $(uname -s) (Windows users: winget install Peloponeso31.Grafile)"
        ;;
esac

if [ -n "$version" ]; then
    tag="v${version#v}"
else
    tag=$(curl -LsSf -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest") ||
        err "could not resolve the latest release"
    tag="${tag##*/}"
fi
case "$tag" in
    v*.*.*) ;;
    *) err "could not resolve a release tag (got '$tag')" ;;
esac

base="https://github.com/$repo/releases/download/$tag"
tarball="$bin_name-${tag#v}-$asset.tar.gz"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

curl -LsSf "$base/$tarball" -o "$tmp/$tarball" || err "could not download $tarball"
curl -LsSf "$base/SHA256SUMS.txt" -o "$tmp/SHA256SUMS.txt" || err "could not download SHA256SUMS.txt"

expected=$(awk -v name="$tarball" '$2 == name { print $1 }' "$tmp/SHA256SUMS.txt")
[ -n "$expected" ] || err "no checksum found for $tarball"

if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/$tarball" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$tmp/$tarball" | awk '{ print $1 }')
elif command -v openssl >/dev/null 2>&1; then
    actual=$(openssl dgst -sha256 "$tmp/$tarball" | awk '{ print $NF }')
else
    err "no SHA-256 tool found (need sha256sum, shasum, or openssl)"
fi

[ "$actual" = "$expected" ] || err "checksum mismatch for $tarball"

tar -xzf "$tmp/$tarball" -C "$tmp"
[ -f "$tmp/bin/$bin_name" ] || err "archive did not contain bin/$bin_name"

mkdir -p "$prefix/bin"
install -m 755 "$tmp/bin/$bin_name" "$prefix/bin/$bin_name"

have_library=false
if [ -f "$tmp/lib/$lib_name" ]; then
    have_library=true
    mkdir -p "$prefix/lib"
    cp -R "$tmp/lib/." "$prefix/lib/"
fi

have_header=false
if [ -f "$tmp/include/grafile.h" ]; then
    have_header=true
    mkdir -p "$prefix/include"
    cp -R "$tmp/include/." "$prefix/include/"
fi

case ":${PATH:-}:" in
    *":$prefix/bin:"*) ;;
    *)
        printf 'warning: %s is not on your PATH. Add it with:\n' "$prefix/bin" >&2
        printf '  export PATH="%s"\n' "$prefix/bin:\$PATH" >&2
        ;;
esac

if [ "$have_library" = true ]; then
    case "$prefix" in
        /usr | /usr/local) ;;
        *)
            printf 'hint: for pkg-config, add:\n' >&2
            printf '  export PKG_CONFIG_PATH="%s"\n' "$prefix/lib/pkgconfig:\$PKG_CONFIG_PATH" >&2
            ;;
    esac
fi

"$prefix/bin/$bin_name" --version
printf 'installed %s %s under %s\n' "$bin_name" "${tag#v}" "$prefix"
printf '  binary:     %s\n' "$prefix/bin/$bin_name"
if [ "$have_library" = true ]; then
    printf '  library:    %s\n' "$prefix/lib/$lib_name"
fi
if [ "$have_header" = true ]; then
    printf '  header:     %s\n' "$prefix/include/grafile.h"
fi
if [ "$have_library" = true ] && [ -f "$prefix/lib/pkgconfig/grafile.pc" ]; then
    printf '  pkg-config: %s\n' "$prefix/lib/pkgconfig/grafile.pc"
fi
