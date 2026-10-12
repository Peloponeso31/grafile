#!/usr/bin/env bash
# Build the grafile C ABI artifacts (cdylib + staticlib).
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build -p grafile-ffi --release
echo
echo "artifacts:"
for pattern in target/release/libgrafile.so target/release/libgrafile.dylib \
               target/release/libgrafile.a target/release/grafile.dll \
               target/release/grafile.lib; do
    [ -e "$pattern" ] && ls -l "$pattern"
done
echo "header: ffi/include/grafile.h"
