# A node of the stand: systemd, telemt and the agent. Build from the repository root:
# docker build -f e2e/images/node/Dockerfile .
FROM rust:1.99-alpine AS build
RUN apk add --no-cache build-base
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY deploy ./deploy
# The agent verifies releases with this key: the public half of the stand's test key.
COPY e2e/keys/minisign.pub ./deploy/minisign.pub
# One target directory per tag: cargo goes by file times, so a directory shared with another working
# copy would leave that copy's key and code in the binary, and the layer cache would keep them.
# Within a tag the key can still change, and a key older than the last build would be missed.
ARG MTPOOL_E2E_TAG=local
RUN touch deploy/minisign.pub
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=mtpool-e2e-target-${MTPOOL_E2E_TAG},target=/src/target,sharing=locked \
    cargo build --release --locked -p mtpool-agent \
 && install -D target/release/mtpool-agent /out/mtpool-agent \
 && cp Cargo.toml /tmp/Cargo.toml.orig && cp Cargo.lock /tmp/Cargo.lock.orig \
 && for v in 0.0.1 0.9.1; do \
      sed -i "s/^version = \"0.0.0\"/version = \"$v\"/" Cargo.toml \
   && cargo build --release -p mtpool-agent \
   && install -D target/release/mtpool-agent /out/agent-$v \
   && cp /tmp/Cargo.toml.orig Cargo.toml && cp /tmp/Cargo.lock.orig Cargo.lock; \
    done

FROM ubuntu:24.04
ENV container=docker DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends systemd systemd-sysv iptables ipset ca-certificates curl iproute2 \
 && rm -rf /var/lib/apt/lists/* \
 && systemctl mask getty.target console-getty.service systemd-logind.service \
      systemd-timesyncd.service apt-daily.timer apt-daily-upgrade.timer \
      dpkg-db-backup.timer e2scrub_all.timer fstrim.timer motd-news.timer

# telemt is downloaded, never kept in the repository. The archive is checked against a pinned hash.
ARG TELEMT_VERSION=3.5.16
ARG TELEMT_SHA256_X86_64=e6f46b2f3becf29bfd555cec8a059e429f7c082c1af402499b3a7940cf8ebd34
ARG TELEMT_SHA256_AARCH64=1e1bc87f05923b9673cbbaf34a3e50f7fdcf7d496cd24efcdb34c278ed18a659
RUN set -eu; \
    arch="$(uname -m)"; \
    case "$arch" in \
      x86_64) sha="$TELEMT_SHA256_X86_64" ;; \
      aarch64) sha="$TELEMT_SHA256_AARCH64" ;; \
      *) echo "no telemt build for $arch" >&2; exit 1 ;; \
    esac; \
    file="telemt-$arch-linux-gnu.tar.gz"; \
    curl -fsSL -o "/tmp/$file" "https://github.com/telemt/telemt/releases/download/$TELEMT_VERSION/$file"; \
    echo "$sha  /tmp/$file" | sha256sum -c -; \
    tar -xzf "/tmp/$file" -C /usr/local/bin telemt; \
    rm "/tmp/$file"; \
    telemt --version

COPY --from=build /out/mtpool-agent /usr/local/bin/mtpool-agent
# Builds of the same agent under another version, for the rollout scenarios to offer as releases.
COPY --from=build /out/agent-0.0.1 /opt/e2e/agent-0.0.1
COPY --from=build /out/agent-0.9.1 /opt/e2e/agent-0.9.1
# The units and the engine's starting configuration are the ones the installer puts on a real
# node. Only the agent's environment is the stand's: plain http to the registrar, private
# addresses and the stand-ins for the services outside the pool.
COPY deploy/systemd/telemt.service deploy/systemd/mtpool-agent.service \
     deploy/systemd/mtpool-agent-rollback.service /etc/systemd/system/
COPY deploy/systemd/telemt.toml /tmp/telemt.toml
RUN mkdir -p /etc/mtpool /etc/telemt /var/lib/mtpool /etc/systemd/system/multi-user.target.wants \
 && sed -e 's/__PORT__/443/' -e 's/__TLS_DOMAIN__/www.example.com/' \
        -e 's/__SECRET__/00112233445566778899aabbccddeeff/' /tmp/telemt.toml > /etc/telemt/telemt.toml \
 && rm /tmp/telemt.toml \
 && ln -s /etc/systemd/system/telemt.service /etc/systemd/system/multi-user.target.wants/telemt.service
RUN printf '%s\n' \
      'MTPOOL_ALLOW_PRIVATE_IPS=true' \
      'MTPOOL_GLOBALPING_URL=http://fakes:9000/globalping/' \
      'MTPOOL_IP_SOURCES=http://fakes:9000/ip,http://fakes:9000/ip?2,http://fakes:9000/ip?3,http://fakes:9000/ip?4' \
      'MTPOOL_IPSUM_URL=http://fakes:9000/ipsum' \
      'MTPOOL_AGENT_UPDATE_POLL_SEC=2' \
      > /etc/mtpool/agent.env

STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
