# Every UDP flow touching X (4000/4001) or Y (6000/6001), with packet counts and RTP payload types.
import struct, sys, collections
d = open(sys.argv[1], "rb").read(); off = 24; c = collections.Counter(); pts = collections.defaultdict(collections.Counter)
while off < len(d):
    _, _, incl, _ = struct.unpack("<IIII", d[off:off+16]); pkt = d[off+16:off+16+incl]; off += 16 + incl
    ip = pkt[14:] if pkt[12:14] == b"\x08\x00" else pkt[16:]
    if not ip or ip[0] >> 4 != 4 or ip[9] != 17: continue
    ihl = (ip[0] & 15) * 4; sp, dp = struct.unpack(">HH", ip[ihl:ihl+4]); u = ip[ihl+8:]
    if not ({sp, dp} & {4000, 4001, 6000, 6001}): continue
    c[(sp, dp)] += 1
    if dp % 2 == 0 and sp % 2 == 0 and len(u) > 1: pts[(sp, dp)][u[1] & 0x7f] += 1
    elif len(u) > 1: pts[(sp, dp)]["rtcp%d" % u[1]] += 1
for (sp, dp), n in sorted(c.items()):
    print(f"  :{sp:<5} -> :{dp:<5} {n:5d} pkts  {dict(pts[(sp, dp)])}")
