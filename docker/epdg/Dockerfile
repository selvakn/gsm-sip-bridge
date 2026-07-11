# VoWiFi ePDG client (SWu-IKEv2) — establishes an IKEv2/IPsec tunnel to the
# operator ePDG using the SIM inside the modem (EAP-AKA via AT+CSIM).
# Target: Vodafone (Vi) India, MCC 404 / MNC 43, Quectel EC200U on /dev/ttyUSB6.
# See README.md for usage. Standalone — not part of the main gsm-sip-bridge image.
FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

# Runtime + build deps:
#   build-essential/swig/python3-dev/libpcsclite-dev -> build pyscard
#   iproute2 + net-tools -> the dialer calls BOTH `ip` and net-tools `route`
#   iputils-ping/dnsutils/procps -> entrypoint DNS resolve + keepalive
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 python3-pip python3-dev \
        build-essential swig libpcsclite-dev \
        git ca-certificates \
        iproute2 net-tools iputils-ping dnsutils procps \
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
COPY patches/0001-ec200u-at-csim-fixes.patch /tmp/
RUN patch --binary -p1 -d SWu-IKEv2 < /tmp/0001-ec200u-at-csim-fixes.patch \
    && rm /tmp/0001-ec200u-at-csim-fixes.patch

# Python deps. Use pyserial (NOT the 'serial' PyPI pkg) to provide `import serial`.
RUN pip install --no-cache-dir --break-system-packages \
        wheel pyserial requests cryptography pycryptodome pyscard \
    && pip install --no-cache-dir --break-system-packages \
        git+https://github.com/mitshell/CryptoMobile.git

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
COPY healthcheck.sh /usr/local/bin/healthcheck.sh
RUN chmod +x /usr/local/bin/entrypoint.sh /usr/local/bin/healthcheck.sh

WORKDIR /opt/SWu-IKEv2
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
