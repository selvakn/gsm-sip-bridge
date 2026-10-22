# AMR-capable softphone for the live passthrough checks (specs/120): Ubuntu's
# baresip ships the amr module; tcpdump + python3 are for the pcap compare.
FROM ubuntu:24.04
RUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
      baresip-core tcpdump python3 procps netcat-openbsd >/dev/null && rm -rf /var/lib/apt/lists/*
