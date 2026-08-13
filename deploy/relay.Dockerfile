# The Ferrocast relay as a small container (ARCHITECTURE.md §1). Built from the repo root:
#   docker build -f deploy/relay.Dockerfile -t ferrocast-relay .
FROM rust:1.99-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p ferrocast-relay \
    && strip target/release/ferrocast-relay

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /data relay \
    && mkdir -p /data && chown relay /data
COPY --from=build /src/target/release/ferrocast-relay /usr/local/bin/ferrocast-relay
USER relay
ENV RELAY_BIND=0.0.0.0:8787 RELAY_DATA=/data/relay.db RUST_LOG=info
EXPOSE 8787
VOLUME /data
# Bash (part of every Debian image) for /dev/tcp: no curl needed in the image.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --start-interval=2s CMD ["/bin/bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/8787 && printf 'GET /health HTTP/1.0\\r\\n\\r\\n' >&3 && grep -q ok <&3"]
ENTRYPOINT ["/usr/local/bin/ferrocast-relay"]
