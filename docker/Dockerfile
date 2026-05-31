ARG PJSIP_VERSION=2.16

# Stage 1: Build PJSIP as static libraries on Alpine
FROM alpine:3.21 AS pjsip-builder
ARG PJSIP_VERSION
RUN apk add --no-cache \
    build-base wget pkgconf openssl-dev alsa-lib-dev util-linux-dev linux-headers
RUN wget -q "https://github.com/pjsip/pjproject/archive/refs/tags/${PJSIP_VERSION}.tar.gz" && \
    tar xzf "${PJSIP_VERSION}.tar.gz" && \
    cd "pjproject-${PJSIP_VERSION}" && \
    ./configure --prefix=/usr/local --disable-shared --enable-static \
        --disable-video --disable-v4l2 --with-external-pa=no \
        CFLAGS="-fPIC" && \
    make -j$(nproc) dep && make -j$(nproc) && make install

# Stage 2: Generate bindgen bindings (needs glibc + libclang)
FROM debian:bookworm-slim AS bindgen-stage
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config clang libclang-dev ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*
COPY --from=pjsip-builder /usr/local/include /usr/local/include
COPY --from=pjsip-builder /usr/local/lib/pkgconfig /usr/local/lib/pkgconfig
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.94.0 --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
WORKDIR /gen
COPY pjsua-sys/Cargo.toml pjsua-sys/build.rs ./pjsua-sys/
COPY pjsua-sys/src/ ./pjsua-sys/src/
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY pjsua-safe/Cargo.toml ./pjsua-safe/Cargo.toml
COPY pjsua-safe/src/ ./pjsua-safe/src/
COPY gsm-sip-bridge/Cargo.toml ./gsm-sip-bridge/Cargo.toml
COPY gsm-sip-bridge/src/ ./gsm-sip-bridge/src/
RUN cargo build -p pjsua-sys --release 2>&1 || true
RUN find /gen/target -name "bindings.rs" -path "*/pjsua-sys-*" | head -1 | xargs -I{} cp {} /gen/bindings.rs
RUN test -s /gen/bindings.rs || (echo "ERROR: bindings.rs not generated" && exit 1)

# Stage 3: Build Rust on Alpine with static PJSIP
FROM alpine:3.21 AS rust-builder
RUN apk add --no-cache \
    curl gcc g++ musl-dev pkgconf linux-headers \
    alsa-lib-dev alsa-lib libusb-dev eudev-dev \
    openssl-dev openssl-libs-static \
    util-linux-dev util-linux-static
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.94.0 --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
COPY --from=pjsip-builder /usr/local/lib /usr/local/lib
COPY --from=pjsip-builder /usr/local/include /usr/local/include
COPY --from=bindgen-stage /gen/bindings.rs /tmp/bindings.rs
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml deny.toml ./
COPY pjsua-sys/ pjsua-sys/
COPY pjsua-safe/ pjsua-safe/
COPY gsm-sip-bridge/ gsm-sip-bridge/
ENV PKG_CONFIG_PATH="/usr/local/lib/pkgconfig"
ENV PJSUA_SYS_BINDINGS="/tmp/bindings.rs"
ENV PJSUA_SYS_STATIC="1"
ENV OPENSSL_STATIC="1"
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN cargo build --release --workspace --features gsm-sip-bridge/pjsip-linked

# Stage 4: Minimal Alpine runtime
FROM alpine:3.21 AS runtime
RUN apk add --no-cache alsa-lib eudev-libs ca-certificates wget \
    libgcc libuuid libssl3 libcrypto3

RUN addgroup -S bridge && adduser -S -G bridge -h /data bridge
COPY --from=rust-builder /app/target/release/gsm-sip-bridge /usr/local/bin/
COPY etc/99-ec20-gsm-sip-bridge.rules /etc/udev/rules.d/

RUN mkdir -p /data /etc/gsm-sip-bridge && chown bridge:bridge /data
VOLUME ["/data"]

COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD wget -qO- http://localhost:9091/metrics >/dev/null || exit 1

ENTRYPOINT ["entrypoint.sh"]
CMD ["--config", "/etc/gsm-sip-bridge/config.toml"]
