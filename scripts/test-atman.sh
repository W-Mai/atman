#!/bin/bash
# Run atman in an isolated test environment (debug builds only)
# Usage: ./scripts/test-atman.sh [extra args...]

cd "$(dirname "$0")/.."

TMPDIR=$(mktemp -d /tmp/atman-test-XXXXXX)
echo "test env: $TMPDIR"

ATMAN_TEST_DATA_DIR="$TMPDIR/data" \
ATMAN_TEST_CONFIG_DIR="$TMPDIR/config" \
cargo run -- "$@"

echo -n "clean up $TMPDIR? [Y/n] "
read -r response
case "$response" in
    [nN]*) echo "kept at $TMPDIR" ;;
    *) rm -rf "$TMPDIR"; echo "cleaned up" ;;
esac
