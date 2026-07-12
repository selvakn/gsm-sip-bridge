# Phase 1 Data Model: Inbound VoWiFi-to-SIP Call Bridge

**Feature**: 011-vowifi-sip-bridge | **Date**: 2026-07-12

This feature is call-control/media-plane, not data-storage-heavy. The entities below are mostly
in-memory state within Agent A / Agent B rather than new database tables, except where noted.

## VoWiFi Line Registration

Represents Agent A's current IMS registration state (spec Key Entity: *VoWiFi Line Registration*).

| Field | Type | Notes |
|---|---|---|
| `state` | enum: `Unregistered`, `Registering`, `Registered`, `Renewing`, `Failed` | Drives whether inbound calls can be answered at all; mirrors the existing CS-side `CardState` (`modules/card.rs`) enum-per-state pattern. |
| `impi` / `impu` | string | Private/public identity, derived at startup from IMSI (+ optional MSISDN override), same as today's `ims::mod::register_session` locals. |
| `registered_at` | timestamp | Set on the last successful `200 OK`. |
| `expires_at` | timestamp | `registered_at + Expires` (default 3600s); renewal is scheduled ahead of this per FR-001/FR-007. |
| `last_failure` | `Option<{ at: timestamp, reason: string }>` | Surfaced via status tooling (FR-008, User Story 3). |
| `gm_sa_installed` | bool | Whether kernel XFRM SAs are currently live for this registration (reused from `gm_ipsec::GmEndpoints` tracking, kept alive instead of torn down per-call as today). |

**State transitions**: `Unregistered → Registering → Registered`. `Registered → Renewing →
Registered` on successful renewal before `expires_at`. Any step can fall to `Failed` (network
error, AKA failure, non-200 final response), from which retry/backoff returns to `Registering`.
`Failed` does not silently loop forever without being observable — `last_failure` is always
populated so User Story 3 can report it.

## Bridged Call

Represents one inbound VoWiFi call and its paired SIP/PBX leg (spec Key Entity: *Bridged Call*).
Lives only for the call's duration; not persisted across restarts (no requirement calls for call
history durability beyond "recent activity," FR-008).

| Field | Type | Notes |
|---|---|---|
| `call_id` | string | The IMS-side SIP `Call-ID`, used to correlate the two legs and log lines. |
| `caller` | string | Calling party identity from the inbound INVITE's `From`/`P-Asserted-Identity`, forwarded to the PBX leg per FR-011. |
| `state` | enum: `Ringing`, `Answering`, `Bridged`, `Declining`, `Ended` | Mirrors the existing CS-side `CardState` shape for consistency. |
| `outcome` | enum: `Answered`, `Declined { reason }`, `Failed { reason }` | Set on `Ended`; reason values include `busy` (FR-009), `sip_unreachable` (FR-010), or a transport/protocol error. |
| `started_at` / `ended_at` | timestamp | For duration reporting (User Story 3, Acceptance Scenario 2). |
| `sip_destination` | string | Which PBX destination the call was routed to (FR-003 — reuses the existing GSM-bridge routing config). |
| `codec` | enum: `Pcmu`, `AmrWb` | Negotiated codec for the IMS leg (research item 3). |

**State transitions**: `Ringing → Answering → Bridged → Ended` (happy path). `Ringing →
Declining → Ended` when FR-009/FR-010 apply (busy or SIP-side unreachable — declined before ever
reaching `Answering`). `Bridged → Ended` on a `BYE` from either leg (FR-005).

**Concurrency constraint**: at most one `Bridged Call` (or one in `Ringing`/`Answering`) exists at a
time per the single-line assumption (spec Assumptions); a second inbound `INVITE` while one is
already active goes straight to `Declining` (FR-009).

## SIP/PBX Destination

Not a new entity — this is the existing call-routing configuration already used by the
circuit-switched bridge (`[bridge] sip_destination` in `config.toml`, resolved by
`compute_destination_uri`, `gsm-sip-bridge/src/sip/mod.rs:141-149`). Reused as-is per FR-003 and
the spec's Assumptions ("SIP/PBX-side call routing ... reuses the same configuration"); no new
schema.

## Agent A ↔ Agent B Control Message

Not a spec-level entity, but the mechanism that carries "a `Bridged Call` just started/ended"
information across the process boundary (the veth link). Defined fully in
`contracts/agent-control-protocol.md`; summarized here for completeness:

| Field | Type | Notes |
|---|---|---|
| `event` | enum: `IncomingCall`, `Answered`, `Declined`, `Hangup` | Lifecycle events for one `Bridged Call`. |
| `call_id` | string | Correlates to the `Bridged Call.call_id` above. |
| `caller` | string | Present on `IncomingCall`. |
| `reason` | string | Present on `Declined`/`Hangup` when non-normal. |

## No new persistent storage

Unlike the existing `card_slots`/`card_mode_prefs` tables added by feature 009, this feature
introduces no new SQLite schema: VoWiFi Line Registration and Bridged Call are both live,
in-process state, and FR-008's "recent call outcomes" requirement is satisfied by structured
`tracing` log events plus in-memory recent-history (bounded ring buffer) exposed through status
tooling — consistent with the spec's Assumption that this reuses whatever status mechanism already
exists rather than introducing new durable storage.
