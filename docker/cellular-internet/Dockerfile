# Cellular-internet sidecar (specs/032-cellular-internet-sidecar).
#
# A deliberately small image whose only job is to bring up internet over an
# EC20's QMI data path (/dev/cdc-wdm0) and report — via a Docker healthcheck —
# only when the internet is *actually* reachable (a DNS probe). It ships NOTHING
# of the bridge; the bridge image is unchanged and unaffected.
#
# QMI only: this never opens the modem's AT port, which the bridge needs for
# AT+CSIM. See docs/ec20-internet-plus-vowifi.md.
FROM alpine:3.20

# libqmi   -> qmicli (QMI control over /dev/cdc-wdm0)
# iproute2 -> `ip` (raw-ip link + address/route config)
# bind-tools optional at runtime; busybox `nslookup` is the default probe tool.
RUN apk add --no-cache libqmi iproute2

COPY docker/cellular-internet/internet-lib.sh /usr/local/bin/internet-lib.sh
COPY docker/cellular-internet/internet-entrypoint.sh /usr/local/bin/internet-entrypoint.sh
COPY docker/cellular-internet/internet-healthcheck.sh /usr/local/bin/internet-healthcheck.sh
RUN chmod +x /usr/local/bin/internet-entrypoint.sh /usr/local/bin/internet-healthcheck.sh

# Timings from the spec (grace 90s, interval 10s); kept in step with the Compose
# override's healthcheck, whichever is more specific wins at runtime.
HEALTHCHECK --start-period=90s --interval=10s --timeout=5s --retries=1 \
    CMD ["/usr/local/bin/internet-healthcheck.sh"]

ENTRYPOINT ["/usr/local/bin/internet-entrypoint.sh"]
