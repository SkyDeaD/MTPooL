#!/usr/bin/env bash
# Writes the released version into the workspace: the binaries report CARGO_PKG_VERSION, the
# updater compares it with the manifest, and the bill of materials names it. Cargo.lock changes
# with it, which keeps --locked working. For a release checkout only; never commit the result.
set -euo pipefail

version=${1:?usage: stamp-version.sh VERSION}

sed -i.orig -e "1,/^version = \"[^\"]*\"/ s/^version = \"[^\"]*\"/version = \"$version\"/" Cargo.toml
rm -f Cargo.toml.orig
awk -v v="$version" '
  /^name = "mtpool-/ { mine = 1; print; next }
  /^name = / { mine = 0 }
  mine && /^version = / { print "version = \"" v "\""; mine = 0; next }
  { print }
' Cargo.lock >Cargo.lock.new
mv Cargo.lock.new Cargo.lock

grep -m1 '^version = ' Cargo.toml
if grep -A1 '^name = "mtpool-' Cargo.lock | grep '^version = ' | grep -v "\"$version\""; then
    echo "::error::Cargo.lock still has a workspace crate at another version."
    exit 1
fi
