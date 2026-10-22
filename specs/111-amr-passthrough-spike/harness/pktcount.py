# per-direction RTP packet counts and payload types for the X(4000) and Y(6000) legs
import struct, sys, collections
d = open(sys.argv[1], "rb").read(); off = 24; c = collections.Counter(); t = {}
while off < len(d):
    sec, usec, incl, _ = struct.unpack("<IIII", d[off:off+16]); pkt = d[off+16:off+16+incl]; off += 16 + incl
    ip = pkt[14:] if pkt[12:14] == b"\x08\x00" else pkt[16:]
    if not ip or ip[0] >> 4 != 4 or ip[9] != 17: continue
    ihl = (ip[0] & 15) * 4; sp, dp = struct.unpack(">HH", ip[ihl:ihl+4]); u = ip[ihl+8:]
    if len(u) < 12 or u[0] >> 6 != 2 or sp in (5060, 5070, 5080) or sp % 2: continue
    key = None
    for p, n in ((4000, "X"), (6000, "Y")):
        if sp == p: key = f"{n}->B"
        if dp == p: key = f"B->{n}"
    if key:
        c[(key, u[1] & 0x7f)] += 1; t.setdefault(key, [sec + usec/1e6, 0]); t[key][1] = sec + usec/1e6
for (k, pt), n in sorted(c.items()):
    dur = t[k][1] - t[k][0]
    print(f"  {k:5s} pt={pt:3d} pkts={n:5d} over {dur:5.1f}s = {n/dur if dur else 0:5.1f} pps")
