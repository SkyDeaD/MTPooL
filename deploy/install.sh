#!/bin/sh
# MTPooL node installer. The registrar serves it at /install.sh with its own address, its public
# signing key and the pool's settings written over the markers below.
#
#   curl -fsSL https://<pool>/install.sh | MTPOOL_TOKEN=<token> sh -s -- --name <name>
#
# Options:
#   --name <name>        the name of this node (required)
#   --token-file <path>  read the enroll token from a file instead of MTPOOL_TOKEN
#   --preset telemt|mtproxyl
#                        install a proxy engine too: telemt, or MTProxyL (which brings its own telemt)
#   --port <n>           the port the engine listens on, when it is not read from its config
#
# The token is never an argument: arguments show in `ps`.
set -eu

POOL_URL='__MTPOOL_URL__'
PUBKEY='__MTPOOL_PUBKEY__'
POOL_PORT='__MTPOOL_PROXY_PORT__'
POOL_TLS_DOMAIN='__MTPOOL_TLS_DOMAIN__'

TELEMT_VERSION=3.5.16
TELEMT_SHA256_X86_64=e6f46b2f3becf29bfd555cec8a059e429f7c082c1af402499b3a7940cf8ebd34
TELEMT_SHA256_AARCH64=1e1bc87f05923b9673cbbaf34a3e50f7fdcf7d496cd24efcdb34c278ed18a659

# MTProxyL (github.com/Liafanx/MTProxyL) is installed by its own installer, pinned to one commit
# and to the sha256 of that commit's install.sh. That installer downloads its libraries from the
# same commit, so they are pinned by the commit hash. To move to a newer MTProxyL, read its
# installer again, then change both lines.
MTPROXYL_COMMIT=6bedcea9801e06f8c246b882b6bab45e080ecdf3
MTPROXYL_INSTALLER_SHA256=26008d2baceb7d89e60904e1d8a26572a02857497d382cb3ac97eeed15ca1418

STATE_DIR=/var/lib/mtpool
CONFIG_DIR=/etc/mtpool

die() {
    echo "error: $*" >&2
    exit 1
}

usage() {
    echo "usage: install.sh --name <name> [--token-file <path>] [--preset telemt|mtproxyl] [--port <n>]" >&2
    echo "the enroll token comes from MTPOOL_TOKEN or --token-file, never an argument" >&2
    exit 2
}

