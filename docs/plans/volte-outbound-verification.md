# Plan: Close out "outbound calls via GSM" — VoLTE verification gap

**Triaged**: 2026-08-06 · **Effort**: hardware verification only, no code
expected · **Origin**: `docs/todo.md` item 2

**UPDATED 2026-09-04.** A first pass here found the legacy
`[volte].bridge_inbound=false` path (the default) has no route to outbound at
all — `orchestrate_volte::start_legacy_registration` spawns only
`volte-register`, no Agent A control listener, no Agent B/registrar. That is
real, but it turned out to be the whole story only for that one path: with
`[volte].bridge_inbound=true`, `orchestrate_volte::start_multiline` already
spawns `volte-bridge`, which reuses VoWiFi's exact `run_telephony_side`
(registrar, `run_outbound_listener`, everything) — no dispatcher needed
building. The real bug was narrower: `RuntimeLine` had no `status_port`
field, so `try_place_on_line` connected to VoWiFi's fixed port on every line,
which is wrong for VoLTE's per-line-derived ports. **Fixed** — see
`docs/todo.md` item 2's second 2026-09-04 note for the full trace and the
files touched.

**Still open**: live end-to-end confirmation. The fixed build's
`volte-bridge --modem` diagnostic path got as far as the registrar/dispatcher
genuinely attempting the VoLTE line, but IMS registration itself was blocked
by an unrelated PDN-routing issue on that test host (RA not accepted — see
`docs/operations.md`'s "attached but unusable" section) before Agent A ever
opened its status listener. Re-run once a session doesn't hit that routing
issue.

## Revised plan

1. Use `[volte].bridge_inbound=true` (not `volte-register`/legacy mode) — the
   legacy path has no outbound route by design; don't re-test it for this.
2. Fastest path to a clean signal: the single-`--modem` diagnostic invocation
   (`gsm-sip-bridge volte-bridge --modem <port> --iface <net-iface>
   --pcscf-source-path <file>`, run directly — bypasses
   `supervise`/`orchestrate_volte`/netns). Needs `[vowifi].enabled=false` in
   config (both to avoid the RFC 5626 IMPU conflict and because VoWiFi's own
   `run_telephony_side` would otherwise also try to bind `[sip_server]`'s
   port), `[outbound].enabled=true`, `[sip_server].enabled=true` with an
   account (never `1001` if a real handset might hold it), and `[sip].server`
   *removed* (mutually exclusive with `[sip_server].enabled` — FATAL at
   startup otherwise). Prime `/tmp/pcscf-<line>` first with a real VoWiFi run
   on the same SIM, same container instance (`docker compose restart`, never
   recreate, to keep the file).
3. Once Agent A completes IMS registration (watch for `registration accepted`
   and its status listener binding — `ss -tlnp` on the per-line status port,
   `LOOPBACK_STATUS_PORT + index*4`), register `siptest` (account `1002`) and
   place a real call. `require = "packets"` is enough — an answered call with
   RTP flowing both ways, the far end doesn't need to speak.
4. Confirm via `gsm_sip_bridge_outbound_attempts_total{outcome="placed"}` and
   the `siptest call` report exiting 0.
5. Revert config/image, confirm VoWiFi `Registered`/`gm_connection: up` again.
6. Only then close `docs/todo.md` item 2 and `specs/025-outbound-calling/tasks.md`
   T050e, recording the outcome the way T023/T033/T072 did.

Production-topology verification (`supervise` + real per-line netns, the
actual deployment shape) is a separate, higher-effort follow-up once the
diagnostic path above has passed — see `docs/todo.md` item 2 for why that's
lower priority (the diagnostic path already proves the fix; production-topology
netns/veth issues, if any, would be a different, unrelated bug).

## Why this is still open

`specs/025-outbound-calling` shipped outbound calling for every path (CS,
VoWiFi, VoLTE, PC/SC) from both the PBX and SIP-server-mode phones. Its own
task list explicitly declines to close this out:

> T050e Mark the outbound-calling item complete in `docs/todo.md` — **NOT
> DONE, deliberately**: CS (T023) and VoWiFi (T072) are now both
> audio-verified on real hardware, and PC/SC's carrier leg was verified up
> to a real answer (T073), but VoLTE specifically has never been
> independently exercised for *outbound* calling — it shares `ims::agent`'s
> origination code with VoWiFi (same code path, different underlying
> transport), which is good evidence but not the same as a real VoLTE
> outbound call observed end to end.

So the code is believed to work (VoLTE and VoWiFi share `originate_and_bridge`
in `gsm-sip-bridge/src/ims/agent.rs`), but nobody has placed and heard a real
VoLTE-originated call. This is a verification task, not an implementation
task.

## Plan

1. Attach a VoLTE-capable line (per `docs/ec20-volte-setup.md`) with
   `[outbound].enabled = true`.
2. Follow `specs/025-outbound-calling/quickstart.md` steps 1–3, but dial from
   a line whose transport is VoLTE rather than VoWiFi (confirm via
   `volte-status`/`vowifi-status` which line resolved to which transport).
3. Confirm two-way audio, same as T072/T073 did for VoWiFi/PC-SC.
4. Check `OUTBOUND_ATTEMPTS_TOTAL{outcome="placed"}` incremented for that
   line's label, corroborating the audio observation via metrics (same
   cross-check T050d used).
5. If it works: check off `docs/todo.md` item 2 and update T050e's note in
   `specs/025-outbound-calling/tasks.md` to record the verification (date,
   line, outcome).
6. If it doesn't: the bug is almost certainly in whatever differs between
   VoWiFi's and VoLTE's transport setup (`session.rs`/registration flow), not
   in `originate_and_bridge` itself — start there.

## Open question for you

Do you currently have a VoLTE-registered line attached and available to test
against, or does this wait until one is? If not available soon, worth
deciding whether to leave the todo item open indefinitely or downgrade the
confidence note to "believed working, unverified" and close it anyway.
