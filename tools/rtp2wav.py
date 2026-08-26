#!/usr/bin/env python3
"""Turn the veth RTP of a bridged call into a listenable WAV.

Agent A transcodes the carrier's AMR-WB to L16/16000 before handing it to
Agent B, so capturing on the veth gets speech out of a call without decoding
AMR or touching the Gm ESP at all — which is what makes this the fastest way
to hear what a carrier's announcement actually says:

    # on the host running the bridge, while the call is placed:
    sudo tcpdump -i veth-sip0 -n -s 0 -w /tmp/call.pcap udp
    # Agent B's RTP port is in the log line
    #   "Agent B advertised a non-veth RTP address ... using=10.99.0.2:<port>"
    ./tools/rtp2wav.py /tmp/call.pcap /tmp/call.wav <port>

Reads classic pcap (not pcapng) and needs nothing but the stdlib. L16 is
big-endian network order (RFC 3551 §4.5.11) and WAV is little-endian, hence
the byte swap.
"""

import struct
import sys
import wave


def main() -> None:
    if len(sys.argv) != 4:
        raise SystemExit(f"usage: {sys.argv[0]} <pcap> <out.wav> <dst-port>")
    pcap, out, want_dst_port = sys.argv[1], sys.argv[2], int(sys.argv[3])

    data = open(pcap, "rb").read()
    magic = data[:4]
    if magic == b"\xd4\xc3\xb2\xa1":
        endian = "<"
    elif magic == b"\xa1\xb2\xc3\xd4":
        endian = ">"
    else:
        raise SystemExit(f"not a classic pcap (pcapng is not supported): {magic!r}")
    linktype = struct.unpack(endian + "I", data[20:24])[0]

    off = 24
    packets = []
    while off + 16 <= len(data):
        _ts_s, _ts_us, caplen, _wirelen = struct.unpack(endian + "IIII", data[off : off + 16])
        off += 16
        frame = data[off : off + caplen]
        off += caplen

        ip = frame[14:] if linktype == 1 else frame  # 1 = Ethernet
        if len(ip) < 20 or (ip[0] >> 4) != 4 or ip[9] != 17:  # IPv4 + UDP only
            continue
        udp = ip[(ip[0] & 0xF) * 4 :]
        if len(udp) < 8:
            continue
        _sport, dport, ulen = struct.unpack("!HHH", udp[:6])
        if dport != want_dst_port:
            continue
        payload = udp[8:ulen]
        if len(payload) < 12:  # RTP fixed header
            continue
        packets.append((struct.unpack("!H", payload[2:4])[0], payload[1] & 0x7F, payload[12:]))

    if not packets:
        raise SystemExit(f"no RTP to port {want_dst_port} in {pcap}")
    print(f"packets={len(packets)} payload_types={sorted({p[1] for p in packets})}")

    packets.sort(key=lambda p: p[0])  # by RTP sequence number
    pcm = b"".join(p[2] for p in packets)
    swapped = bytearray(len(pcm))
    swapped[0::2], swapped[1::2] = pcm[1::2], pcm[0::2]

    with wave.open(out, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(16000)
        w.writeframes(bytes(swapped))
    print(f"wrote {out}  {len(pcm) / 2 / 16000:.2f}s")


if __name__ == "__main__":
    main()
