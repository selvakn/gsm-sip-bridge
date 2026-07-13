# gsm-sip-bridge — one image, two optional subsystems:
#   1. the circuit-switched GSM-to-SIP daemon (always attempted; no-ops
#      gracefully if no supported modem is attached)
#   2. the inbound VoWiFi-to-SIP bridge (specs/011-vowifi-sip-bridge) — the
#      ePDG SWu-IKEv2 tunnel + vowifi-ims-agent/vowifi-sip-agent, gated on
#      [vowifi].enabled in the mounted config.toml
#
# docker/entrypoint.sh decides at container start which of these to launch,
# by asking the binary itself (`gsm-sip-bridge config vowifi-enabled`)
# rather than hand-parsing TOML in bash.
ARG PJSIP_VERSION=2.16

# Stage 1: build gsm-sip-bridge with a real, statically-linked PJSIP.
#
# Debian doesn't package libpjproject, so it's built from source here.
# Single glibc/Debian stage (no separate Alpine+bindgen split like this
# Dockerfile used before) — the runtime image below needs to be
# glibc/Debian anyway for the SWu-IKEv2 dialer's Python toolchain
# (pyscard/pcsclite), so there's no cross-libc mismatch to work around by
# splitting stages. Only PJSIP itself is statically linked; OpenSSL, ALSA,
# libuuid, and libudev stay ordinary dynamic Debian runtime packages,
# already available in both this stage and the runtime stage below.
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

# Only installed now, after PJSIP's own ./configure/build: if these -dev
# packages are present earlier, PJSIP's configure auto-detects them and
# tries to statically link its own bundled AMR-WB codec plugin against
# them — but the apt packages only ship the shared .so, not a static .a,
# which breaks the fully-static PJSIP build above. Installing them here
# means only our own amr-sys crate's pkg-config probe (below) sees them;
# it links dynamically, which these packages support fine.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libvo-amrwbenc-dev libopencore-amrwb-dev libopencore-amrnb-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml deny.toml ./
COPY pjsua-sys/ pjsua-sys/
COPY pjsua-safe/ pjsua-safe/
COPY amr-sys/ amr-sys/
COPY amr-safe/ amr-safe/
COPY gsm-sip-bridge/ gsm-sip-bridge/

ENV PKG_CONFIG_PATH="/usr/local/lib/pkgconfig"
ENV PJSUA_SYS_STATIC="1"
RUN cargo build --release -p gsm-sip-bridge --bin gsm-sip-bridge \
    --features gsm-sip-bridge/pjsip-linked,gsm-sip-bridge/amr-linked

# Stage 2: runtime image — gsm-sip-bridge (real PJSIP linked, from stage 1)
# plus the SWu-IKEv2 Python dialer for the VoWiFi/ePDG tunnel.
FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

# Runtime + build deps:
#   build-essential/swig/python3-dev/libpcsclite-dev -> build pyscard
#   iproute2 + net-tools -> the SWu dialer calls BOTH `ip` and net-tools `route`;
#     also used directly by entrypoint.sh for the vowifi bridge's veth pair
#   iputils-ping/dnsutils/procps -> entrypoint DNS resolve + keepalive + pgrep
#   libasound2/libssl3/libuuid1/libudev1 -> runtime shared libs for the
#     gsm-sip-bridge binary above (PJSIP itself is statically linked; these
#     are its dynamic dependencies — libudev1 via the `serialport` crate,
#     the rest per pjsua-sys/build.rs's `system_libs` set)
#   libvo-amrwbenc0/libopencore-amrwb0/libopencore-amrnb0 -> AMR encode/decode
#     runtime libs (amr-linked is enabled in the build above). Needed in both
#     directions: an *outbound* offer that doesn't list AMR-WB is rejected even
#     when the network then negotiates PCMU (commit 830ac87), and an *inbound*
#     carrier offer frequently carries no PCMU at all — Airtel offers AMR-WB +
#     AMR-NB on some mobile-terminating calls and AMR-NB alone on others, so
#     without both codecs those calls can only be declined.
#   wget -> HEALTHCHECK's metrics-endpoint probe
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 python3-pip python3-dev \
        build-essential swig libpcsclite-dev \
        git ca-certificates wget \
        iproute2 net-tools iputils-ping dnsutils procps \
        libasound2 libssl3 libuuid1 libudev1 \
        libvo-amrwbenc0 libopencore-amrwb0 libopencore-amrnb0 \
    && rm -rf /var/lib/apt/lists/*

RUN addgroup --system bridge && adduser --system --ingroup bridge --home /data bridge

# Vendor the SWu-IKEv2 dialer and the mitshell 'card' module. Per the
# osmocom wiki the 'card' package must be copied in as a local module — do
# NOT `pip install card` (that pulls an unrelated PyPI package).
WORKDIR /opt
RUN git clone --depth 1 https://github.com/fasferraz/SWu-IKEv2.git \
    && git clone --depth 1 https://github.com/mitshell/card.git \
    && cp -av card/card SWu-IKEv2/card

# Fixes needed for the Quectel EC200U / Vodafone India USIM — see the patch
# file for details (P2 byte + AT+CSIM response-chaining quirks).
# Upstream stores swu_emulator.py with CRLF line endings; our patch's context
# uses LF, so normalize before applying (patch --binary matches EOLs exactly).
COPY docker/patches/0001-ec200u-at-csim-fixes.patch /tmp/
RUN sed -i 's/\r$//' SWu-IKEv2/swu_emulator.py \
    && patch --binary -p1 -d SWu-IKEv2 < /tmp/0001-ec200u-at-csim-fixes.patch \
    && rm /tmp/0001-ec200u-at-csim-fixes.patch

# Python deps. Use pyserial (NOT the 'serial' PyPI pkg) to provide `import serial`.
RUN pip install --no-cache-dir --break-system-packages \
        wheel pyserial requests cryptography pycryptodome pyscard \
    && pip install --no-cache-dir --break-system-packages \
        git+https://github.com/mitshell/CryptoMobile.git

COPY --from=builder /app/target/release/gsm-sip-bridge /usr/local/bin/gsm-sip-bridge
COPY etc/99-ec20-gsm-sip-bridge.rules /etc/udev/rules.d/

RUN mkdir -p /data /etc/gsm-sip-bridge && chown bridge:bridge /data
VOLUME ["/data"]

COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
COPY docker/healthcheck.sh /usr/local/bin/healthcheck.sh
RUN chmod +x /usr/local/bin/entrypoint.sh /usr/local/bin/healthcheck.sh

# Runs as root (not `bridge`): the VoWiFi tunnel subsystem needs
# CAP_NET_ADMIN/CAP_SYS_ADMIN for netns/veth/tun management, granted via
# `privileged: true` in docker-compose.yml — capabilities are only usable
# by a process that's actually root inside the container. Matches this
# image's behavior before this change too (the old entrypoint execed
# gsm-sip-bridge directly with no privilege drop).
WORKDIR /opt/SWu-IKEv2

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD /usr/local/bin/healthcheck.sh

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
