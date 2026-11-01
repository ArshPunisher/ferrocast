# Ferrocast Bond (Pro) as a small container, with ffmpeg for its Be right back picture.
# Built from the repo root:
#   docker build -f deploy/bond.Dockerfile -t ferrocast-bond .
FROM rust:1.99-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p ferrocast-bond \
    && strip target/release/ferrocast-bond

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10002 --home /nonexistent bond
COPY --from=build /src/target/release/ferrocast-bond /usr/local/bin/ferrocast-bond
USER bond
ENV BOND_HTTP=0.0.0.0:8795 BOND_SRTLA=0.0.0.0:5000 BOND_SRT=0.0.0.0:5001 BOND_LOG=info
EXPOSE 8795 5000/udp 5001/udp 40000-40099/udp
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD ["/bin/bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/8795 && printf 'GET /health HTTP/1.0\\r\\n\\r\\n' >&3 && grep -q ok <&3"]
ENTRYPOINT ["/usr/local/bin/ferrocast-bond"]
