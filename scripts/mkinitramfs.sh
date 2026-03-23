#!/bin/bash
# Build a minimal cpio initramfs containing /init and optionally /runner
set -euo pipefail

INIT_BIN="${1:?Usage: mkinitramfs.sh <init-binary> <output.cpio> [runner-binary]}"
OUTPUT="${2:?Usage: mkinitramfs.sh <init-binary> <output.cpio> [runner-binary]}"
RUNNER_BIN="${3:-}"

TMPDIR=$(mktemp -d)
trap "rm -rf $TMPDIR" EXIT

# Create minimal directory structure
mkdir -p "$TMPDIR"/{dev,proc,sys,tmp}

# Copy init binary
cp "$INIT_BIN" "$TMPDIR/init"
chmod 755 "$TMPDIR/init"

# Copy optional runner binary
if [ -n "$RUNNER_BIN" ]; then
    cp "$RUNNER_BIN" "$TMPDIR/runner"
    chmod 755 "$TMPDIR/runner"
fi

# Create the cpio archive
cd "$TMPDIR"
find . | cpio -o -H newc --quiet > "$OUTPUT"

echo "initramfs: $(du -h "$OUTPUT" | cut -f1) ($OUTPUT)"
