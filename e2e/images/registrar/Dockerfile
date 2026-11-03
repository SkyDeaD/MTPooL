# The registrar and the stand-ins for the external services, for the end-to-end stand.
# Build from the repository root: docker build -f e2e/images/registrar/Dockerfile .
FROM rust:1.99-alpine AS build
RUN apk add --no-cache build-base
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p mtpool-registrar -p mtpool-fakes \
 && install -D target/release/mtpool-registrar /out/mtpool-registrar \
 && install -D target/release/mtpool-fakes /out/mtpool-fakes

FROM alpine:3.21
COPY --from=build /out/mtpool-registrar /out/mtpool-fakes /usr/local/bin/
RUN mkdir -p /var/lib/mtpool
CMD ["mtpool-registrar", "serve"]
