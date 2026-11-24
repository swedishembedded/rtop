#!/usr/bin/env bash
# Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
#
# SPDX-License-Identifier: Apache-2.0
#
# Converts the HTML screens rendered by the `screenshots` example into
# lossless WebP images: headless Chromium takes the screenshot, ImageMagick
# (`magick`) or `cwebp` encodes it. Needs a Chromium/Chrome binary
# (CHROME=..., or found on PATH or in the Playwright browser cache) and the
# DejaVu Sans Mono font for exact glyph widths.
#
#   scripts/shots.sh target/shots [name-filter]
set -euo pipefail

dir=${1:-target/shots}
filter=${2:-}
scale=${SCALE:-1}

find_chrome() {
    if [[ -n "${CHROME:-}" ]]; then echo "$CHROME"; return; fi
    for c in chromium chromium-browser google-chrome google-chrome-stable chrome-headless-shell; do
        if command -v "$c" >/dev/null 2>&1; then command -v "$c"; return; fi
    done
    local cached
    cached=$(ls -d "$HOME"/.cache/ms-playwright/chromium_headless_shell-*/chrome-headless-shell-linux64/chrome-headless-shell 2>/dev/null | tail -1 || true)
    if [[ -n "$cached" ]]; then echo "$cached"; return; fi
    cached=$(ls -d /opt/pw-browsers/chromium*/chrome-linux*/chrome 2>/dev/null | tail -1 || true)
    if [[ -n "$cached" ]]; then echo "$cached"; return; fi
    echo "shots.sh: no Chromium found; set CHROME=/path/to/chrome" >&2
    exit 1
}

encode_webp() {
    if command -v magick >/dev/null 2>&1; then
        magick "$1" -define webp:lossless=true -define webp:method=6 "$2"
    elif command -v cwebp >/dev/null 2>&1; then
        cwebp -quiet -lossless -z 9 "$1" -o "$2"
    else
        echo "shots.sh: need ImageMagick (magick) or cwebp to write WebP" >&2
        exit 1
    fi
}

chrome=$(find_chrome)
count=0
for html in "$dir"/*.html; do
    name=$(basename "$html" .html)
    [[ -n "$filter" && "$name" != *"$filter"* ]] && continue
    # File names end in -<cols>x<rows>; cell = 8.43 x 17 px plus 12 px padding.
    size=${name##*-}
    cols=${size%x*}
    rows=${size#*x}
    w=$(( (cols * 843 + 99) / 100 + 24 ))
    h=$(( rows * 17 + 24 ))
    "$chrome" --headless --no-sandbox --disable-gpu --hide-scrollbars \
        --force-device-scale-factor="$scale" --window-size="$w,$h" \
        --screenshot="$dir/$name.png" "file://$(realpath "$html")" >/dev/null 2>&1
    encode_webp "$dir/$name.png" "$dir/$name.webp"
    rm -f "$dir/$name.png"
    count=$((count + 1))
done
echo "$count WebP screenshots in $dir"
