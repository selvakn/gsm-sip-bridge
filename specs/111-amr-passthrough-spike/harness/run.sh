#!/bin/bash
# Reproduce spike #111 phase 1. Builds pjproject 2.16 twice (stock, and
# switchboard+passthrough), compiles the "Agent B" harness against both, and
# runs every experiment on loopback inside one throwaway container. Results
# land in ../results/. No privileges needed; no network beyond the pjproject
# tarball download.
set -euo pipefail
H=$(cd "$(dirname "$0")" && pwd); R=$H/../results; W=$(mktemp -d)
cp "$H"/* "$W"/; mkdir -p "$W/out"
docker build -t spike111-lab "$W" >/dev/null
docker run --rm -v "$W":/lab spike111-lab sh -c '
  set -e
  gcc -g -o /lab/b-stock /lab/b.c $(PKG_CONFIG_PATH=/opt/stock/lib/pkgconfig pkg-config --cflags --libs --static libpjproject)
  gcc -g -o /lab/b-passthru /lab/b.c -Wl,--allow-multiple-definition $(PKG_CONFIG_PATH=/opt/passthru/lib/pkgconfig pkg-config --cflags --libs --static libpjproject)
  python3 -I /lab/gen_wav.py /lab/out/speech.wav
  echo "== R1 registration"; for b in "b-stock" "b-passthru" "b-passthru --pt-amr"; do /lab/$b --secs 1 2>&1 | grep "^B|"; done
  echo "== R1 call, opencore kept";    /lab/run_call.sh r1a b-passthru --pt-amr; python3 -I /lab/amrdiff.py /lab/out/r1a.pcap 6000 4000 || true
  echo "== R1 call, clean passthrough"; /lab/run_call.sh r1d b-passthru --pt-amr --no-opencore --ptime20 --clock 8000; python3 -I /lab/amrdiff.py /lab/out/r1d.pcap 6000 4000
  echo "== R2 Agent B ringback";       /lab/run_call.sh r2a b-passthru --pt-amr --no-opencore --ptime20 --clock 8000 --ringback
  echo "== R2 GSM-style slot 0";       /lab/run_gsm.sh r2d b-passthru --gsm --clock 8000
  echo "== R3 matrix";                 /lab/r3.sh
' | tee "$R/run-$(date +%Y%m%d-%H%M%S).txt"
