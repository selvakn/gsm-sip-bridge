#!/bin/bash
# usage: run_call.sh <tag> <b-binary> <b-args...>
# X (stock pjsua, caller) -> B (harness) -> Y (stock pjsua, auto-answer, plays speech)
tag=$1; bbin=$2; shift 2
cd /lab/out; rm -f $tag.*
XCODEC=${XCODEC:-AMR/8000}; YCODEC=${YCODEC:-AMR/8000}
dis() { for c in speex iLBC GSM PCMU PCMA G722 AMR-WB opus L16 AMR/8000; do [ "$c" = "$1" ] || [ "${1#$c}" != "$1" ] || echo --dis-codec=$c; done; }
tcpdump -i lo -w $tag.pcap -s 0 udp >/dev/null 2>&1 & TD=$!
sleep 1
/lab/$bbin --port 5070 --peer "sip:y@127.0.0.1:5080;transport=udp" --secs 14 "$@" > $tag.b.log 2>&1 & BP=$!
(sleep 14; echo q) | /opt/stock/pjsua --no-tcp --null-audio --local-port 5080 --rtp-port 6000 --auto-answer 200 \
   --play-file /lab/out/speech.wav --auto-play --no-vad $(dis $YCODEC) $YEXTRA > $tag.y.log 2>&1 &
sleep 1
(sleep 11; echo h; sleep 1; echo q) | /opt/stock/pjsua --no-tcp --null-audio --local-port 5060 --rtp-port 4000 --no-vad \
   --rec-file /lab/out/$tag.x.wav --auto-rec $(dis $XCODEC) $XEXTRA "sip:b@127.0.0.1:5070;transport=udp" > $tag.x.log 2>&1
wait $BP; sleep 1; kill $TD; wait 2>/dev/null
grep "^B|" $tag.b.log | grep -v "prio=" 
