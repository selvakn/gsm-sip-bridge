#!/bin/bash
# usage: run_gsm.sh <tag> <b-binary> <b-args...>   X calls B directly; B --gsm
tag=$1; bbin=$2; shift 2
cd /lab/out; rm -f $tag.*
/lab/$bbin --port 5070 --secs 8 "$@" > $tag.b.log 2>&1 & BP=$!
sleep 1
(sleep 5; echo h; sleep 1; echo q) | /opt/stock/pjsua --no-tcp --null-audio --local-port 5060 --rtp-port 4000 --no-vad \
   --dis-codec=speex --dis-codec=iLBC --dis-codec=GSM --dis-codec=PCMA --dis-codec=G722 --dis-codec=AMR --dis-codec=opus --dis-codec=L16 \
   "sip:b@127.0.0.1:5070;transport=udp" > $tag.x.log 2>&1
wait $BP; echo "B exit=$?"
grep -E "^B\||Assert|assert" $tag.b.log | grep -v "prio="
