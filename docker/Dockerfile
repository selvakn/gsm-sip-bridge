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
#
# Alpine/musl throughout (not Debian/glibc): the only reason this was ever
# Debian was to build pyscard in the same stage it shipped in. Splitting the
# Python build into its own stage removes that constraint, and Alpine's
# apk repos carry every native dependency this image needs — PJSIP's own
# build deps, the AMR-NB/AMR-WB codec libs, and pcsclite for pyscard — so
# there's no reason to carry glibc's and Debian's much larger base + apt
# footprint. See docker/patches/ for the one carrier/modem-specific patch
# applied to the vendored SWu-IKEv2 dialer.
ARG PJSIP_VERSION=2.16
ARG ALPINE_VERSION=3.21

# Stage 1: build PJSIP as static libraries.
#
# Only PJSIP itself ends up statically linked (--disable-shared
# --enable-static below); OpenSSL, ALSA, libuuid, and libudev stay ordinary
# dynamic Alpine runtime packages — see pjsua-sys/build.rs's `system_libs`
# set, which force-dynamic-links exactly those even when PJSUA_SYS_STATIC=1.
FROM alpine:${ALPINE_VERSION} AS pjsip-builder
ARG PJSIP_VERSION
RUN apk add --no-cache \
        build-base wget pkgconf \
        openssl-dev alsa-lib-dev util-linux-dev linux-headers

# PJSIP's build-time config: enables L16/16000, which the VoWiFi bridge's
# wideband path needs and which pjproject does not register by default.
COPY docker/pjsip-config-site.h /tmp/config_site.h

RUN wget -q "https://github.com/pjsip/pjproject/archive/refs/tags/${PJSIP_VERSION}.tar.gz" && \
    tar xzf "${PJSIP_VERSION}.tar.gz" && \
    cd "pjproject-${PJSIP_VERSION}" && \
    cp /tmp/config_site.h pjlib/include/pj/config_site.h && \
    ./configure --prefix=/usr/local --disable-shared --enable-static \
        --disable-video --disable-v4l2 --with-external-pa=no \
        CFLAGS="-fPIC" && \
    make -j"$(nproc)" dep && make -j"$(nproc)" && make install

# Stage 2: build gsm-sip-bridge against the static PJSIP from stage 1.
FROM rust:1-alpine${ALPINE_VERSION} AS rust-builder
RUN apk add --no-cache \
        build-base pkgconf clang-dev \
        openssl-dev alsa-lib-dev util-linux-dev eudev-dev linux-headers \
        opencore-amr-dev vo-amrwbenc-dev
COPY --from=pjsip-builder /usr/local /usr/local

WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml deny.toml ./
COPY pjsua-sys/ pjsua-sys/
COPY pjsua-safe/ pjsua-safe/
COPY amr-sys/ amr-sys/
COPY amr-safe/ amr-safe/
COPY gsm-sip-bridge/ gsm-sip-bridge/

ENV PKG_CONFIG_PATH="/usr/local/lib/pkgconfig"
ENV PJSUA_SYS_STATIC="1"
# musl's default Rust target statically links the whole binary, including
# libc — which would prevent the dynamic libs above (ssl, asound, uuid,
# udev) from resolving at link/run time. -crt-static off keeps those dynamic,
# matching how the previous Debian/glibc build linked them.
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN cargo build --release -p gsm-sip-bridge --bin gsm-sip-bridge \
    --features gsm-sip-bridge/pjsip-linked,gsm-sip-bridge/amr-linked

# Stage 3: build the SWu-IKEv2 Python dialer's dependencies (pyscard et al)
# in isolation, so the compiler toolchain that only exists to build them
# (build-base, swig, python3-dev, pcsc-lite-dev) never reaches the runtime
# image. `pip install --target` (not a venv) so the result is a plain
# directory tree that COPYs cleanly into the runtime stage without needing
# a matching venv interpreter symlink there.
FROM alpine:${ALPINE_VERSION} AS python-builder
RUN apk add --no-cache \
        python3 py3-pip python3-dev \
        build-base swig pcsc-lite-dev \
        git

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
RUN pip install --no-cache-dir --target=/opt/pydeps \
        wheel pyserial requests cryptography pycryptodome pyscard \
    && pip install --no-cache-dir --target=/opt/pydeps \
        git+https://github.com/mitshell/CryptoMobile.git

# Stage 4: runtime image — gsm-sip-bridge (stage 2) + the SWu-IKEv2 Python
# dialer (stage 3), on plain Alpine with only the shared libs both need.
FROM alpine:${ALPINE_VERSION}

# bash: docker/entrypoint.sh and healthcheck.sh use arrays, process
#   substitution, and /dev/tcp/* redirection — none of which busybox ash
#   (Alpine's default /bin/sh) supports.
# python3 -> must match python-builder's python3 (same ALPINE_VERSION base)
#   for the compiled pyscard/cryptography extensions copied in below to load.
# iproute2 + net-tools -> the SWu dialer calls BOTH `ip` and net-tools `route`;
#   also used directly by entrypoint.sh for the vowifi bridge's veth pair
# iputils/bind-tools/procps -> entrypoint DNS resolve (dig) + keepalive + pgrep
# alsa-lib/libssl3/libcrypto3/libuuid/eudev-libs/libgcc -> runtime shared libs
#   for the gsm-sip-bridge binary (PJSIP itself is statically linked; these
#   are its dynamic dependencies — eudev-libs via the `serialport` crate,
#   the rest per pjsua-sys/build.rs's `system_libs` set)
# opencore-amr/vo-amrwbenc -> AMR encode/decode runtime libs (amr-linked is
#   enabled in the build above). Needed in both directions: an *outbound*
#   offer that doesn't list AMR-WB is rejected even when the network then
#   negotiates PCMU (commit 830ac87), and an *inbound* carrier offer
#   frequently carries no PCMU at all — Airtel offers AMR-WB + AMR-NB on some
#   mobile-terminating calls and AMR-NB alone on others, so without both
#   codecs those calls can only be declined.
# pcsc-lite-libs -> pyscard's runtime dependency (libpcsclite.so)
# wget -> HEALTHCHECK's metrics-endpoint probe
RUN apk add --no-cache \
        bash \
        python3 \
        iproute2 net-tools iputils bind-tools procps \
        alsa-lib libssl3 libcrypto3 libuuid eudev-libs libgcc \
        opencore-amr vo-amrwbenc \
        pcsc-lite-libs \
        ca-certificates wget

RUN addgroup -S bridge && adduser -S -G bridge -h /data bridge

COPY --from=python-builder /opt/SWu-IKEv2 /opt/SWu-IKEv2
COPY --from=python-builder /opt/pydeps /opt/pydeps
ENV PYTHONPATH="/opt/pydeps"

COPY --from=rust-builder /app/target/release/gsm-sip-bridge /usr/local/bin/gsm-sip-bridge
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
