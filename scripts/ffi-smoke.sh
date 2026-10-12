#!/usr/bin/env bash
# Compile and run the C and Python smoke tests against the built library.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build -p grafile-ffi

case "$(uname -s)" in
    Darwin) LIB="target/debug/libgrafile.dylib"; LIBVAR="DYLD_LIBRARY_PATH" ;;
    *)      LIB="target/debug/libgrafile.so";    LIBVAR="LD_LIBRARY_PATH" ;;
esac

SMOKE_DIR="$(mktemp -d)"
trap 'rm -rf "$SMOKE_DIR"' EXIT

echo "== C smoke test"
cc -Wall -Wextra -I ffi/include ffi/examples/smoke.c -L target/debug -lgrafile \
    -o target/ffi-smoke
env "$LIBVAR=target/debug" ./target/ffi-smoke "$SMOKE_DIR/smoke.grafile"

echo
echo "== Python ctypes smoke test"
if command -v python3 >/dev/null 2>&1; then
    env "$LIBVAR=target/debug" GRAFILE_LIB="$LIB" python3 ffi/examples/smoke.py
else
    echo "python3 not found; skipped"
fi
