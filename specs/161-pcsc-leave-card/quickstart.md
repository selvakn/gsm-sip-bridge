# Quickstart: Verifying the Card-Reader Reset Fix

Needs the reader host: a PC/SC reader with a SIM, and pcscd running. Never paste a real IMSI into this repo. Scripts read it from the gitignored rig config or from the card.

## 1. Automated tests (any host)

```bash
make format && make lint && make test
```

## 2. Hardware-gated tests (reader host)

```bash
CARGO_TARGET_DIR=$PWD/target-161 cargo test -p gsm-sip-bridge pcsc -- --ignored --test-threads=1
```

Expected: every `#[ignore = "needs a PC/SC reader…"]` test passes.

- The control test proves the bug is still detectable: a bare `Card` dropped the crate's way *does* break the other handle.
- `--test-threads=1` because the tests share one physical card.
- A separate target dir avoids clobbering by other worktrees (see the memory note on the shared target dir).

## 3. Live two-process repro (SC-001)

The holder in `tmp/issue232-repro` (scratch, gitignored) connects shared, probes in a transaction, waits, then transmits.

```bash
cd tmp/issue232-repro
IMSI=$(grep -m1 '^imsi_override' ../pr228-reader/config.toml | cut -d'"' -f2)
B=../../target-161/debug/gsm-sip-bridge     # the fixed build
for i in $(seq 10); do
  ./target/debug/issue232-repro 3 & sleep 1; $B vowifi-plmn --pcsc-imsi "$IMSI" >/dev/null 2>&1; wait
done | grep -c 'transmit ok'          # expect 10   (scenario B)
for i in $(seq 10); do
  ./target/debug/issue232-repro 3 & sleep 1; $B vowifi-plmn --pcsc-imsi 001010000000001 >/dev/null 2>&1; wait
done | grep -c 'transmit ok'          # expect 10   (scenario E)
for i in $(seq 10); do
  ./target/debug/issue232-repro 3 & sleep 1; $B pcsc-list >/dev/null 2>&1; wait
done | grep -c 'transmit ok'          # expect 10   (reader listing)
```

Baseline with the `main` binary: scenarios B and E give `0`.

## 4. End-to-end registration (SC-004)

Build an image from the branch (`git archive HEAD` context, see the memory note on the docker build context). Point `tmp/pr228-reader/docker-compose.yaml` at it, then `docker compose up -d`.

Expect in `/tmp/ims-agent-0.out`:

- `connected to PC/SC reader`
- `registration through P-CSCF … outcome="registered"`, about 5 s after the agent starts
- no `smart card has been reset` anywhere

The challenged-refresh path cannot be exercised live on Vodafone, which accepts refreshes unchallenged. It is covered by step 1.
