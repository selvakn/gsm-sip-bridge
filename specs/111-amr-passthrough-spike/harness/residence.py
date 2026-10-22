# Time each RTP packet spends inside B: arrival from :a vs departure to :b, matched by (ssrc, seq).
import struct, sys, statistics
d = open(sys.argv[1], "rb").read(); off = 24; a, b = int(sys.argv[2]), int(sys.argv[3]); tin = {}; dl = []
while off < len(d):
    sec, usec, incl, _ = struct.unpack("<IIII", d[off:off+16]); pkt = d[off+16:off+16+incl]; off += 16 + incl
    ip = pkt[14:] if pkt[12:14] == b"\x08\x00" else pkt[16:]
    if not ip or ip[0] >> 4 != 4 or ip[9] != 17: continue
    ihl = (ip[0] & 15) * 4; sp, dp = struct.unpack(">HH", ip[ihl:ihl+4]); u = ip[ihl+8:]
    if len(u) < 12: continue
    key = (struct.unpack(">I", u[8:12])[0], struct.unpack(">H", u[2:4])[0]); t = sec + usec / 1e6
    if sp == a: tin[key] = t
    elif dp == b and key in tin: dl.append((t - tin[key]) * 1e6)
dl.sort()
print(f"  :{a} -> B -> :{b}: n={len(dl)} median={statistics.median(dl):.0f}us p99={dl[int(len(dl)*.99)]:.0f}us max={dl[-1]:.0f}us")
