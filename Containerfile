# homestack-server image. Build from the repository root:
#
#   docker build -f Containerfile -t homestack-server .
#
# Configuration and secrets: docs/migration/deploy.md.

FROM docker.io/library/rust:1.99.0-slim-trixie AS build
# Lets scripts remove this stage (about 2 GB) after a build.
LABEL org.homestack.stage=build
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
RUN cargo build --release --locked -p hs-server --bin homestack-server

FROM docker.io/library/debian:trixie-20260918-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/homestack-server /usr/local/bin/homestack-server
# Run with --user matching the owner of the mounted secret files.
USER 65534:65534
ENV HOMESTACK_LISTEN=0.0.0.0:8080 \
    HOMESTACK_LOG_FORMAT=json
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/homestack-server"]
