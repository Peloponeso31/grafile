#!/usr/bin/env bash
# Write tests/fixtures/versions/<version>.grafile, the cross-version fixture
# for one release, by running tests/fixtures/versions/fixture.cypher with
# that release's CLI. Run it once per release and check the file in.
#
# Usage: scripts/version-fixture.sh [path/to/grafile]
# Without an argument, the release build of this checkout is used.
set -euo pipefail
cd "$(dirname "$0")/.."

BIN="${1:-}"
if [ -z "$BIN" ]; then
    cargo build --release --bin grafile
    BIN=target/release/grafile
fi
VERSION="$("$BIN" --version | awk '{print $2}')"
OUT="tests/fixtures/versions/$VERSION.grafile"
rm -f "$OUT" "$OUT-journal"
"$BIN" "$OUT" tests/fixtures/versions/fixture.cypher
echo "wrote $OUT"
