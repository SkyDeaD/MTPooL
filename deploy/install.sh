#!/bin/sh
# MTPooL node installer. The registrar serves it at /install.sh with its own address, its public
# signing key and the pool's settings written over the markers below.
# It shows its steps on the terminal and keeps the full output in /var/log/mtpool/install.log.
#
#   curl -fsSL https://<pool>/install.sh | sudo env MTPOOL_TOKEN=<token> sh -s -- --name <name>
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

LOG_DIR=/var/log/mtpool
LOG_FILE=$LOG_DIR/install.log

# How long the installer waits for the pool, in seconds.
ENROLL_WAIT=60
HEALTH_WAIT=180

# --- output ------------------------------------------------------------------------------------
#
# What people read goes to descriptor 3 (errors to 4). Once the log is open, the standard output
# and errors of every command go there, so no program can write over the steps.

ESC=$(printf '\033')
log_open=0
tty=0
wide=0
color=0
spin_frames='⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏'
ok_mark='✓'
bad_mark='✗'
wait_mark='…'
cols=80
spinner_pid=
step_title=
step_started=0
reported=0
waiting_for_pool=0
work=
last_detail=

setup_output() {
    if [ -t 3 ] && [ "${TERM:-dumb}" != dumb ]; then
        tty=1
        cols=$(stty size </dev/tty 2>/dev/null) || cols=
        cols=${cols#* }
        case "$cols" in
        '' | 0 | *[!0-9]*) cols=80 ;;
        esac
        [ "$cols" -lt 76 ] || wide=1
        [ -n "${NO_COLOR:-}" ] || color=1
        # The text console of Linux has no fonts for these.
        if [ "$TERM" = linux ]; then
            # shellcheck disable=SC1003
            spin_frames='| / - \'
            ok_mark='+'
            bad_mark='x'
            wait_mark='~'
        fi
    fi
}

# paint <SGR code> <text>
paint() {
    if [ "$color" = 1 ]; then
        printf '%s[%sm%s%s[0m' "$ESC" "$1" "$2" "$ESC"
    else
        printf '%s' "$2"
    fi
}

log_line() {
    if [ "$log_open" = 1 ]; then
        echo "$*"
    fi
}

show_logo() {
    row=0
    while IFS= read -r line; do
        row=$((row + 1))
        # Light blue to blue, one shade a line, where the terminal has 256 colors.
        case "${TERM:-}" in
        *256*) shade="38;5;$((57 - row * 6))" ;;
        *) shade=36 ;;
        esac
        printf '  ' >&3
        paint "$shade" "$line" >&3
        printf '\n' >&3
    done <<'MTPOOL_LOGO'
███╗   ███╗████████╗██████╗  ██████╗  ██████╗ ██╗     
████╗ ████║╚══██╔══╝██╔══██╗██╔═══██╗██╔═══██╗██║     
██╔████╔██║   ██║   ██████╔╝██║   ██║██║   ██║██║     
██║╚██╔╝██║   ██║   ██╔═══╝ ██║   ██║██║   ██║██║     
██║ ╚═╝ ██║   ██║   ██║     ╚██████╔╝╚██████╔╝███████╗
╚═╝     ╚═╝   ╚═╝   ╚═╝      ╚═════╝  ╚═════╝ ╚══════╝
MTPOOL_LOGO
}

# label <text> <value>: one line of a summary. Cyrillic takes two bytes a letter and dash counts
# bytes, so a column is a move of the cursor and not padding.
label() {
    if [ "$tty" = 1 ]; then
        printf '  %s%s[20G%s\n' "$1" "$ESC" "$2" >&3
    else
        printf '  %s: %s\n' "$1" "$2" >&3
    fi
}

show_header() {
    printf '\n' >&3
    if [ "$tty" = 1 ] && [ "$cols" -ge 56 ]; then
        show_logo
        printf '\n' >&3
        printf '  %s\n' "$(paint 1 'Установка ноды')" >&3
    else
        printf '  %s\n' "$(paint 1 'MTPooL: установка ноды')" >&3
    fi
    label 'Пул' "$POOL_URL"
    node_line="$name, порт $POOL_PORT"
    case "$preset" in
    telemt) node_line="$node_line, движок telemt" ;;
    mtproxyl) node_line="$node_line, движок MTProxyL" ;;
    esac
    label 'Нода' "$node_line"
    printf '\n' >&3
}

