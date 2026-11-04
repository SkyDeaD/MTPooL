#!/bin/sh
# MTPooL registrar installer for Ubuntu 24.04. Run it as root from a copy of this `deploy/`
# directory (the units and the Caddyfile are read from next to the script):
#
#   sh install-registrar.sh --domain pool.example.com
#
# Options:
#   --domain <name>       the public name of the pool; Caddy gets its certificate (required)
#   --binary <path>       install this registrar binary instead of downloading a release
#   --no-caddy            do not install or configure Caddy (the registrar then listens on
#                         127.0.0.1:8080 only; put your own proxy in front of it)
#   --release-url <url>   where releases are downloaded from
#                         (default https://github.com/SkyDeaD/MTPooL/releases/latest/download)
#   --release-repo <o/r>  the GitHub repository the registrar updates itself from, written to
#                         /etc/mtpool/registrar.env on a first install (default SkyDeaD/MTPooL)
#   --pubkey <key>        the release signing key (the base64 line of the .pub file); the
#                         released copy of this script has it written in
#
# It creates the `mtpool` user, /etc/mtpool, /var/lib/mtpool and /var/backups/mtpool, installs the
# systemd units, starts them, and prints the admin token once.
set -eu

PUBKEY='__MTPOOL_PUBKEY__'
RELEASE_URL=https://github.com/SkyDeaD/MTPooL/releases/latest/download

die() {
    echo "error: $*" >&2
    exit 1
}

usage() {
    echo "usage: install-registrar.sh --domain <name> [--binary <path>] [--no-caddy] [--release-url <url>] [--release-repo <owner/name>] [--pubkey <key>]" >&2
    exit 2
}

