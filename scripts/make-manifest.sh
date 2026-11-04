#!/usr/bin/env bash
# Writes manifest.json for a release.
#
#   make-manifest.sh VERSION DIR [--announce-key FILE]
#
# DIR holds the builds, named mtpool-registrar-<target> and mtpool-agent-<target> for each target
# below. The manifest is {version, artifacts: [{component, target, name, sha256}]}, the shape that
# crates/selfupdate/src/manifest.rs reads.
#
# A normal release announces no key. A key rotation passes --announce-key FILE, the new public key
# as a file inside DIR; the manifest then lists it as the "mtpool-signing-key" component. The
# release that announces a new key is signed with the OLD key and built with the NEW one.
set -euo pipefail

TARGETS=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl)
COMPONENTS=(mtpool-registrar mtpool-agent)
# Keep equal to KEY_COMPONENT in crates/selfupdate/src/manifest.rs.
KEY_COMPONENT=mtpool-signing-key

die() {
    echo "make-manifest: $*" >&2
    exit 1
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

[ "$#" -ge 2 ] || die "usage: make-manifest.sh VERSION DIR [--announce-key FILE]"
version=$1
dir=$2
shift 2

key_file=
if [ "$#" -gt 0 ]; then
    [ "$#" -eq 2 ] && [ "$1" = "--announce-key" ] || die "usage: make-manifest.sh VERSION DIR [--announce-key FILE]"
    key_file=$2
fi

# Semver as crates/selfupdate reads it: no leading v, no build metadata, no leading zeros.
num='(0|[1-9][0-9]*)'
ident='(0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)'
semver="^${num}\\.${num}\\.${num}(-${ident}(\\.${ident})*)?\$"
[[ $version =~ $semver ]] || die "version is not semver (x.y.z or x.y.z-pre): '$version'"

[ -d "$dir" ] || die "not a directory: $dir"
plain='^[A-Za-z0-9._-]{1,120}$'

entries=()
add() { # component target name
    local file=$dir/$3
    [ -f "$file" ] && [ ! -L "$file" ] || die "missing file: $3"
    entries+=("$(jq -cn --arg c "$1" --arg t "$2" --arg n "$3" --arg s "$(sha256 "$file")" \
        '{component: $c, target: $t, name: $n, sha256: $s}')")
}

for component in "${COMPONENTS[@]}"; do
    for target in "${TARGETS[@]}"; do
        add "$component" "$target" "$component-$target"
    done
done

if [ -n "$key_file" ]; then
    [[ $key_file =~ $plain ]] && [[ $key_file != .* ]] || die "the key file name is not a plain name: $key_file"
    add "$KEY_COMPONENT" any "$key_file"
fi

# Write to a temporary file and move it, so a failure leaves no manifest behind.
tmp=$(mktemp "$dir/.manifest.XXXXXX")
trap 'rm -f "$tmp"' EXIT
printf '%s\n' "${entries[@]}" | jq -cs --arg v "$version" '{version: $v, artifacts: .}' >"$tmp"
mv "$tmp" "$dir/manifest.json"
trap - EXIT
echo "wrote $dir/manifest.json ($version, ${#entries[@]} artifacts)"
