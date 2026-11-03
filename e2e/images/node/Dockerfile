# A node of the stand: systemd, telemt and the agent. Build from the repository root:
# docker build -f e2e/images/node/Dockerfile .
FROM rust:1.99-alpine AS build
RUN apk add --no-cache build-base
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p mtpool-agent \
 && install -D target/release/mtpool-agent /out/mtpool-agent

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
COPY e2e/images/node/telemt.service e2e/images/node/mtpool-agent.service /etc/systemd/system/
COPY e2e/images/node/telemt.toml /etc/telemt/telemt.toml
RUN mkdir -p /etc/mtpool /var/lib/mtpool /etc/systemd/system/multi-user.target.wants \
 && ln -s /etc/systemd/system/telemt.service /etc/systemd/system/multi-user.target.wants/telemt.service

STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
