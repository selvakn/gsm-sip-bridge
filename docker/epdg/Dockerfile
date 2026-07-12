# VoWiFi ePDG client (SWu-IKEv2) + the inbound VoWiFi-to-SIP bridge agents
# (specs/011-vowifi-sip-bridge) — establishes an IKEv2/IPsec tunnel to the
# operator ePDG using the SIM inside the modem (EAP-AKA via AT+CSIM), then
# runs vowifi-ims-agent/vowifi-sip-agent to answer inbound calls.
# Target: Vodafone (Vi) India, MCC 404 / MNC 43, Quectel EC200U on /dev/ttyUSB6.
# See README.md for usage. Standalone — not part of the main gsm-sip-bridge image.
#
# Build context is the repo root (see docker-compose.epdg.yml's `build.context`),
# not this directory, so the builder stage below can see the whole Cargo
# workspace.
ARG PJSIP_VERSION=2.16

# Stage 1: build gsm-sip-bridge with a real, statically-linked PJSIP.
#
# Debian doesn't package libpjproject, so it's built from source here —
# same approach docker/Dockerfile uses for the main image (there: Alpine,
# for a fully static binary; here: Debian/glibc, matching this image's
# runtime base, so only PJSIP itself needs static linking — OpenSSL,
# ALSA, and libuuid stay ordinary dynamic Debian runtime packages, already
# available in both this stage and the final runtime stage below).
FROM rust:1-bookworm AS builder
ARG PJSIP_VERSION
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential wget pkg-config \
        libssl-dev libasound2-dev uuid-dev libudev-dev \
        clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*

RUN wget -q "https://github.com/pjsip/pjproject/archive/refs/tags/${PJSIP_VERSION}.tar.gz" && \
    tar xzf "${PJSIP_VERSION}.tar.gz" && \
    cd "pjproject-${PJSIP_VERSION}" && \
    ./configure --prefix=/usr/local --disable-shared --enable-static \
        --disable-video --disable-v4l2 --with-external-pa=no \
        CFLAGS="-fPIC" && \
    make -j"$(nproc)" dep && make -j"$(nproc)" && make install

WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY pjsua-sys/ pjsua-sys/
COPY pjsua-safe/ pjsua-safe/
COPY amr-sys/ amr-sys/
COPY amr-safe/ amr-safe/
COPY gsm-sip-bridge/ gsm-sip-bridge/

ENV PKG_CONFIG_PATH="/usr/local/lib/pkgconfig"
# Statically link PJSIP's own libraries (pjsua-sys/build.rs); OpenSSL,
# ALSA, and libuuid are left dynamic — ordinary Debian runtime packages
# installed in the final stage below, no need to vendor those too.
ENV PJSUA_SYS_STATIC="1"
RUN cargo build --release -p gsm-sip-bridge --bin gsm-sip-bridge \
    --features gsm-sip-bridge/pjsip-linked

# Stage 2: runtime image — the SWu-IKEv2 dialer (Python) + the compiled
# gsm-sip-bridge binary (real PJSIP linked, from stage 1 above).
FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

# Runtime + build deps:
#   build-essential/swig/python3-dev/libpcsclite-dev -> build pyscard
#   iproute2 + net-tools -> the dialer calls BOTH `ip` and net-tools `route`
#   iputils-ping/dnsutils/procps -> entrypoint DNS resolve + keepalive
#   libasound2/libssl3/libuuid1/libudev1 -> runtime shared libs for the
#     gsm-sip-bridge binary above (PJSIP itself is statically linked; these
#     are its dynamic dependencies — libudev1 via the `serialport` crate's
#     port-enumeration support, the rest per pjsua-sys/build.rs's
#     `system_libs` set)
#   libvo-amrwbenc0/libopencore-amrwb0 -> AMR-WB encode/decode for the
#     diagnostic `ims-call` CLI (see gsm-sip-bridge's amr-linked feature,
#     not enabled in the build above — the bridge agents use PCMU only for
#     now, see specs/011-vowifi-sip-bridge/research.md item 3)
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 python3-pip python3-dev \
        build-essential swig libpcsclite-dev \
        git ca-certificates \
        iproute2 net-tools iputils-ping dnsutils procps \
        libasound2 libssl3 libuuid1 libudev1 libvo-amrwbenc0 libopencore-amrwb0 \
    && rm -rf /var/lib/apt/lists/*

# Vendor the dialer and the mitshell 'card' module. Per the osmocom wiki the
# 'card' package must be copied in as a local module — do NOT `pip install card`
# (that pulls an unrelated PyPI package).
WORKDIR /opt
RUN git clone --depth 1 https://github.com/fasferraz/SWu-IKEv2.git \
    && git clone --depth 1 https://github.com/mitshell/card.git \
    && cp -av card/card SWu-IKEv2/card

# Fixes needed for the Quectel EC200U / Vodafone India USIM — see the patch
# file for details (P2 byte + AT+CSIM response-chaining quirks).
# Upstream stores swu_emulator.py with CRLF line endings; our patch's context
# uses LF, so normalize before applying (patch --binary matches EOLs exactly).
COPY docker/epdg/patches/0001-ec200u-at-csim-fixes.patch /tmp/
RUN sed -i 's/\r$//' SWu-IKEv2/swu_emulator.py \
    && patch --binary -p1 -d SWu-IKEv2 < /tmp/0001-ec200u-at-csim-fixes.patch \
    && rm /tmp/0001-ec200u-at-csim-fixes.patch

# Python deps. Use pyserial (NOT the 'serial' PyPI pkg) to provide `import serial`.
RUN pip install --no-cache-dir --break-system-packages \
        wheel pyserial requests cryptography pycryptodome pyscard \
    && pip install --no-cache-dir --break-system-packages \
        git+https://github.com/mitshell/CryptoMobile.git

COPY docker/epdg/entrypoint.sh /usr/local/bin/entrypoint.sh
COPY docker/epdg/healthcheck.sh /usr/local/bin/healthcheck.sh
RUN chmod +x /usr/local/bin/entrypoint.sh /usr/local/bin/healthcheck.sh

COPY --from=builder /app/target/release/gsm-sip-bridge /usr/local/bin/gsm-sip-bridge
RUN mkdir -p /etc/gsm-sip-bridge

WORKDIR /opt/SWu-IKEv2
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
