import sys, struct, collections
sys.argv += []
exec(open(__import__("os").path.join(__import__("os").path.dirname(__file__), "amrdiff.py")).read().split("pcap, a, b = sys.argv")[0])
pcap, port, direction = sys.argv[1], int(sys.argv[2]), sys.argv[3]
pred = (lambda s, d: s == port) if direction == "from" else (lambda s, d: d == port)
d = open(pcap, "rb").read(); off = 24; prev = None; arr = []
link = struct.unpack("<I", d[20:24])[0]
pk = list(rtp(pcap, pred))
ts = [p[4] for p in pk]; seq = [p[3] for p in pk]
print("ts deltas", collections.Counter((b - a) & 0xffffffff for a, b in zip(ts, ts[1:])).most_common(4))
print("seq deltas", collections.Counter((b - a) & 0xffff for a, b in zip(seq, seq[1:])).most_common(3))
pat = "".join("S" if f[0] < 8 else ("D" if f[0] == 15 else "x") for p in pk[:15] for f in parse(p[6], False)[1])
print("first frames:", pat)
