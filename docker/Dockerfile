# gsm-sip-bridge — one image, two optional subsystems:
#   1. the circuit-switched GSM-to-SIP daemon (always attempted; no-ops
#      gracefully if no supported modem is attached)
#   2. the inbound VoWiFi-to-SIP bridge (specs/011-vowifi-sip-bridge) — an
#      ePDG tunnel + vowifi-ims-agent/vowifi-sip-agent, gated on
#      [vowifi].enabled in the mounted config.toml. Two tunnel engines ship
#      side by side (specs/012-strongswan-epdg), selected at deploy time by
#      TUNNEL_ENGINE: the original SWu-IKEv2 Python dialer ("swu", default
#      during the proving period) or strongSwan ("strongswan" — proper IKE
#      rekeying/re-auth/DPD/MOBIKE and a network namespace that survives
#      reconnects, via a pcscd/vpcd bridge to the SIM inside the modem).
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

# Stage 5: build strongSwan (specs/012-strongswan-epdg) — the osmocom
# "strongswan-epdg" fork, which adds two things upstream strongSwan lacks
# and this feature needs: the P-CSCF config-attribute request (`p-cscf`
# plugin) and USIM/AKA authentication via a PC/SC card (`eap-sim-pcsc`
# patched for real USIM auth, not just test vectors). Pinned to a specific
# `jolly/work` commit (not floating on the branch) — validated to build
# clean on Alpine/musl at this commit; research.md item 8's "does the fork
# build on musl" risk is resolved: yes, with one extra package (`gperf`)
# beyond what research.md anticipated.
FROM alpine:${ALPINE_VERSION} AS strongswan-builder
RUN apk add --no-cache \
        build-base autoconf automake libtool pkgconf gettext-dev \
        openssl-dev pcsc-lite-dev linux-headers git bison flex gperf

WORKDIR /build
RUN git clone https://gitea.osmocom.org/ims-volte-vowifi/strongswan-epdg.git \
    && git -C strongswan-epdg checkout 022f3e515b2c7b607def0fbbf1d7b9b1df08fb8c
WORKDIR /build/strongswan-epdg

RUN autoreconf -if
# --disable-defaults: only the plugins this feature actually uses get
# built — smaller image, smaller attack surface, and avoids dragging in
# build deps (e.g. gmp, curl, ldap) for crypto backends/features we don't
# need (--enable-openssl covers IKE crypto and DH on its own).
RUN ./configure --prefix=/usr --sysconfdir=/etc \
        --disable-defaults \
        --enable-charon --enable-ikev2 --enable-swanctl --enable-vici \
        --enable-eap-aka --enable-eap-sim --enable-eap-sim-pcsc \
        --enable-eap-identity --enable-p-cscf --enable-openssl \
        --enable-random --enable-nonce --enable-hmac \
        --enable-socket-default --enable-kernel-netlink --enable-resolve \
        --enable-updown --enable-stroke \
        CFLAGS="-fPIC"
RUN make -j"$(nproc)"
RUN make install DESTDIR=/staging

# Stage 6: build vpcd (specs/012-strongswan-epdg) — vsmartcard's virtual
# PC/SC reader, the pcscd-side driver that `eap-sim-pcsc` (above) talks to
# and that `vowifi-usim-bridge` (gsm-sip-bridge itself) connects to as a
# TCP client, forwarding APDUs to the SIM inside the modem over AT+CSIM.
# Only the `vpcd`/`ifd-vpcd` subprojects are built — not `vicc`/`vpicc`
# (interactive client-side testing tools, not needed at runtime).
# Pinned to a specific master commit for reproducibility.
FROM alpine:${ALPINE_VERSION} AS vpcd-builder
RUN apk add --no-cache \
        build-base autoconf automake libtool pkgconf git \
        pcsc-lite-dev python3 help2man

WORKDIR /build
RUN git clone https://github.com/frankmorgner/vsmartcard.git \
    && git -C vsmartcard checkout 809675dc982addfc3fdb8cbaf177e4430477b0b2
WORKDIR /build/vsmartcard/virtualsmartcard

RUN autoreconf -vis
# Explicit serialdropdir/serialconfdir: the configure-computed defaults
# double up the prefix with pkg-config's already-absolute usbdropdir
# (producing /usr/usr/...) — verified while proving this stage; pin the
# correct absolute paths instead.
RUN ./configure --prefix=/usr \
        --enable-serialdropdir=/usr/lib/pcsc/drivers/serial \
        --enable-serialconfdir=/etc/reader.conf.d