# draw_step <colored mark> <detail> <seconds>
draw_step() {
    if [ "$wide" = 1 ]; then
        printf '\r  %s %s%s[24G%s%s[70G%s%s[K' "$1" "$step_title" "$ESC" "$2" "$ESC" \
            "$(paint 2 "$3 с")" "$ESC" >&3
    elif [ "$tty" = 1 ]; then
        printf '\r  %s %s  %s  %s%s[K' "$1" "$step_title" "$2" "$(paint 2 "$3 с")" "$ESC" >&3
    elif [ -n "$2" ]; then
        printf '  %s %s: %s (%s с)' "$1" "$step_title" "$2" "$3" >&3
    else
        printf '  %s %s (%s с)' "$1" "$step_title" "$3" >&3
    fi
}

spinner_start() {
    [ "$tty" = 1 ] || return 0
    printf '%s[?25l' "$ESC" >&3
    (
        while kill -0 "$$" 2>/dev/null; do
            for frame in $spin_frames; do
                if [ "$wide" = 1 ]; then
                    detail=$(cat "$work/detail" 2>/dev/null) || detail=
                else
                    detail=
                fi
                draw_step "$(paint 36 "$frame")" "$detail" "$(($(date +%s) - step_started))"
                sleep 0.1
            done
        done
    ) &
    spinner_pid=$!
}

spinner_stop() {
    if [ -n "$spinner_pid" ]; then
        kill "$spinner_pid" 2>/dev/null || true
        wait "$spinner_pid" 2>/dev/null || true
        spinner_pid=
        printf '\r%s[K%s[?25h' "$ESC" "$ESC" >&3
    fi
}

step_begin() {
    step_title=$1
    step_started=$(date +%s)
    last_detail=
    : >"$work/detail"
    log_line "== $step_title"
    spinner_start
}

# What the running step is doing now. Without a terminal a change gets a line of its own.
step_detail() {
    # Whole or not at all: the spinner reads it at the same time.
    printf '%s' "$1" >"$work/detail.new"
    mv "$work/detail.new" "$work/detail"
    if [ "$tty" = 0 ] && [ "$1" != "$last_detail" ]; then
        printf '  %s %s: %s\n' "$wait_mark" "$step_title" "$1" >&3
    fi
    last_detail=$1
}

step_end() {
    spinner_stop
    draw_step "$1" "$2" "$(($(date +%s) - step_started))"
    printf '\n' >&3
    log_line "   $2"
    step_title=
}

step_done() {
    step_end "$(paint 32 "$ok_mark")" "$1"
}

# fail_step <text>: the line of the failed step. A short text stays on it, a long one goes below.
fail_step() {
    if [ -n "$step_title" ]; then
        if [ "${#1}" -le 80 ]; then
            step_end "$(paint 31 "$bad_mark")" "$1"
        else
            step_end "$(paint 31 "$bad_mark")" ""
            printf '  %s\n' "$1" >&4
        fi
    else
        spinner_stop
        printf '  %s %s\n' "$(paint 31 'Ошибка:')" "$1" >&4
        log_line "error: $1"
    fi
}

show_log_path() {
    if [ "$log_open" = 1 ]; then
        printf '\n' >&4
        printf '  Полный лог: %s\n' "$LOG_FILE" >&4
    fi
}

die() {
    reported=1
    fail_step "$*"
    show_log_path
    exit 1
}

# die_with_log <text>: for a program that failed, with the end of what it said.
die_with_log() {
    reported=1
    fail_step "$*"
    if [ "$log_open" = 1 ]; then
        printf '\n' >&4
        tail -n 12 "$LOG_FILE" | sed 's/^/    /' >&4
    fi
    show_log_path
    exit 1
}

