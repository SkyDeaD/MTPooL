#!/usr/bin/env bash
# Rebuilds web/src/fonts/*.woff2 from the Geist fonts of the npm package `geist` (SIL OFL 1.1).
#
# Needs fonttools with brotli, outside the repository:
#   python3 -m venv /tmp/fonts-venv && /tmp/fonts-venv/bin/pip install fonttools brotli
#   mkdir /tmp/geist && (cd /tmp/geist && npm pack geist && tar xzf geist-*.tgz)
#   web/scripts/subset-fonts.sh /tmp/fonts-venv/bin /tmp/geist/package
#
# Keeps Latin, Latin-1, Cyrillic, digits and the common punctuation; the weight axis is cut to
# 400..700, which is all the interface uses.
set -euo pipefail

VENV_BIN=${1:?path to the bin directory of a venv with fonttools and brotli}
PKG=${2:?path to the unpacked npm package geist}
OUT="$(cd "$(dirname "$0")/.." && pwd)/src/fonts"

UNICODES="U+0020-007E,U+00A0-00FF,U+0400-045F,U+0490-0491,U+2010-2015,U+2018-201E,U+2022,U+2026,U+2030,U+2116,U+20AC,U+2190-2193,U+2212,U+2248,U+2264-2265,U+25CF"
FEATURES="kern,liga,calt,ccmp,locl,case,tnum,mark,mkmk"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

subset() {
  local src=$1 name=$2
  "$VENV_BIN/fonttools" varLib.instancer "$src" "wght=400:700" -o "$work/$name.ttf" -q
  "$VENV_BIN/pyftsubset" "$work/$name.ttf" \
    --unicodes="$UNICODES" \
    --layout-features="$FEATURES" \
    --flavor=woff2 \
    --no-hinting \
    --output-file="$OUT/$name.woff2"
}

subset "$PKG/dist/fonts/geist-sans/Geist-Variable.ttf" Geist
subset "$PKG/dist/fonts/geist-mono/GeistMono-Variable.ttf" GeistMono
cp "$PKG/LICENSE.txt" "$OUT/OFL.txt"
ls -l "$OUT"
