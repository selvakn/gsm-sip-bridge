# Exact payload comparison: RTP payloads sent from portA vs payloads delivered to portB, in order.
import sys
sys.argv = sys.argv[:4]
exec(open(__import__("os").path.join(__import__("os").path.dirname(__file__), "amrdiff.py")).read().split("def collect")[0])
pcap, a, b = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
pa = [(p[3], p[4], p[5], p[6]) for p in rtp(pcap, lambda s, d: s == a)]
pb = [(p[3], p[4], p[5], p[6]) for p in rtp(pcap, lambda s, d: d == b)]
same = sum(1 for x, y in zip(pa, pb) if x == y)
print(f"  :{a}->B {len(pa)} pkts, B->:{b} {len(pb)} pkts; identical (seq, ts, ssrc, payload) in order: {same}/{min(len(pa), len(pb))}")
