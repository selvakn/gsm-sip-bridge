# Quickstart: verifying RFC 4028 session-timer refresh (outbound/UAC leg)

## Unit tests

All of this feature's core logic (`agent/session_refresh.rs`'s parsing and
state machine) is pure, following `agent/ping.rs`'s existing precedent — no
live socket/session harness needed for the state machine itself:

- `SessionRefreshState::verdict` — every transition, driven by an
  explicit `now: Instant` (never a real sleep): `WaitingToSend` before/after
  `due_at`; `AwaitingResponse` before/after its timeout; `WaitingForPeer`
  before/after its `min(32s, interval/3)` deadline; `Failed` always
  `Overdue`.
- `Refresher`/`Session-Expires` parsing: `refresher=uac`, `refresher=uas`,
  no `refresher` parameter (defaults to `Uac`), no `Session-Expires` header
  at all (`None` — no obligation), and a value carrying extra/unknown
  parameters (ignored, not a parse failure).
- `on_response` ignores a response whose `CSeq` doesn't match the
  in-flight refresh (mirrors `PingState::on_response`'s existing test for
  the identical case).
- `DialogInfo::build_update_for` — a dedicated `sip_client.rs` test proves
  the built `UPDATE` carries no body (`Content-Length: 0`), a `CSeq`
  strictly higher than the dialog's prior value, and
  `Supported: timer`/`Session-Expires: <n>;refresher=uac`.
- `origination.rs`: the `200 OK` handling gains a fixture-based test per
  case — `Session-Expires` with `refresher=uac`, with `refresher=uas`, with
  no `refresher` param, and absent entirely — confirming
  `PendingOrigination`/`ActiveCall.session_refresh` ends up in the expected
  state, mirroring this file's existing `SipResponse`-fixture test style.
- `agent/mod.rs`: `handle_carrier_update` — a body-less `UPDATE` matching
  the active call's dialog while `refresher == Uas` is accepted (`200 OK`);
  the same request with a non-empty body, or naming a different Call-ID, or
  arriving while `refresher == Uac` (nothing to accept — the carrier isn't
  the one on refresh duty), all fall through to today's unchanged
  `unserved_method_response`.
- `EndedBy::SessionTimerExpired`/`reason::SESSION_TIMER_EXPIRED` — a
  confirming test alongside the existing `EndedBy`/`reason` coincidence
  tests that the two string values agree, matching the pattern already
  established for `AttachmentLost`/`ATTACHMENT_LOST` etc.

## Hardware round

Per `spec.md`'s own framing (and `docs/todo.md`'s triage): **no carrier
reachable here has ever been observed requiring `timer` on a call that
reaches `200 OK`** — Jio is the only carrier caught doing anything with
`timer` at all, and only on a `183` that always `480`s before any `200 OK`.
This feature's actual trigger condition (a `200 OK` carrying
`Session-Expires`) is therefore, like `specs/048`'s MT-06 precondition
work, not exercisable by any call this bridge can currently place —
**regression-only** on real hardware:

Rebuild and redeploy, place one ordinary outbound call over the real rig
(local Vodafone/EC200U or the remote Jio Pi) exactly as
`docs/todo.md`'s 2026-09-04 VoLTE outbound entry already establishes end to
end, and confirm no regression: `100 → 183 → 180 → 200 OK`, real
bidirectional audio, clean hangup either side. That call's `200 OK` carries
no `Session-Expires` (no carrier reachable here sends one on a connecting
call), so per FR-001/`ActiveCall.session_refresh` staying `None` it takes
the exact same path as before this feature — this round proves the
*existing* outbound-call path still works with the new code compiled in,
not that the new refresh logic itself fires live.

If a carrier is ever caught sending `Session-Expires` on a `200 OK` to an
outbound call (the trigger this feature has been waiting on since
`docs/todo.md`'s original triage), that capture becomes the first real
live confirmation and should be recorded in `docs/todo.md`/a dedicated
`docs/plans/` note, the same way `docs/plans/jio-vowifi-outbound-480-followup.md`
recorded the header-set experiment this feature's motivation cites.
