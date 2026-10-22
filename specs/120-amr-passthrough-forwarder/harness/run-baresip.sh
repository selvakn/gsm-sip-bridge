#!/bin/sh
# usage: run-baresip.sh <registrar-host> <user> <password> [codecs] [name] [console-port]
#   Registers a baresip (auto-answer) with AMR-WB first, host networking.
#   Commands go to its console over UDP:  echo /hold | nc -u -w1 127.0.0.1 5555
#   (digits during a call are sent as DTMF). Logs: docker logs <name>.
set -eu
host=$1 user=$2 pass=$3 codecs=${4:-AMR-WB/16000/1,AMR/8000/1} name=${5:-baresip-amr} cons=${6:-5555}
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$here/run"
sed "s/127.0.0.1:5555/127.0.0.1:$cons/" "$here/baresip/config" > "$here/run/config-$name"
printf '<sip:%s@%s>;auth_pass=%s;audio_codecs=%s;answermode=auto;regint=60\n' \
  "$user" "$host" "$pass" "$codecs" > "$here/run/accounts-$name"
# aufile needs a file at exactly the codec's sample rate: 16 kHz for AMR-WB, 8 kHz otherwise.
python3 -I "$here/mkwav.py" "$here/run/in16.wav" "$here/run/in8.wav"
case $codecs in AMR-WB*) wav=in16.wav ;; *) wav=in8.wav ;; esac
docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" --network host \
  -v "$here/run/config-$name:/root/.baresip/config:ro" \
  -v "$here/run/accounts-$name:/root/.baresip/accounts:ro" \
  -v "$here/run/$wav:/root/in.wav:ro" \
  baresip-amr baresip -f /root/.baresip -v
