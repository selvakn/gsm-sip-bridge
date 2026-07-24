# Contract: `volte-carrier-agent --line N`

**Feature**: `020-volte-line-netns` | **Satisfies**: FR-001–FR-004b, FR-008–FR-012

The per-line carrier-facing process, launched inside that line's network namespace. The direct
counterpart to `vowifi-ims-agent --line N` (Agent A), extracted from `volte::bridge::run_line`/
`run_line_carrier` without changing their logic (research.md R3).

## Namespace obligation

The process MUST be launched via `ip netns exec <line's netns>` and MUST NOT itself attempt to
create, join, or leave a namespace (no `setns()`, consistent with the zero-`unsafe` policy — the
namespace boundary is entirely `docker/entrypoint.sh`'s responsibility, established before this
process starts). Every socket this process opens and every `ip`/`sysctl` shell-out it makes (via
`volte::netcfg`, `volte::pdn`, `ims::sip_client`) MUST therefore be confined to that namespace as an
inherited property of the process, not as anything this process's own code checks or asserts
(FR-001/FR-002).

## Startup obligations

- MUST read this line's settings from the manifest `docker/entrypoint.sh` and `volte-bridge`'s line
  resolution already agree on (`VolteLineManifestEntry`, extended per data-model.md) — no
  independent re-discovery, matching the existing "discover once" principle (specs/013 research item
  3, reused as-is for VoLTE by specs/018).
- MUST reach the shared telephone-side half (Agent B, in the default namespace) over this line's
  veth pair, using the same control-channel protocol Agent A/Agent B already speak over loopback
  today (`crate::control::protocol`) — only the address changes, from `LOOPBACK` to this line's
  `veth_carrier_addr`/`veth_telephony_addr` (data-model.md).
- MUST NOT start answering calls before Agent B's control-channel listener for this line is up (same
  ordering `bridge.rs`'s `TELEPHONY_STARTUP_GRACE` already provides in-process — `entrypoint.sh`
  MUST preserve an equivalent ordering across processes: this line's carrier agent starts only after
  Agent B's listeners are bound).

## Fault-isolation obligations

- A failure in this line's attach, registration, or namespace/interface setup MUST end only this
  process (non-zero exit, logged with this line's `card_id`) and MUST NOT touch any other line's
  namespace, veth pair, or carrier-agent process (FR-008/FR-009). `docker/entrypoint.sh`'s existing
  per-line supervision loop (already used for VoWiFi's `vowifi-ims-agent`) restarts only this line's
  process.
- MUST NOT write to, or otherwise assume ownership of, the shared `pbx_registered` state directly —
  that remains Agent B's alone, read by this process only over the control channel, exactly as today
  (`bridge.rs`'s existing comment on why a per-line failure must not clear the *shared* flag applies
  unchanged; the mechanism just moves from an `Arc` to a wire message).

## Compatibility obligations

- Every observable behavior of a single line — registration, inbound call answer/bridge, SMS via
  both routes, attachment-loss-during-call handling, per-line status — MUST be unchanged from
  today's in-process thread (FR-005/FR-006).
- The line's own carrier-facing logic (`ims::agent::serve_inbound`, `volte::registration`,
  `volte::pdn`) MUST be reused unmodified — this contract is about where the code runs, not what it
  does.

---

# Contract: `docker/entrypoint.sh`'s per-line VoLTE setup

**Satisfies**: FR-001, FR-003, FR-004a, FR-010, FR-011, FR-012

Mirrors the existing VoWiFi per-line loop (`ensure_epdg_interface`/`start_line_tail`) for VoLTE.

## Per line, before starting that line's carrier agent

1. Idempotently ensure the line's namespace exists (`ip netns add` if absent, reuse if present —
   same check `ensure_epdg_interface` already uses for VoWiFi).
2. Idempotently move the line's LTE interface into that namespace (research.md R5): already there →
   reuse; in the default namespace → move it; neither → wait/retry, then FATAL-and-skip-this-line on
   timeout (same shape as the existing modem-port-presence check).
3. Idempotently create the line's veth pair, one end in the namespace, one in the default namespace,
   with this line's derived addresses (mirrors `start_line_tail`'s veth handling exactly).
4. Only then launch `ip netns exec <netns> gsm-sip-bridge --config <config> volte-carrier-agent
   --line <idx>`, supervised (restart-on-exit, matching every other per-line supervisor in this
   script).

## Cleanup obligation (research.md R6)

On container shutdown, for each started VoLTE line, teardown MUST run **before** the namespace is
deleted, and MUST run **inside** that namespace: `ip netns exec <netns> gsm-sip-bridge --config
<config> volte-pdn --action down ...` (or `volte-cleanup`, for the auto-discovered multi-line case),
not from the default namespace. Only after that command returns (best-effort — cleanup must not hang
container shutdown) does the namespace get deleted, via the same `STARTED_NETNS` array/loop VoWiFi's
cleanup already uses, extended to include every started VoLTE line's namespace.

## Ordering obligation

Every line's namespace/veth/carrier-agent setup (steps 1-4 above) MUST complete for every line before
the shared `volte-bridge` (Agent B only, in the default namespace) starts — mirroring the existing
VoWiFi ordering, where every line's veth pair exists before `vowifi-sip-agent` starts.