RUN make -j"$(nproc)" -C src/vpcd
RUN make -j"$(nproc)" -C src/ifd-vpcd
RUN make -C src/vpcd install DESTDIR=/staging
RUN make -C src/ifd-vpcd install DESTDIR=/staging

# Stage 7: runtime image — gsm-sip-bridge (stage 2), the SWu-IKEv2 Python
# dialer (stage 3), and the strongSwan/vpcd engine (stages 5-6), on plain
# Alpine with only the shared libs all of them need. Both ePDG tunnel
# engines ship in the one image, selectable at deploy time via
# TUNNEL_ENGINE (specs/012-strongswan-epdg) — the legacy SWu engine stays
# until strongSwan is proven against both carriers (spec FR-001/FR-009).
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
# pcsc-lite-libs -> pyscard's runtime dependency (libpcsclite.so); also
#   charon's eap-sim-pcsc plugin and the vpcd driver (both stage 5/6
#   outputs, dynamically linked against it — confirmed via ldd while
#   proving those stages)
# pcsc-lite -> the pcscd daemon itself (specs/012-strongswan-epdg): bridges
#   charon's eap-sim-pcsc plugin to the vpcd virtual reader (stage 6)
# wget -> HEALTHCHECK's metrics-endpoint probe
RUN apk add --no-cache \
        bash \
        python3 \
        iproute2 net-tools iputils bind-tools procps \
        alsa-lib libssl3 libcrypto3 libuuid eudev-libs libgcc \
        opencore-amr vo-amrwbenc \
        pcsc-lite-libs pcsc-lite \
        ca-certificates wget

RUN addgroup -S bridge && adduser -S -G bridge -h /data bridge

COPY --from=python-builder /opt/SWu-IKEv2 /opt/SWu-IKEv2
COPY --from=python-builder /opt/pydeps /opt/pydeps
ENV PYTHONPATH="/opt/pydeps"

COPY --from=rust-builder /app/target/release/gsm-sip-bridge /usr/local/bin/gsm-sip-bridge
COPY etc/99-ec20-gsm-sip-bridge.rules /etc/udev/rules.d/

# strongSwan (specs/012-strongswan-epdg): charon + swanctl + the plugins
# built in stage 5, plus the vpcd virtual-reader driver from stage 6 and
# its pcscd reader.conf.d entry. Copied selectively (not the whole
# /staging tree) — man pages, headers, and the legacy `ipsec`/starter CLI
# aren't needed at runtime; charon/swanctl are driven via swanctl.conf and
# this project's own entrypoint.sh, not the ipsec.conf/starter workflow.
COPY --from=strongswan-builder /staging/usr/libexec/ipsec/charon /usr/libexec/ipsec/charon
COPY --from=strongswan-builder /staging/usr/sbin/swanctl /usr/sbin/swanctl
COPY --from=strongswan-builder /staging/usr/lib/ipsec /usr/lib/ipsec
COPY --from=strongswan-builder /staging/etc/strongswan.conf /etc/strongswan.conf
COPY --from=strongswan-builder /staging/etc/strongswan.d /etc/strongswan.d
COPY --from=vpcd-builder /staging/usr/lib/pcsc/drivers/serial /usr/lib/pcsc/drivers/serial
COPY --from=vpcd-builder /staging/etc/reader.conf.d/vpcd /etc/reader.conf.d/vpcd

# This project's own strongSwan config overrides (specs/012-strongswan-epdg,
# docker/strongswan/): p-cscf's `enable { ims = yes }` block and the extra
# reliability settings, replacing/adding to the fork's empty-by-default
# strongswan.d templates. swanctl-epdg.conf.template is rendered by
# entrypoint.sh at runtime (needs the SIM's IMSI), not baked in here.
COPY docker/strongswan/charon-logging.conf /etc/strongswan.d/charon-logging.conf
COPY docker/strongswan/charon-extra.conf /etc/strongswan.d/charon-extra.conf
COPY docker/strongswan/p-cscf.conf /etc/strongswan.d/charon/p-cscf.conf
COPY docker/strongswan/ims.updown /etc/strongswan.d/ims.updown
COPY docker/strongswan/swanctl-epdg.conf.template /etc/strongswan.d/swanctl-epdg.conf.template
COPY docker/strongswan/swanctl.conf /etc/swanctl/swanctl.conf
RUN chmod +x /etc/strongswan.d/ims.updown \
    && mkdir -p /etc/swanctl/conf.d

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
