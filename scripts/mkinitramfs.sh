#!/bin/bash
# Build a minimal cpio initramfs containing just /init
set -euo pipefail

INIT_BIN="${1:?Usage: mkinitramfs.sh <init-binary> <output.cpio>}"
OUTPUT="${2:?Usage: mkinitramfs.sh <init-binary> <output.cpio>}"

TMPDIR=$(mktemp -d)
trap "rm -rf $TMPDIR" EXIT

# Create minimal directory structure
mkdir -p "$TMPDIR"/{dev,proc,sys,tmp}

# Copy init binary
cp "$INIT_BIN" "$TMPDIR/init"
chmod 755 "$TMPDIR/init"

# Create the cpio archive
cd "$TMPDIR"
find . | cpio -o -H newc --quiet > "$OUTPUT"

echo "initramfs: $(du -h "$OUTPUT" | cut -f1) ($OUTPUT)"