# The whole script is read before anything runs: a program that reads standard input (apt-get,
# systemctl) cannot eat the rest of a script that is piped in.
main() {
    name=
    token_file=
    preset=
    engine_port=

    while [ $# -gt 0 ]; do
        case "$1" in
        --name)
            [ $# -ge 2 ] || die "--name needs a value"
            name=$2
            shift 2
            ;;
        --token-file)
            [ $# -ge 2 ] || die "--token-file needs a path"
            token_file=$2
            shift 2
            ;;
        --preset)
            [ $# -ge 2 ] || die "--preset needs telemt or mtproxyl"
            preset=$2
            shift 2
            ;;
        --port)
            [ $# -ge 2 ] || die "--port needs a number"
            engine_port=$2
            shift 2
            ;;
        -h | --help) usage ;;
        *) die "unknown option: $1" ;;
        esac
    done

    [ -n "$name" ] || die "--name is required"
    case "$name" in
    *[!A-Za-z0-9._-]*) die "--name may hold letters, digits, dot, dash and underscore only" ;;
    esac
    [ "${#name}" -le 63 ] || die "--name is longer than 63 characters"
    case "$preset" in
    '' | telemt | mtproxyl) ;;
    *) die "unknown preset: $preset" ;;
    esac
    if [ -n "$engine_port" ]; then
        case "$engine_port" in
        *[!0-9]* | '') die "--port must be a number" ;;
        esac
        [ "$engine_port" -ge 1 ] && [ "$engine_port" -le 65535 ] || die "--port must be 1-65535"
    fi

    [ "$(id -u)" -eq 0 ] || die "run as root"

    # Ubuntu and Debian only.
    os_id=unknown
    if [ -r /etc/os-release ]; then
        # shellcheck source=/dev/null
        os_id=$(. /etc/os-release && echo "${ID:-unknown}")
    fi
    case "$os_id" in
    ubuntu | debian) ;;
    *) die "only Ubuntu and Debian are supported, this is $os_id" ;;
    esac

    # The token: from the file or the environment. It is taken into a shell variable and removed
    # from the environment at once, so no program started below inherits it.
    if [ -n "$token_file" ]; then
        [ -r "$token_file" ] || die "cannot read $token_file"
        token=
        IFS= read -r token <"$token_file" || [ -n "$token" ] || die "$token_file is empty"
    else
        token=${MTPOOL_TOKEN:-}
    fi
    unset MTPOOL_TOKEN
    [ -n "$token" ] || die "no enroll token: set MTPOOL_TOKEN or pass --token-file"

    case "$(uname -m)" in
    x86_64 | amd64)
        arch=x86_64
        ;;
    aarch64 | arm64)
        arch=aarch64
        ;;
    *) die "no build for $(uname -m)" ;;
    esac
    target="$arch-unknown-linux-musl"

    work=$(mktemp -d)
    cleanup() {
        rm -rf "$work"
    }
    trap cleanup EXIT INT TERM

    # --- tools -----------------------------------------------------------------------------------

    missing=
    for tool in curl minisign jq; do
        command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
    done
    [ -r /etc/ssl/certs/ca-certificates.crt ] || missing="$missing ca-certificates"
    if [ -n "$missing" ]; then
        echo "installing:$missing"
        export DEBIAN_FRONTEND=noninteractive
        # The package names are the tool names, so the list goes in as it is.
        # shellcheck disable=SC2086
        { apt-get update -qq && apt-get install -y -qq --no-install-recommends $missing; } >"$work/apt.log" 2>&1 ||
            {
                cat "$work/apt.log" >&2
                die "cannot install$missing"
            }
    fi

    # --- the release, checked ----------------------------------------------------------------------

    fetch() {
        curl -fsSL --retry 3 --connect-timeout 10 -o "$2" "$1"
    }

    echo "fetching the release manifest from $POOL_URL"
    fetch "$POOL_URL/releases/manifest.json" "$work/manifest.json" || die "cannot fetch the manifest"
    fetch "$POOL_URL/releases/manifest.json.minisig" "$work/manifest.json.minisig" ||
        die "cannot fetch the manifest signature"
    if ! minisign -Vm "$work/manifest.json" -x "$work/manifest.json.minisig" -P "$PUBKEY" >/dev/null 2>"$work/minisign.log"; then
        cat "$work/minisign.log" >&2
        die "signature check failed: the manifest is not signed with the pool's key; nothing was installed"
    fi

    picked=$(jq -r --arg target "$target" \
        '[.artifacts[] | select(.component == "mtpool-agent" and .target == $target)] | first | "\(.name) \(.sha256)"' \
        "$work/manifest.json") || die "the manifest is not valid"
    artifact=${picked% *}
    want_sum=${picked#* }
    case "$artifact" in
    '' | null | *[!A-Za-z0-9._-]*) die "the manifest has no mtpool-agent for $target" ;;
    esac
    case "$want_sum" in
    *[!0-9a-f]* | '') die "the manifest has no checksum for $artifact" ;;
    esac
    [ "${#want_sum}" -eq 64 ] || die "the manifest has no checksum for $artifact"

    echo "downloading $artifact"
    fetch "$POOL_URL/releases/$artifact" "$work/$artifact" || die "cannot download $artifact"
    echo "$want_sum  $work/$artifact" | sha256sum -c - >/dev/null || die "checksum of $artifact does not match the signed manifest"

    # --- the engine --------------------------------------------------------------------------------

    # The port in the "[server]" table of a telemt configuration; telemt's own default is 443.
    config_port() {
        awk '
            /^[ \t]*\[/ { in_server = ($0 ~ /^[ \t]*\[server\][ \t]*(#.*)?$/) }
            in_server && /^[ \t]*port[ \t]*=/ {
                value = $0
                sub(/^[^=]*=[ \t]*/, "", value)
                sub(/[ \t#].*$/, "", value)
                print value
                found = 1
                exit
            }
            END { if (!found) print 443 }
        ' "$1"
    }

    engine_config() {
        if [ -d /opt/mtpr-simple ] || [ ! -d /opt/mtproxyl ]; then
            [ -r /etc/telemt/telemt.toml ] && echo /etc/telemt/telemt.toml
            return 0
        fi
        for candidate in /opt/mtproxyl/mtproxy/config.toml /opt/mtproxyl/mtproxy/telemt.toml /opt/mtproxyl/superexpert.toml; do
            if [ -r "$candidate" ]; then
                echo "$candidate"
                return 0
            fi
        done
        return 0
    }

    install_telemt() {
        [ -n "$POOL_TLS_DOMAIN" ] || die "the pool has no tls_domain set; set it in the panel first"
        case "$arch" in
        x86_64) sum=$TELEMT_SHA256_X86_64 ;;
        aarch64) sum=$TELEMT_SHA256_AARCH64 ;;
        esac
        file="telemt-$arch-linux-gnu.tar.gz"
        echo "installing telemt $TELEMT_VERSION"
        fetch "https://github.com/telemt/telemt/releases/download/$TELEMT_VERSION/$file" "$work/$file" ||
            die "cannot download telemt"
        echo "$sum  $work/$file" | sha256sum -c - >/dev/null || die "checksum of $file does not match"
        mkdir "$work/telemt"
        tar -xzf "$work/$file" -C "$work/telemt" telemt
        install -m 0755 "$work/telemt/telemt" /usr/local/bin/telemt

        secret=$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
        mkdir -p /etc/telemt
        cat >"$work/telemt.toml" <<'TELEMT_TOML'
__MTPOOL_TELEMT_TOML__
TELEMT_TOML
        sed -e "s/__PORT__/$POOL_PORT/" -e "s/__TLS_DOMAIN__/$POOL_TLS_DOMAIN/" \
            -e "s/__SECRET__/$secret/" "$work/telemt.toml" >/etc/telemt/telemt.toml
        chmod 0600 /etc/telemt/telemt.toml
        cat >/etc/systemd/system/telemt.service <<'TELEMT_UNIT'
__MTPOOL_TELEMT_UNIT__
TELEMT_UNIT
    }

    # MTProxyL's official installer, run without questions: one manager, the pool's port and
    # FakeTLS domain, and the same pinned telemt build the telemt preset installs. Its extras
    # (Zapret2, system tuning) are off: the pool does not need them.
    install_mtproxyl() {
        [ -n "$POOL_TLS_DOMAIN" ] || die "the pool has no tls_domain set; set it in the panel first"
        if [ -e /opt/mtproxyl ]; then
            die "/opt/mtproxyl exists: MTProxyL is already installed; run the installer without --preset"
        fi
        command -v bash >/dev/null 2>&1 || die "MTProxyL's installer needs bash"
        case "$arch" in
        x86_64) sum=$TELEMT_SHA256_X86_64 ;;
        aarch64) sum=$TELEMT_SHA256_AARCH64 ;;
        esac
        file="telemt-$arch-linux-gnu.tar.gz"
        echo "installing MTProxyL (commit $MTPROXYL_COMMIT)"
        fetch "https://raw.githubusercontent.com/Liafanx/MTProxyL/$MTPROXYL_COMMIT/install.sh" \
            "$work/mtproxyl-install.sh" || die "cannot download the MTProxyL installer"
        echo "$MTPROXYL_INSTALLER_SHA256  $work/mtproxyl-install.sh" | sha256sum -c - >/dev/null ||
            die "checksum of the MTProxyL installer does not match"
        bash "$work/mtproxyl-install.sh" --branch "$MTPROXYL_COMMIT" -- \
            --mode manager --proxy-mode mtproto --port "$POOL_PORT" --sni "$POOL_TLS_DOMAIN" \
            --engine binary \
            --engine-url "https://github.com/telemt/telemt/releases/download/$TELEMT_VERSION/$file" \
            --engine-sha256 "$sum" \
            --zapret2 no --meko no </dev/null || die "the MTProxyL installer failed"
    }

    case "$preset" in
    telemt)
        install_telemt
        systemctl daemon-reload
        systemctl enable --now telemt
        ;;
    mtproxyl) install_mtproxyl ;;
    esac

    if [ -n "$engine_port" ]; then
        found_port=$engine_port
    else
        config=$(engine_config)
        [ -n "$config" ] || die "no proxy engine found; install one or use --preset telemt"
        found_port=$(config_port "$config")
    fi
    if [ "$found_port" != "$POOL_PORT" ]; then
        die "engine listens on $found_port, the pool expects $POOL_PORT; change the engine's port, or the pool's in the panel"
    fi

    # --- the agent ---------------------------------------------------------------------------------

    install -m 0755 "$work/$artifact" /usr/local/bin/mtpool-agent
    install -d -m 0755 "$CONFIG_DIR"
    install -d -m 0700 "$STATE_DIR"
    umask 077
    printf '%s' "$token" >"$STATE_DIR/enroll_token"
    umask 022
    chmod 0600 "$STATE_DIR/enroll_token"
    unset token

    {
        printf 'registrar = "%s"\n' "$POOL_URL"
        printf 'name = "%s"\n' "$name"
        echo 'antiscan = true'
        [ -z "$engine_port" ] || printf 'port = %s\n' "$engine_port"
    } >"$CONFIG_DIR/agent.toml"

    cat >/etc/systemd/system/mtpool-agent.service <<'MTPOOL_AGENT_UNIT'
__MTPOOL_AGENT_UNIT__
MTPOOL_AGENT_UNIT
    cat >/etc/systemd/system/mtpool-agent-rollback.service <<'MTPOOL_AGENT_ROLLBACK_UNIT'
__MTPOOL_AGENT_ROLLBACK_UNIT__
MTPOOL_AGENT_ROLLBACK_UNIT

    systemctl daemon-reload
    systemctl enable --now mtpool-agent
    echo "done: $name is installed and enrolls with $POOL_URL"
}

main "$@"
