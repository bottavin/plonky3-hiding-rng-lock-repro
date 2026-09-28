#!/usr/bin/env bash
# Creates vendor/plonky3-fixed: Plonky3 at the pinned commit, with the fix applied.
set -euo pipefail

REV=11053cbe49dbb3f78ad57f1d80aa837592ce9276
ROOT="$(cd "$(dirname "$0")" && pwd)"
DEST="$ROOT/vendor/plonky3-fixed"
PATCH="$ROOT/patches/0001-hiding-rng-lock.patch"

if [ -e "$DEST" ]; then
    echo "error: $DEST already exists. Delete it to create it again." >&2
    exit 1
fi

mkdir -p "$ROOT/vendor"
git clone https://github.com/Plonky3/Plonky3.git "$DEST"
git -C "$DEST" checkout --detach "$REV"
git -C "$DEST" apply --check "$PATCH"
git -C "$DEST" apply "$PATCH"

echo
echo "Fixed copy ready at $DEST"
git -C "$DEST" log -1 --format='base commit: %H %cd'
git -C "$DEST" diff --stat
