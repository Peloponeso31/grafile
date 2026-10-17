#!/bin/sh
# Installer for grafile (https://github.com/Peloponeso31/grafile).
#
# Usage:
#   curl -LsSf https://github.com/Peloponeso31/grafile/releases/latest/download/install.sh | sh
set -eu

repo="Peloponeso31/grafile"
bin_name="grafile"
version="${GRAFILE_VERSION:-}"
install_dir="${GRAFILE_INSTALL_DIR:-$HOME/.local/bin}"

usage() {
    cat <<'EOF'
Install the grafile CLI from GitHub releases.

Usage: install.sh [OPTIONS]

Options:
  --version VERSION   Install a specific version (e.g. 0.1.0 or v0.1.0)
  --dir DIR           Install the binary into DIR (default: ~/.local/bin)
  -h, --help          Show this help

Environment:
  GRAFILE_VERSION      Same as --version
  GRAFILE_INSTALL_DIR  Same as --dir
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
        --dir)
            [ $# -ge 2 ] || err "--dir requires an argument"
            install_dir="$2"
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
        ;;
    Linux)
        case "$(uname -m)" in
            x86_64 | amd64) asset="x86_64-linux-musl" ;;
            aarch64 | arm64) asset="aarch64-linux-musl" ;;
            *) err "unsupported Linux architecture: $(uname -m)" ;;
        esac
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
[ -f "$tmp/$bin_name" ] || err "archive did not contain the $bin_name binary"

mkdir -p "$install_dir"
install -m 755 "$tmp/$bin_name" "$install_dir/$bin_name"

case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *)
        printf 'warning: %s is not on your PATH. Add it with:\n' "$install_dir" >&2
        printf '  export PATH="%s"\n' "$install_dir:\$PATH" >&2
        ;;
esac

"$install_dir/$bin_name" --version
printf 'installed to %s\n' "$install_dir/$bin_name"
