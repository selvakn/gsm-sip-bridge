# Plan: Close out "outbound calls via GSM" — VoLTE verification gap

**Triaged**: 2026-08-06 · **Effort**: hardware verification only, no code
expected · **Origin**: `docs/todo.md` item 2

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
