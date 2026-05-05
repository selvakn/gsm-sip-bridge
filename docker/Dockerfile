ARG PJSIP_VERSION=2.16

# Stage 1: Build PJSIP
FROM debian:bookworm-slim AS pjsip-builder
ARG PJSIP_VERSION
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential wget ca-certificates pkg-config libssl-dev libuuid1 uuid-dev libasound2-dev && \
    rm -rf /var/lib/apt/lists/*
RUN wget -q "https://github.com/pjsip/pjproject/archive/refs/tags/${PJSIP_VERSION}.tar.gz" && \
    tar xzf "${PJSIP_VERSION}.tar.gz" && \
    cd "pjproject-${PJSIP_VERSION}" && \
    ./configure --prefix=/usr/local --enable-shared --disable-video --disable-v4l2 --with-external-pa=no && \
    make -j$(nproc) dep && make -j$(nproc) && make install && \
    ldconfig

# Stage 2: Build Rust workspace
FROM rust:bookworm AS rust-builder
COPY --from=pjsip-builder /usr/local /usr/local
RUN ldconfig
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config clang libclang-dev libasound2-dev libusb-1.0-0-dev libudev-dev && \
    rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml deny.toml ./
COPY pjsua-sys/ pjsua-sys/
COPY pjsua-safe/ pjsua-safe/
COPY gsm-sip-bridge/ gsm-sip-bridge/
ENV PKG_CONFIG_PATH="/usr/local/lib/pkgconfig"
RUN cargo build --release --workspace --features gsm-sip-bridge/pjsip-linked

# Stage 3: Runtime
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    libasound2 libusb-1.0-0 libudev1 libssl3 ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*
COPY --from=pjsip-builder /usr/local/lib/*.so* /usr/local/lib/
RUN ldconfig

RUN groupadd -r bridge && useradd -r -g bridge -d /data bridge
COPY --from=rust-builder /app/target/release/gsm-sip-bridge /usr/local/bin/
COPY --from=rust-builder /app/target/release/gsm-echo /usr/local/bin/
COPY --from=rust-builder /app/target/release/sip-echo /usr/local/bin/
COPY etc/99-ec20-gsm-sip-bridge.rules /etc/udev/rules.d/

RUN apt-get update && apt-get install -y --no-install-recommends gosu && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /data /etc/gsm-sip-bridge && chown bridge:bridge /data
VOLUME ["/data"]

COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -sf http://localhost:9091/metrics || exit 1

ENTRYPOINT ["entrypoint.sh"]
CMD ["--config", "/etc/gsm-sip-bridge/config.toml"]
