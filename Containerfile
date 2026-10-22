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

# The node image (`--target node`), published as
# ghcr.io/hipotures/homestack-server and run by homestack-deploy as a
# Proxmox container from this OCI image (phase 11): runit starts the server
# on 127.0.0.1:8080 and Caddy on :443 in front of it (deploy/lxc).
FROM docker.io/library/alpine:3.24.2 AS node
RUN apk add --no-cache ca-certificates caddy runit \
    && adduser -S -D -H -h /var/lib/homestack -s /sbin/nologin homestack \
    && mkdir -p /home/app \
    && install -d -m 0700 -o homestack -g nogroup /var/lib/homestack
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/homestack-server /usr/local/bin/homestack-server
COPY deploy/lxc/Caddyfile /etc/caddy/Caddyfile
COPY deploy/lxc/service /etc/service
EXPOSE 443
STOPSIGNAL SIGTERM
ENTRYPOINT ["runsvdir", "-P", "/etc/service"]

# The build stage targets musl and links statically, so the runtime needs
# nothing but CA certificates.
FROM docker.io/library/alpine:3.24.2
# /var/lib/homestack holds the uploaded server.toml; mount a volume there.
RUN apk add --no-cache ca-certificates \
    && install -d -m 0700 -o 65534 -g 65534 /var/lib/homestack
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/homestack-server /usr/local/bin/homestack-server
# Run with --user matching the owner of the mounted secret files and of
# /var/lib/homestack (65534 in a fresh named volume).
USER 65534:65534
ENV HOMESTACK_LISTEN=0.0.0.0:8080 \
    HOMESTACK_LOG_FORMAT=json \
    HOMESTACK_CONFIG_FILE=/var/lib/homestack/server.toml
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/homestack-server"]
