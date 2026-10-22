#!/bin/bash
# Risk 3 matrix on the passthru build. X = veth stand-in, Y = PBX/phone stand-in.
run() { # tag clock xcodec ycodec b-extra...
  tag=$1 clock=$2 xc=$3 yc=$4; shift 4
  echo "######## $tag: X=$xc  Y=$yc  conf clock=$clock  $*"
  XCODEC=$xc YCODEC=$yc XEXTRA="--add-codec=$xc" YEXTRA="--add-codec=$yc" \
    /lab/run_call.sh $tag b-passthru --clock $clock --prio L16/16000 1 "$@" | grep -E "PAIR|media active|Assert|assert"
  python3 -I /lab/pktcount.py /lab/out/$tag.pcap
}
run r3a 16000 L16/16000 G722     # wideband veth -> G.722 PBX (today's common case)
run r3b 16000 L16/16000 PCMU     # wideband veth -> PCMU PBX
run r3c 16000 PCMU      G722     # narrowband veth -> G.722 PBX
run r3d 16000 PCMU      PCMU     # narrowband both
run r3e 8000  L16/16000 G722     # same as r3a but at the 8k clock passthrough-NB needs
run r3f 8000  PCMU      AMR/8000 --pt-amr --no-opencore --ptime20   # PCM veth -> passthrough-AMR phone
run r3g 16000 L16/16000 AMR-WB   # wideband veth -> opencore AMR-WB phone (spec 085 path)
