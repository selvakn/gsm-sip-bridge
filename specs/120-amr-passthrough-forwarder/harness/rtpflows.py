"""Summarise and compare the RTP flows in a pcap (Ethernet or Linux 'any' capture).

  rtpflows.py cap.pcap                 list UDP flows: packets, payload types
  rtpflows.py cap.pcap frames A apack B bpack   compare AMR-WB frames across packings (oa|be)
  rtpflows.py cap.pcap SRC DST         compare two flows' RTP payloads in order
                                       (SRC/DST are 'ip:port->ip:port' as listed)
Payloads compare as bytes after the 12-byte RTP header; the payload type is
excluded because forwarding relabels it per leg (FR-008).
"""
import struct, sys
from collections import Counter, OrderedDict

def packets(path):
    data = open(path, "rb").read()
    magic = struct.unpack("<I", data[:4])[0]
    le = magic in (0xa1b2c3d4, 0xa1b23c4d)
    e = "<" if le else ">"
    link = struct.unpack(e + "I", data[20:24])[0]
    off = 24
    while off + 16 <= len(data):
        ts_s, ts_f, cap, _ = struct.unpack(e + "IIII", data[off:off + 16]); off += 16
        pkt = data[off:off + cap]; off += cap
        if link == 276: ip = pkt[20:]; proto = struct.unpack(">H", pkt[0:2])[0]
        elif link == 113: ip = pkt[16:]; proto = struct.unpack(">H", pkt[14:16])[0]
        elif link == 1: ip = pkt[14:]; proto = struct.unpack(">H", pkt[12:14])[0]
        else: continue
        if proto == 0x0800 and ip and ip[0] >> 4 == 4:
            ihl = (ip[0] & 15) * 4
            if ip[9] != 17: continue
            src = ".".join(map(str, ip[12:16])); dst = ".".join(map(str, ip[16:20])); u = ip[ihl:]
        elif proto == 0x86dd:
            if ip[6] != 17: continue
            import ipaddress
            src = str(ipaddress.IPv6Address(ip[8:24])); dst = str(ipaddress.IPv6Address(ip[24:40])); u = ip[40:]
        else: continue
        sp, dp, ln = struct.unpack(">HHH", u[:6])
        yield ts_s + ts_f / 1e6, f"{src}:{sp}", f"{dst}:{dp}", u[8:ln]

def is_rtp(p): return len(p) >= 12 and p[0] >> 6 == 2 and (p[1] & 0x7f) not in range(72, 77)

def flows(path):
    f = OrderedDict()
    for t, s, d, p in packets(path):
        if is_rtp(p):
            f.setdefault(f"{s}->{d}", []).append((t, p))
    return f

WB_BITS = {0: 132, 1: 177, 2: 253, 3: 285, 4: 317, 5: 365, 6: 397, 7: 461, 8: 477, 9: 40, 14: 0, 15: 0}

def parse_wb(payload, octet_aligned):
    """AMR-WB RTP payload -> (cmr, [(frame_type, quality, speech_bits_as_int, nbits)])."""
    bits = int.from_bytes(payload, "big"); total = len(payload) * 8
    def get(pos, n): return (bits >> (total - pos - n)) & ((1 << n) - 1)
    cmr = get(0, 4); pos = 8 if octet_aligned else 4
    toc = []
    while True:
        f, ft, q = get(pos, 1), get(pos + 1, 4), get(pos + 5, 1)
        pos += 8 if octet_aligned else 6
        toc.append((ft, q))
        if not f: break
    frames = []
    for ft, q in toc:
        n = WB_BITS[ft]
        if octet_aligned: pos = (pos + 7) // 8 * 8
        frames.append((ft, q, get(pos, n) if n else 0, n)); pos += n
    return cmr, frames

def compare_frames(a, a_oa, b, b_oa):
    """Per packet, same CMR, frame types and speech bits regardless of packing; matched by RTP seq."""
    # RFC 4733 events (4-byte payloads) share the flow; they are compared separately.
    def audio_pt(f): return Counter(p[1] & 0x7f for _, p in f).most_common(1)[0][0]
    pa, pb = audio_pt(a), audio_pt(b)
    ev_a = [p for _, p in a if p[1] & 0x7f != pa]; ev_b = [p for _, p in b if p[1] & 0x7f != pb]
    a = [x for x in a if x[1][1] & 0x7f == pa]; b = [x for x in b if x[1][1] & 0x7f == pb]
    ib = {p[2:4]: p for _, p in b}
    paired = same = 0
    print(f"telephone-event packets: {len(ev_a)} vs {len(ev_b)}; payloads identical: {sorted(set(p[12:] for p in ev_a)) == sorted(set(p[12:] for p in ev_b))}")
    for _, p in a:
        q = ib.get(p[2:4])
        if q is None: continue
        paired += 1
        if parse_wb(p[12:], a_oa) == parse_wb(q[12:], b_oa) and p[8:12] == q[8:12] and p[4:8] == q[4:8]:
            same += 1
    print(f"{len(a)} vs {len(b)} pkts; matched by seq: {paired}; identical CMR+frame types+speech bits (and ts, ssrc): {same}")

if __name__ == "__main__" and len(sys.argv) == 7 and sys.argv[2] == "frames":
    f = flows(sys.argv[1])
    compare_frames(f[sys.argv[3]], sys.argv[4] == "oa", f[sys.argv[5]], sys.argv[6] == "oa")
    sys.exit(0)

if __name__ == "__main__":
    f = flows(sys.argv[1])
    if len(sys.argv) == 2:
        for k, v in f.items():
            pts = Counter(p[1] & 0x7f for _, p in v)
            print(f"{k:55} {len(v):5} pkts  PT {dict(pts)}")
    else:
        a, b = f[sys.argv[2]], f[sys.argv[3]]
        pa = [(p[2:4], p[4:8], p[8:12], p[12:]) for _, p in a if (p[1] & 0x7f) not in range(100, 128) or True]
        pb = [(p[2:4], p[4:8], p[8:12], p[12:]) for _, p in b]
        # match by sequence number so a leading/trailing partial overlap doesn't shift the pairing
        ib = {x[0]: x for x in pb}
        paired = [(x, ib[x[0]]) for x in pa if x[0] in ib]
        same = sum(1 for x, y in paired if x == y)
        print(f"{sys.argv[2]}: {len(pa)} pkts  {sys.argv[3]}: {len(pb)} pkts  matched by seq: {len(paired)}  identical (seq,ts,ssrc,payload): {same}")