main() {
    domain=
    binary=
    caddy=yes
    release_repo=SkyDeaD/MTPooL
    here=$(cd "$(dirname "$0")" && pwd)

    while [ $# -gt 0 ]; do
        case "$1" in
        --domain)
            [ $# -ge 2 ] || die "--domain needs a name"
            domain=$2
            shift 2
            ;;
        --binary)
            [ $# -ge 2 ] || die "--binary needs a path"
            binary=$2
            shift 2
            ;;
        --no-caddy)
            caddy=no
            shift
            ;;
        --release-url)
            [ $# -ge 2 ] || die "--release-url needs a URL"
            RELEASE_URL=${2%/}
            shift 2
            ;;
        --release-repo)
            [ $# -ge 2 ] || die "--release-repo needs owner/name"
            release_repo=$2
            shift 2
            ;;
        --pubkey)
            [ $# -ge 2 ] || die "--pubkey needs a key"
            PUBKEY=$2
            shift 2
            ;;
        -h | --help) usage ;;
        *) die "unknown option: $1" ;;
        esac
    done

    [ -n "$domain" ] || die "--domain is required"
    case "$release_repo" in
    */*/* | */ | /* | *[!A-Za-z0-9._/-]* | '') die "--release-repo is not owner/name: $release_repo" ;;
    */*) ;;
    *) die "--release-repo is not owner/name: $release_repo" ;;
    esac
    case "$domain" in
    *[!A-Za-z0-9.-]* | .* | *. | *..* | -*) die "--domain is not a host name: $domain" ;;
    esac
    case "$domain" in
    *.*) ;;
    *) die "--domain needs a full name such as pool.example.com" ;;
    esac
    if [ -n "$binary" ]; then
        [ -f "$binary" ] || die "cannot read $binary"
    fi

    [ "$(id -u)" -eq 0 ] || die "run as root"
    os=unknown
    if [ -r /etc/os-release ]; then
        # shellcheck source=/dev/null
        os=$(. /etc/os-release && echo "${ID:-unknown} ${VERSION_ID:-unknown}")
    fi
    [ "$os" = "ubuntu 24.04" ] || die "only Ubuntu 24.04 is supported, this is $os"

    for file in systemd/mtpool-registrar.socket systemd/mtpool-registrar.service \
        systemd/mtpool-backup.service systemd/mtpool-backup.timer \
        systemd/mtpool-registrar-update.path systemd/mtpool-registrar-update.service \
        systemd/mtpool-registrar-rollback.service Caddyfile; do
        [ -r "$here/$file" ] || die "$here/$file is missing: run this script from the deploy directory"
    done

    case "$(uname -m)" in
    x86_64 | amd64) target=x86_64-unknown-linux-musl ;;
    aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
    *) die "no build for $(uname -m)" ;;
    esac

    work=$(mktemp -d)
    cleanup() {
        rm -rf "$work"
    }
    trap cleanup EXIT INT TERM

    # --- packages --------------------------------------------------------------------------------

    packages="ca-certificates curl"
    [ -n "$binary" ] || packages="$packages minisign jq"
    missing=
    for tool in $packages; do
        case "$tool" in
        ca-certificates) [ -r /etc/ssl/certs/ca-certificates.crt ] || missing="$missing $tool" ;;
        *) command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool" ;;
        esac
    done
    export DEBIAN_FRONTEND=noninteractive
    apt_install() {
        { apt-get update -qq && apt-get install -y -qq --no-install-recommends "$@"; } >"$work/apt.log" 2>&1 ||
            {
                cat "$work/apt.log" >&2
                die "cannot install $*"
            }
    }
    if [ -n "$missing" ]; then
        echo "installing:$missing"
        # shellcheck disable=SC2086
        apt_install $missing
    fi

    fetch() {
        curl -fsSL --retry 3 --connect-timeout 10 -o "$2" "$1"
    }

    # --- the binary ------------------------------------------------------------------------------

    if [ -z "$binary" ]; then
        case "$PUBKEY" in
        '' | __MTPOOL_PUBKEY__) die "no release signing key: pass --pubkey, or use --binary" ;;
        esac
        echo "fetching the release manifest from $RELEASE_URL"
        fetch "$RELEASE_URL/manifest.json" "$work/manifest.json" || die "cannot fetch the manifest"
        fetch "$RELEASE_URL/manifest.json.minisig" "$work/manifest.json.minisig" ||
            die "cannot fetch the manifest signature"
        if ! minisign -Vm "$work/manifest.json" -x "$work/manifest.json.minisig" -P "$PUBKEY" >/dev/null 2>"$work/minisign.log"; then
            cat "$work/minisign.log" >&2
            die "signature check failed: the manifest is not signed with the expected key; nothing was installed"
        fi
        picked=$(jq -r --arg target "$target" \
            '[.artifacts[] | select(.component == "mtpool-registrar" and .target == $target)] | first | "\(.name) \(.sha256)"' \
            "$work/manifest.json") || die "the manifest is not valid"
        artifact=${picked% *}
        want_sum=${picked#* }
        case "$artifact" in
        '' | null | *[!A-Za-z0-9._-]*) die "the manifest has no mtpool-registrar for $target" ;;
        esac
        case "$want_sum" in
        *[!0-9a-f]* | '') die "the manifest has no checksum for $artifact" ;;
        esac
        [ "${#want_sum}" -eq 64 ] || die "the manifest has no checksum for $artifact"
        echo "downloading $artifact"
        fetch "$RELEASE_URL/$artifact" "$work/$artifact" || die "cannot download $artifact"
        echo "$want_sum  $work/$artifact" | sha256sum -c - >/dev/null ||
            die "checksum of $artifact does not match the signed manifest"
        binary=$work/$artifact
    fi

    # --- user, directories, binary, units ----------------------------------------------------------

    if ! id mtpool >/dev/null 2>&1; then
        useradd --system --user-group --home-dir /var/lib/mtpool --no-create-home \
            --shell /usr/sbin/nologin mtpool
    fi
    install -d -m 0750 -o root -g mtpool /etc/mtpool
    # 0755 like the unit's StateDirectoryMode: the update helper (no capabilities) passes through
    # it to the releases. The database inside is a 0600 file.
    install -d -m 0755 -o mtpool -g mtpool /var/lib/mtpool
    # The registrar service (user mtpool) publishes verified releases here; the root helper only
    # reads them. The state of the updater is root's and the service cannot write it.
    install -d -m 0755 -o mtpool -g mtpool /var/lib/mtpool/releases
    install -d -m 0755 -o root -g root /var/lib/mtpool-update
    install -d -m 0750 -o mtpool -g mtpool /var/backups/mtpool
    if [ ! -e /etc/mtpool/registrar.env ]; then
        {
            echo "# Settings of the registrar service, one NAME=value per line. See the documentation"
            echo "# for MTPOOL_DB_PATH, MTPOOL_LISTEN, MTPOOL_LOG and MTPOOL_RELEASES_DIR."
            echo "# The registrar polls this GitHub repository for signed releases and updates itself;"
            echo "# delete the line to switch self-update off."
            echo "MTPOOL_RELEASE_REPO=$release_repo"
        } >/etc/mtpool/registrar.env
        chown root:mtpool /etc/mtpool/registrar.env
        chmod 0640 /etc/mtpool/registrar.env
    fi

    # A fresh install is the new truth: forget what an earlier self-update recorded, and drop the
    # previous build kept for it, so that neither points at a version that is no longer here.
    rm -f /var/lib/mtpool-update/installed /var/lib/mtpool-update/probation /usr/local/bin/mtpool-registrar.prev
    install -m 0755 "$binary" /usr/local/bin/mtpool-registrar
    for unit in mtpool-registrar.socket mtpool-registrar.service mtpool-backup.service mtpool-backup.timer \
        mtpool-registrar-update.path mtpool-registrar-update.service mtpool-registrar-rollback.service; do
        install -m 0644 "$here/systemd/$unit" "/etc/systemd/system/$unit"
    done

    # --- Caddy -----------------------------------------------------------------------------------

    if [ "$caddy" = yes ]; then
        if ! command -v caddy >/dev/null 2>&1; then
            echo "installing Caddy from its apt repository"
            apt_install gnupg apt-transport-https
            fetch https://dl.cloudsmith.io/public/caddy/stable/gpg.key "$work/caddy.gpg.key" ||
                die "cannot fetch the Caddy repository key"
            gpg --batch --yes --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg "$work/caddy.gpg.key" ||
                die "cannot read the Caddy repository key"
            fetch https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt /etc/apt/sources.list.d/caddy-stable.list ||
                die "cannot fetch the Caddy repository list"
            apt_install caddy
        fi
        install -d -m 0755 /etc/caddy
        sed -e "s/{\$MTPOOL_DOMAIN}/$domain/" "$here/Caddyfile" >/etc/caddy/Caddyfile
    fi

    # --- start -----------------------------------------------------------------------------------

    # A first install is one where the database does not exist yet. It is decided before any unit
    # starts, because starting the registrar creates the database.
    db=
    if [ -r /etc/mtpool/registrar.env ]; then
        db=$(sed -n 's/^MTPOOL_DB_PATH=//p' /etc/mtpool/registrar.env | tail -n 1 | tr -d "\"'")
    fi
    [ -n "$db" ] || db=/var/lib/mtpool/registrar.db
    if [ -e "$db" ]; then first=no; else first=yes; fi

    systemctl daemon-reload
    units="mtpool-registrar.socket mtpool-registrar.service mtpool-backup.timer mtpool-registrar-update.path"
    [ "$caddy" = no ] || units="$units caddy"
    # shellcheck disable=SC2086
    systemctl enable --now $units
    [ "$caddy" = no ] || systemctl reload-or-restart caddy

    reset_command="runuser -u mtpool -- mtpool-registrar admin reset-token"
    if [ "$first" = yes ]; then
        # The token is issued by the registrar's own command and shown here once.
        token=$(runuser -u mtpool -- sh -c \
            'set -a; [ ! -r /etc/mtpool/registrar.env ] || . /etc/mtpool/registrar.env; exec /usr/local/bin/mtpool-registrar admin reset-token') ||
            die "cannot issue the admin token; run: $reset_command"
    else
        # An upgrade: the running service gets the new binary, the admin keeps the token.
        systemctl try-restart mtpool-registrar.service
    fi
    if [ "$caddy" = yes ]; then
        panel="https://$domain/"
    else
        panel="http://127.0.0.1:8080/ (behind your own proxy: https://$domain/)"
    fi
    echo
    echo "MTPooL is installed."
    echo "Panel: $panel"
    if [ "$first" = yes ]; then
        echo "Admin token (shown once, keep it): $token"
    else
        echo "The database already exists: this is an upgrade, the admin token is unchanged."
        echo "To issue a new one: $reset_command"
    fi
}

main "$@"
