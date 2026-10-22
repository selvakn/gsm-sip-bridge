# usage: cmp2.py pcap src_port fwd_in_port fwd_out_port dst_port : payload identity through the forwarder (post-upgrade packets only)
import sys
a=sys.argv[:]; sys.argv=sys.argv[:2]
exec(open(__import__("os").path.join(__import__("os").path.dirname(__file__), "amrdiff.py")).read().split("def collect")[0])
pcap,src,fin,fout,dst=a[1],int(a[2]),int(a[3]),int(a[4]),int(a[5])
pa=[(p[3],p[4],p[5],p[6][1:] if False else p[6]) for p in rtp(pcap, lambda s,d: s==src and d==fin)]
pb=[(p[3],p[4],p[5],p[6]) for p in rtp(pcap, lambda s,d: s==fout and d==dst)]
same=sum(1 for x,y in zip(pa,pb) if x==y); n=min(len(pa),len(pb))
print(f"  :{src}->:{fin} {len(pa)} pkts, :{fout}->:{dst} {len(pb)} pkts; identical (seq,ts,ssrc,payload) in order: {same}/{n}")
