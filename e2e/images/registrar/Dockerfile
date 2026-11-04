# The registrar and the stand-ins for the external services, for the end-to-end stand, and a
# bare Ubuntu with systemd for the registrar installer to run on (the `fresh` target).
# Build from the repository root: docker build -f e2e/images/registrar/Dockerfile .
FROM rust:1.99-alpine AS build
RUN apk add --no-cache build-base
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY deploy ./deploy
# The registrar verifies nothing itself but hands the key to the installer it serves: the public
# half of the stand's test key.
COPY e2e/keys/minisign.pub ./deploy/minisign.pub
# The target directory is a cache shared by every build of this image, and cargo goes by the
# file's time: a key older than the last build would leave that build's key in the binary.
RUN touch deploy/minisign.pub
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p mtpool-registrar -p mtpool-fakes \
 && install -D target/release/mtpool-registrar /out/mtpool-registrar \
 && install -D target/release/mtpool-fakes /out/mtpool-fakes

# A machine the registrar installer has never seen: Ubuntu 24.04 with systemd, curl and the
# deploy directory the installer reads its units from. It has no registrar, no Caddy, no minisign.
FROM ubuntu:24.04 AS fresh
ENV container=docker DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends systemd systemd-sysv ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && systemctl mask getty.target console-getty.service systemd-logind.service \
      systemd-timesyncd.service apt-daily.timer apt-daily-upgrade.timer \
      dpkg-db-backup.timer e2scrub_all.timer fstrim.timer motd-news.timer
COPY deploy /opt/deploy
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]

# The load generator (`e2e/load.sh`). Built only when asked for: `--target loadgen`.
FROM build AS build-load
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p mtpool-loadgen \
 && install -D target/release/mtpool-loadgen /out/mtpool-loadgen

FROM alpine:3.21 AS loadgen
COPY --from=build-load /out/mtpool-loadgen /usr/local/bin/
ENTRYPOINT ["mtpool-loadgen"]

FROM alpine:3.21
COPY --from=build /out/mtpool-registrar /out/mtpool-fakes /usr/local/bin/
RUN mkdir -p /var/lib/mtpool /srv/releases
CMD ["mtpool-registrar", "serve"]
