# ── Stage 1: Build ─────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder

WORKDIR /build

# Install native build dependencies required by crypto crates (ring, aws-lc-rs).
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       build-essential cmake pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Cache dependencies by copying only manifests first.
COPY Cargo.toml ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --bin dclone-relay; \
    rm -rf src

# Now copy the real source and do the final build.
COPY src ./src
RUN touch src/main.rs && cargo build --release --bin dclone-relay

# ── Stage 2: Runtime ────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/dclone-relay /usr/local/bin/dclone-relay

# TCP/QUIC relay port + HTTP health-check port
EXPOSE 4001/tcp
EXPOSE 4001/udp
EXPOSE 4002/tcp

ENV RUST_LOG=info,libp2p_relay=debug

ENTRYPOINT ["/usr/local/bin/dclone-relay"]
