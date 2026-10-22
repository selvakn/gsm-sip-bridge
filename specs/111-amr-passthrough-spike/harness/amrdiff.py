# Compare AMR(-NB) frames carried on two RTP legs of a pcap.
# usage: amrdiff.py pcap <portA> <portB>   (frames sent FROM portA vs frames sent TO portB)
import struct, sys
FL_NB = [95,103,118,134,148,159,204,244,39,0,0,0,0,0,0,0]  # bits per FT (8 = SID)
def bits(b):
    for byte in b:
        for i in range(7,-1,-1): yield (byte >> i) & 1
def parse(payload, octet):
    """returns (cmr, [(ft,q,framebits-tuple)])"""
    if octet:
        cmr = payload[0] >> 4; i = 1; tocs = []
        while True:
            t = payload[i]; i += 1; tocs.append(((t>>3)&15, (t>>2)&1))
            if not t & 0x80: break
        frames = []
        for ft, q in tocs:
            n = FL_NB[ft]; nb = (n + 7)//8
            fb = tuple(list(bits(payload[i:i+nb]))[:n]); i += nb
            frames.append((ft, q, fb))
        return cmr, frames
    bs = list(bits(payload)); p = 0
    cmr = int("".join(map(str, bs[0:4])), 2); p = 4; tocs = []
    while True:
        f = bs[p]; ft = int("".join(map(str, bs[p+1:p+5])), 2); q = bs[p+5]; p += 6
        tocs.append((ft, q))
        if not f: break
    frames = []
    for ft, q in tocs:
        n = FL_NB[ft]; frames.append((ft, q, tuple(bs[p:p+n]))); p += n
    return cmr, frames
def rtp(pcap, pred):
    d = open(pcap, "rb").read(); off = 24
    link = struct.unpack("<I", d[20:24])[0]
    while off < len(d):
        _, _, incl, _ = struct.unpack("<IIII", d[off:off+16]); pkt = d[off+16:off+16+incl]; off += 16 + incl
        l2 = 16 if link == 113 else (14 if link == 1 else 0)   # SLL / ethernet / raw
        ip = pkt[l2:]
        if ip[0] >> 4 != 4 or ip[9] != 17: continue
        ihl = (ip[0] & 15) * 4; sp, dp = struct.unpack(">HH", ip[ihl:ihl+4]); u = ip[ihl+8:]
        if len(u) < 12 or u[0] >> 6 != 2: continue
        pt = u[1] & 0x7f
        if pt >= 96 and pred(sp, dp):
            seq, ts, ssrc = struct.unpack(">HII", u[2:12]); yield sp, dp, pt, seq, ts, ssrc, u[12:]
def collect(pcap, pred, octet):
    frames = []; cmrs = set(); meta = []
    for sp, dp, pt, seq, ts, ssrc, pl in rtp(pcap, pred):
        cmr, fr = parse(pl, octet); cmrs.add(cmr); frames += fr; meta.append((seq, ts, ssrc, len(fr)))
    return frames, cmrs, meta
pcap, a, b = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]); octet = len(sys.argv) > 4
fa, ca, ma = collect(pcap, lambda s, d: s == a, octet)
fb, cb, mb = collect(pcap, lambda s, d: d == b, octet)
from collections import Counter
print(f"leg-in  (from :{a}) pkts={len(ma)} frames={len(fa)} CMRs={sorted(ca)} FTs={dict(Counter(f[0] for f in fa))} ssrc={sorted({m[2] for m in ma})}")
print(f"leg-out (to   :{b}) pkts={len(mb)} frames={len(fb)} CMRs={sorted(cb)} FTs={dict(Counter(f[0] for f in fb))} ssrc={sorted({m[2] for m in mb})}")
print(f"frames/pkt in={Counter(m[3] for m in ma)} out={Counter(m[3] for m in mb)}")
# align on the longest common run of speech frames
sa = [f for f in fa if f[0] < 8]; sb = [f for f in fb if f[0] < 8]
idx = {f[2]: i for i, f in enumerate(sa)}
start = next((j for j, f in enumerate(sb) if f[2] in idx), None)
if start is None:
    print("RESULT: no speech frame of the out-leg appears bit-identical on the in-leg -> frames were RE-ENCODED"); sys.exit(1)
i0 = idx[sb[start][2]]; n = 0
while start+n < len(sb) and i0+n < len(sa) and sb[start+n][2] == sa[i0+n][2]: n += 1
print(f"RESULT: {n} consecutive speech frames bit-identical (out-leg speech frames={len(sb)}, offset in={i0} out={start})")
