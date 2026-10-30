#!/usr/bin/env bash
# Regenerate `frontend/static/favicon.ico` from the brand mark.
#
# WHY THIS EXISTS. The committed favicon.ico was not an ICO at all — it was
# 235 bytes of SVG, and malformed SVG at that (every attribute value wrapped
# in backslashes instead of quotes, `width=\16\`), showing a generic blue
# folder rather than the OxiCloud cloud. Browsers hid the problem because
# `app.html` offers `logo/logo-plain.svg` first and they silently ignore the
# broken second link. Clients that fetch `/favicon.ico` directly got garbage:
# Bitwarden showed no icon, and `ocs_handler.rs` advertises that same URL to
# NextCloud clients as the instance favicon.
#
# Source is `logo/logo-plain.svg` — the mark on its light circle. NOT
# `maskable-*.png`: those are Android adaptive icons drawn inside a safe
# zone, so the cloud ends up small and off-centre at favicon sizes.
#
# 16/32/48 in one file: 16 is the tab, 32 is what `app.html` advertises and
# what most icon scrapers take, 48 covers Windows shortcuts and the
# higher-DPI tab. All three are packed into the single .ico, which is the
# format's whole point.
#
#   ./scripts/helpers/make-favicon.sh
#
# Requires ImageMagick (`brew install imagemagick`). Re-run after any change
# to the logo; the output is committed, so CI never needs this.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SRC="$REPO_ROOT/frontend/static/logo/logo-plain.svg"
OUT="$REPO_ROOT/frontend/static/favicon.ico"

if ! command -v magick > /dev/null 2>&1; then
  echo "error: ImageMagick (magick) not found — brew install imagemagick" >&2
  exit 1
fi
if [[ ! -f "$SRC" ]]; then
  echo "error: missing source $SRC" >&2
  exit 1
fi

# `-background none` keeps the corners transparent: the mark is a circle, and
# a white box behind it looks wrong on a dark browser tab strip.
magick -background none "$SRC" -define icon:auto-resize=48,32,16 "$OUT"

# Fail loudly rather than commit another file whose name lies about its
# format — that is the exact defect this script was written to undo.
if ! file -b "$OUT" | grep -q "MS Windows icon resource"; then
  echo "error: $OUT is not an ICO — ImageMagick wrote $(file -b "$OUT")" >&2
  exit 1
fi

echo "wrote $OUT"
file -b "$OUT"
