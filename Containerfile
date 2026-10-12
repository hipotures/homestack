# homestack-server image. Build from the repository root:
#
#   docker build -f Containerfile -t homestack-server .
#
# Configuration and secrets: docs/migration/deploy.md.

FROM docker.io/library/rust:1.99.0-alpine3.24 AS build
# Lets scripts remove this stage (about 2 GB) after a build.
LABEL org.homestack.stage=build
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
# Explicit target and crt-static: one self-contained binary that also runs
# outside this image (Alpine or Debian LXC alike).
ENV RUSTFLAGS="-C target-feature=+crt-static"
RUN cargo build --release --locked --target x86_64-unknown-linux-musl -p hs-server --bin homestack-server \
    && ! readelf -l target/x86_64-unknown-linux-musl/release/homestack-server | grep -q INTERP

# The build stage targets musl and links statically, so the runtime needs
# nothing but CA certificates.
FROM docker.io/library/alpine:3.24.2
RUN apk add --no-cache ca-certificates
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/homestack-server /usr/local/bin/homestack-server
# Run with --user matching the owner of the mounted secret files.
USER 65534:65534
ENV HOMESTACK_LISTEN=0.0.0.0:8080 \
    HOMESTACK_LOG_FORMAT=json
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/homestack-server"]