usage() {
    reported=1
    echo "Запуск: install.sh --name <имя> [--token-file <путь>] [--preset telemt|mtproxyl] [--port <n>]" >&4
    echo "Токен регистрации берётся из MTPOOL_TOKEN или --token-file, в аргументах его быть не должно" >&4
    exit 2
}

show_how_to_follow() {
    label 'Логи агента' 'journalctl -u mtpool-agent -f'
    label 'Служба' 'systemctl status mtpool-agent'
    label 'Настройки' "$CONFIG_DIR/agent.toml"
    label 'Лог установки' "$LOG_FILE"
}

show_node_links() {
    label 'Страница ноды' "$POOL_URL/node/?id=$node_id"
    show_how_to_follow
}

on_exit() {
    code=$?
    spinner_stop
    if [ -n "$work" ]; then
        rm -rf "$work"
    fi
    # `set -e` stopped on a command nobody checked.
    if [ "$code" -ne 0 ] && [ "$reported" = 0 ]; then
        printf '  %s установка прервалась\n' "$(paint 31 'Ошибка:')" >&4
        show_log_path
    fi
}

on_signal() {
    spinner_stop
    reported=1
    if [ "$waiting_for_pool" = 1 ]; then
        # The node is installed; only the waiting stops.
        printf '\n  Нода установлена, ожидание прервано. Пул продолжит проверку сам.\n\n' >&3
        show_node_links
        exit 0
    fi
    printf '\n  Установка прервана.\n' >&4
    exit 130
}

# The id of this node, if it has one that is safe to put in an address.
read_node_id() {
    id=$(cat "$STATE_DIR/node_id" 2>/dev/null) || id=
    case "$id" in
    *[!A-Za-z0-9._-]*) id= ;;
    esac
    echo "$id"
}

pool_knows() {
    curl -fsS --max-time 5 -o /dev/null "$POOL_URL/api/v1/nodes/$1" 2>/dev/null
}

# What the pool has checked so far, from the answer about the node.
checks_text() {
    printf '%s' "$1" | jq -r --arg ok "$ok_mark" '
        select(.checks) | .checks |
        "порт " + (if .tcp then $ok else "ждём" end)
        + "  метрики " + (if .metrics then $ok else "ждём" end)
        + "  Globalping " + (if .globalping == "ok" then $ok
            elif .globalping == "failed" then "не прошёл"
            else "ждём" end)
    ' || true
}

enroll_timed_out() {
    reported=1
    fail_step "пул не принял ноду за $ENROLL_WAIT с"
    printf '\n  Последние строки журнала агента:\n' >&4
    journalctl -u mtpool-agent -n 8 --no-pager -o cat 2>/dev/null | sed 's/^/    /' >&4 || true
    printf '\n  Что проверить: токен регистрации действует, адрес пула открывается с этой машины\n' >&4
    if systemctl is-active --quiet mtpool-agent; then
        printf '  Агент продолжит пытаться сам, следить: journalctl -u mtpool-agent -f\n' >&4
    else
        printf '  Агент остановлен: запустите установщик ещё раз с новым токеном\n' >&4
    fi
    show_log_path
    exit 1
}

