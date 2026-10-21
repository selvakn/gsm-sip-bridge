# Quickstart: verifying #139

## Automated

```bash
make format && make lint && make test
```

Covers: initiate argv contract, one-client-per-line, half-open deferral in the establish loop, two-strike wedge detection, cancel on shutdown, the real-process lifetime bound, and the source scan that rejects a stray `--initiate`.

## Container rig (the triage reproduction)

Needs docker and a published or locally built image containing charon (`/usr/libexec/ipsec/charon`). No modem, no privileges beyond NET_ADMIN.

```bash
IMG=<image with charon>
R=specs/118-bound-swanctl-initiate/rig
docker network create --subnet 172.29.139.0/24 repro139
docker run -d --rm --name repro139-sink --network repro139 --ip 172.29.139.99 alpine \
  sh -c 'nc -ul -p 500 >/dev/null & nc -ul -p 4500 >/dev/null & sleep 100000'
docker run -d --name repro139-charon --network repro139 --ip 172.29.139.10 \
  --cap-add NET_ADMIN --cap-add NET_RAW -v "$PWD/$R":/rig:ro --entrypoint sh $IMG -c 'sleep 100000'
docker exec repro139-charon sh /rig/run.sh
docker exec repro139-charon sh /rig/loop.sh ""            22   # old behaviour: freezes at tick 12
docker exec repro139-charon sh /rig/restart.sh
docker exec repro139-charon sh /rig/loop.sh "--timeout 20" 22 # bounded: clients flat, list_sas_rc=0
docker exec repro139-charon sh /rig/recover.sh 90               # still serving + negotiating: RECOVERY-READY
docker rm -f repro139-charon repro139-sink; docker network rm repro139
```

`loop.sh` prints `clients`, `charon_threads`, `list_sas_rc` (143 = probe timed out) and `log_lines` per tick. Pass criteria: `list_sas_rc=0` and `log_lines` still rising at tick 22.

## Live (one rig, one line)

1. Start the bridge with one VoWiFi line; before startup blackhole its ePDG (no-route is *not* the same: use a silent drop, see the Pi notes).
2. Leave 30 minutes. Sample `pgrep -fc 'swanctl.*--initiate'` (expect <= 1), `timeout 5 swanctl --list-sas` (expect rc 0), `stat /tmp/charon.log` (expect mtime advancing).
3. Remove the drop; the line should register without a restart.
4. Repeat with two lines, one silent: the other keeps its registration.
5. Request shutdown mid-attempt; no `swanctl --initiate` process may remain.
