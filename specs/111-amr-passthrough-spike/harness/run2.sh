#!/bin/bash
# usage: run2.sh <tag> <b2-args...>   X(AMR-WB) -> B2 -> Y(AMR-WB); DTMF + hold/unhold from X
tag=$1; shift
cd /lab/out; rm -f $tag.*
dis="--dis-codec=speex --dis-codec=iLBC --dis-codec=GSM --dis-codec=PCMU --dis-codec=PCMA --dis-codec=G722 --dis-codec=AMR/8000 --dis-codec=opus --dis-codec=L16"
tcpdump -i lo -w $tag.pcap -s 0 udp >/dev/null 2>&1 & TD=$!
sleep 1
/lab/b2 --port 5070 --peer "sip:y@127.0.0.1:5080;transport=udp" --secs 15 "$@" > $tag.b.log 2>&1 & BP=$!
(sleep 15; echo q) | /opt/stock/pjsua --no-tcp --null-audio --local-port 5080 --rtp-port 6000 --auto-answer 200 \
   --play-file /lab/out/speech.wav --auto-play --no-vad $dis > $tag.y.log 2>&1 &
sleep 1
(sleep 3; echo "#"; sleep 0.3; echo 123; sleep 3; echo H; sleep 2; echo v; sleep 3; echo h; sleep 1; echo q) | \
  /opt/stock/pjsua --no-tcp --null-audio --local-port 5060 --rtp-port 4000 --no-vad \
   --rec-file /lab/out/$tag.x.wav --auto-rec $dis "sip:b@127.0.0.1:5070;transport=udp" > $tag.x.log 2>&1
wait $BP; sleep 1; kill $TD; wait 2>/dev/null
grep "^B|" $tag.b.log
echo "-- flows"; python3 -I /lab/flows.py $tag.pcap
echo "-- AMR-WB frames Y->B vs B->X";  python3 -I /lab/amrdiff.py $tag.pcap 6000 4000 wb
echo "-- AMR-WB frames X->B vs B->Y";  python3 -I /lab/amrdiff.py $tag.pcap 4000 6000 wb
echo "-- DTMF seen by Y"; grep -i "dtmf" $tag.y.log | head -5
echo "-- X call events"; grep -E "state changed|media is|hold|Hold" $tag.x.log | head -12
echo "-- exact payload compare"; python3 -I /lab/bytecmp.py $tag.pcap 6000 4000; python3 -I /lab/bytecmp.py $tag.pcap 4000 6000