# The whole script is read before anything runs: a program that reads standard input (apt-get,
# systemctl) cannot eat the rest of a script that is piped in.
main() {
    exec 3>&1 4>&2
    trap on_exit EXIT
    trap on_signal INT TERM

    name=
    token_file=
    preset=
    engine_port=

    while [ $# -gt 0 ]; do
        case "$1" in
        --name)
            [ $# -ge 2 ] || die "у --name нет значения"
            name=$2
            shift 2
            ;;
        --token-file)
            [ $# -ge 2 ] || die "у --token-file нет пути"
            token_file=$2
            shift 2
            ;;
        --preset)
            [ $# -ge 2 ] || die "--preset ждёт telemt или mtproxyl"
            preset=$2
            shift 2
            ;;
        --port)
            [ $# -ge 2 ] || die "у --port нет числа"
            engine_port=$2
            shift 2
            ;;
        -h | --help) usage ;;
        *) die "неизвестный параметр: $1" ;;
        esac
    done

    [ -n "$name" ] || die "нужно указать --name"
    case "$name" in
    *[!A-Za-z0-9._-]*) die "в --name можно только латинские буквы, цифры, точку, дефис и подчёркивание" ;;
    [!A-Za-z0-9]* | *[!A-Za-z0-9]) die "--name должен начинаться и заканчиваться буквой или цифрой" ;;
    esac
    # The pool takes no longer name; the agent would be refused over and over.
    [ "${#name}" -le 32 ] || die "--name длиннее 32 символов"
    case "$preset" in
    '' | telemt | mtproxyl) ;;
    *) die "неизвестный preset: $preset" ;;
    esac
    if [ -n "$engine_port" ]; then
        case "$engine_port" in
        *[!0-9]* | '') die "--port должен быть числом" ;;
        esac
        if ! [ "$engine_port" -ge 1 ] || ! [ "$engine_port" -le 65535 ]; then
            die "--port должен быть от 1 до 65535"
        fi
    fi

    [ "$(id -u)" -eq 0 ] || die "запустите от root"

    # The token: from the file or the environment. It is taken into a shell variable and removed
    # from the environment at once, so no program started below inherits it.
    if [ -n "$token_file" ]; then
        [ -r "$token_file" ] || die "не читается файл $token_file"
        token=
        IFS= read -r token <"$token_file" || [ -n "$token" ] || die "файл $token_file пуст"
    else
        token=${MTPOOL_TOKEN:-}
    fi
    unset MTPOOL_TOKEN
    [ -n "$token" ] || die "нет токена регистрации: задайте MTPOOL_TOKEN или --token-file"

    setup_output
    work=$(mktemp -d)

    # The log is for root alone. It holds what the programs said, and they never get the token.
    install -d -m 0700 "$LOG_DIR"
    touch "$LOG_FILE"
    chmod 0600 "$LOG_FILE"
    exec >>"$LOG_FILE" 2>&1
    log_open=1
    log_line "--- $(date -u '+%Y-%m-%d %H:%M:%S') UTC: install of $name from $POOL_URL"

    show_header

    # --- the system ------------------------------------------------------------------------------

    step_begin 'Система'
    # Ubuntu and Debian only.
    os_id=unknown
    os_version=
    if [ -r /etc/os-release ]; then
        # shellcheck source=/dev/null
        os_id=$(. /etc/os-release && echo "${ID:-unknown}")
        # shellcheck source=/dev/null
        os_version=$(. /etc/os-release && echo "${VERSION_ID:-}")
    fi
    case "$os_id" in
    ubuntu) os_name=Ubuntu ;;
    debian) os_name=Debian ;;
    *) die "поддерживаются только Ubuntu и Debian, а это $os_id" ;;
    esac
    case "$(uname -m)" in
    x86_64 | amd64)
        arch=x86_64
        ;;
    aarch64 | arm64)
        arch=aarch64
        ;;
    *) die "нет сборки для $(uname -m)" ;;
    esac
    target="$arch-unknown-linux-musl"
    step_done "$os_name $os_version, $arch"

    # --- tools -----------------------------------------------------------------------------------

    step_begin 'Инструменты'
    missing=
    for tool in curl minisign jq; do
        command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
    done
    [ -r /etc/ssl/certs/ca-certificates.crt ] || missing="$missing ca-certificates"
    if [ -n "$missing" ]; then
        step_detail "ставлю$missing"
        export DEBIAN_FRONTEND=noninteractive
        # The package names are the tool names, so the list goes in as it is.
        # shellcheck disable=SC2086
        { apt-get update -qq && apt-get install -y -qq --no-install-recommends $missing; } </dev/null ||
            die_with_log "не удалось поставить$missing"
        step_done "поставлены$missing"
    else
        step_done 'уже есть'
    fi

    # --- the release, checked ----------------------------------------------------------------------

    # curl's errors go to the log with everything else.
    fetch() {
        curl -fsSL --retry 3 --connect-timeout 10 -o "$2" "$1"
    }

    step_begin 'Релиз'
    step_detail 'проверяю подпись'
    # The registrar holds one release, and answers 409 when its agent is newer than the registrar:
    # after a rollback, or for the moment before an update restarts it.
    manifest_code=0
    manifest_status=$(curl -fsSL --retry 3 --connect-timeout 10 -w '%{http_code}' \
        -o "$work/manifest.json" "$POOL_URL/releases/manifest.json") || manifest_code=$?
    case "$manifest_code:$manifest_status" in
    0:200) ;;
    *:409) die "релиз на пуле новее его регистратора, а другой сборки агента там нет, ничего не установлено. Повторите установку, когда регистратор обновится" ;;
    *) die_with_log "не удалось скачать манифест релиза" ;;
    esac
    fetch "$POOL_URL/releases/manifest.json.minisig" "$work/manifest.json.minisig" ||
        die_with_log "не удалось скачать подпись манифеста"
    if ! minisign -Vm "$work/manifest.json" -x "$work/manifest.json.minisig" -P "$PUBKEY"; then
        die_with_log "подпись манифеста не совпала с ключом пула, ничего не установлено"
    fi

    picked=$(jq -r --arg target "$target" \
        '[.artifacts[] | select(.component == "mtpool-agent" and .target == $target)] | first | "\(.name) \(.sha256)"' \
        "$work/manifest.json") || die "манифест не читается"
    release=$(jq -r '.version // empty' "$work/manifest.json") || release=
    artifact=${picked% *}
    want_sum=${picked#* }
    case "$artifact" in
    '' | null | *[!A-Za-z0-9._-]*) die "в манифесте нет mtpool-agent для $target" ;;
    esac
    case "$want_sum" in
    *[!0-9a-f]* | '') die "в манифесте нет контрольной суммы $artifact" ;;
    esac
    [ "${#want_sum}" -eq 64 ] || die "в манифесте нет контрольной суммы $artifact"
    case "$release" in
    *[!A-Za-z0-9.+-]*) release= ;;
    esac

    step_detail "скачиваю $artifact"
    fetch "$POOL_URL/releases/$artifact" "$work/$artifact" || die_with_log "не удалось скачать $artifact"
    echo "$want_sum  $work/$artifact" | sha256sum -c - || die "контрольная сумма $artifact не совпала с подписанным манифестом"
    step_title="Релиз $release"
    step_done 'подпись и контрольная сумма верны'

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
        [ -n "$POOL_TLS_DOMAIN" ] || die "в пуле не задан tls_domain, задайте его в панели"
        case "$arch" in
        x86_64) sum=$TELEMT_SHA256_X86_64 ;;
        aarch64) sum=$TELEMT_SHA256_AARCH64 ;;
        esac
        file="telemt-$arch-linux-gnu.tar.gz"
        step_detail "ставлю telemt $TELEMT_VERSION"
        fetch "https://github.com/telemt/telemt/releases/download/$TELEMT_VERSION/$file" "$work/$file" ||
            die_with_log "не удалось скачать telemt"
        echo "$sum  $work/$file" | sha256sum -c - || die "контрольная сумма $file не совпала"
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
        [ -n "$POOL_TLS_DOMAIN" ] || die "в пуле не задан tls_domain, задайте его в панели"
        if [ -e /opt/mtproxyl ]; then
            die "MTProxyL уже установлен в /opt/mtproxyl, запустите установщик без --preset"
        fi
        command -v bash >/dev/null 2>&1 || die "установщику MTProxyL нужен bash"
        case "$arch" in
        x86_64) sum=$TELEMT_SHA256_X86_64 ;;
        aarch64) sum=$TELEMT_SHA256_AARCH64 ;;
        esac
        file="telemt-$arch-linux-gnu.tar.gz"
        step_detail 'ставлю MTProxyL'
        fetch "https://raw.githubusercontent.com/Liafanx/MTProxyL/$MTPROXYL_COMMIT/install.sh" \
            "$work/mtproxyl-install.sh" || die_with_log "не удалось скачать установщик MTProxyL"
        echo "$MTPROXYL_INSTALLER_SHA256  $work/mtproxyl-install.sh" | sha256sum -c - ||
            die "контрольная сумма установщика MTProxyL не совпала"
        bash "$work/mtproxyl-install.sh" --branch "$MTPROXYL_COMMIT" -- \
            --mode manager --proxy-mode mtproto --port "$POOL_PORT" --sni "$POOL_TLS_DOMAIN" \
            --engine binary \
            --engine-url "https://github.com/telemt/telemt/releases/download/$TELEMT_VERSION/$file" \
            --engine-sha256 "$sum" \
            --zapret2 no --meko no </dev/null 3>&- 4>&- || die_with_log "установщик MTProxyL завершился с ошибкой"
    }

    # With --port and no preset there is nothing to look for or install.
    if [ -n "$preset" ] || [ -z "$engine_port" ]; then
        step_begin 'Движок'
        engine_done=
        case "$preset" in
        telemt)
            install_telemt
            systemctl daemon-reload
            systemctl enable --now telemt
            engine_done="telemt $TELEMT_VERSION запущен"
            ;;
        mtproxyl)
            install_mtproxyl
            engine_done='MTProxyL установлен'
            ;;
        esac
    fi
    if [ -n "$engine_port" ]; then
        found_port=$engine_port
    else
        config=$(engine_config)
        [ -n "$config" ] || die "движок не найден: поставьте его или добавьте --preset telemt"
        found_port=$(config_port "$config")
    fi
    if [ "$found_port" != "$POOL_PORT" ]; then
        die "Движок слушает порт $found_port, а пул ждёт $POOL_PORT: измените порт движка или пула в панели"
    fi
    if [ -n "$step_title" ]; then
        step_done "${engine_done:-найден, порт $found_port}"
    fi

    # --- the agent ---------------------------------------------------------------------------------

    step_begin 'Агент'
    # A node that is installed again keeps its id until the pool tells it otherwise.
    old_id=$(read_node_id)
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
    step_done 'служба mtpool-agent запущена'

    # --- the pool: is the node in? ------------------------------------------------------------------

    step_begin 'Регистрация'
    step_detail 'жду ответа пула'
    node_id=
    deadline=$(($(date +%s) + ENROLL_WAIT))
    while :; do
        current=$(read_node_id)
        if [ -n "$current" ]; then
            if [ "$current" != "$old_id" ] || pool_knows "$current"; then
                node_id=$current
                break
            fi
        fi
        [ "$(date +%s)" -lt "$deadline" ] || enroll_timed_out
        sleep 1
    done
    # A long name does not fit the column.
    if [ "${#node_id}" -le 24 ]; then
        step_done "нода $node_id в пуле"
    else
        step_done 'нода в пуле'
    fi

    # --- the pool: is the node well? ----------------------------------------------------------------

    waiting_for_pool=1
    step_begin 'Проверка пулом'
    step_detail 'жду первого отчёта'
    state=
    checks=
    deadline=$(($(date +%s) + HEALTH_WAIT))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        body=$(curl -fsS --max-time 5 "$POOL_URL/api/v1/nodes/$node_id" 2>/dev/null) || body=
        if [ -n "$body" ]; then
            state=$(printf '%s' "$body" | jq -r '.state // empty') || state=
            checks=$(checks_text "$body")
            if [ -n "$checks" ]; then
                step_detail "$checks"
            fi
            case "$state" in
            healthy | banned) break ;;
            esac
        fi
        sleep 5
    done
    waiting_for_pool=0

    case "$state" in
    healthy)
        step_done "${checks:-пройдена}"
        printf '\n  %s\n\n' "$(paint '1;32' "Готово: $node_id здорова и работает в пуле.")" >&3
        show_node_links
        ;;
    banned)
        reported=1
        fail_step 'пул заблокировал IP этой ноды'
        printf '\n' >&3
        show_node_links
        exit 1
        ;;
    *)
        step_end "$(paint 33 "$wait_mark")" 'проверка ещё идёт'
        printf '\n  Нода установлена, пул ещё проверяет её. Обычно это занимает несколько минут.\n\n' >&3
        show_node_links
        ;;
    esac
}

main "$@"
