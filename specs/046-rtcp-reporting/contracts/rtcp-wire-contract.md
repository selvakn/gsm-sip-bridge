# Contract: RTCP on the carrier media leg

What this bridge puts on the wire, what it accepts, and what it guarantees
about both. This is the external contract — the carrier IMS network is the
counterparty. Internal structure is in `data-model.md`.

Every rule below is testable without reading the implementation.

## 1. Ports

| Condition | Local RTCP port | Answer SDP |
| --- | --- | --- |
| `rtp_port + 1` bindable | `rtp_port + 1` | **Byte-identical to today.** No `a=rtcp` line. |
| Not bindable within bounded retries | any ephemeral | `a=rtcp:<port>` added to the audio section |
| No port obtainable at all | none | **Byte-identical to today.** No `a=rtcp` line. |

**Guarantees**

- **C-1.1** Wherever the answer names an RTCP port, the bridge is listening
  on that port. The declared port and the used port are never different.
  (FR-014)
- **C-1.2** `b=AS:` / `b=RS:800` / `b=RR:2400` appear on every answer,
  unconditionally — including when no RTCP port could be obtained.
  (FR-017, FR-022)
- **C-1.3** No `a=rtcp` line is emitted when no RTCP port was obtained.
  (FR-017b)
- **C-1.4** Failing to obtain an RTCP port never fails, delays, or degrades
  the call. (FR-017, SC-006)

**Remote port** — where reports are sent:

- **C-1.5** The offer's `a=rtcp` port when it names a usable one. (FR-015)
- **C-1.6** Otherwise `remote_rtp.port() + 1`. (FR-014)
- **C-1.7** An `a=rtcp` value that is malformed, zero, or otherwise
  unusable falls back to C-1.6 — never a failed call. (FR-016)
- **C-1.8** Reports are always sent to the peer IP the media is negotiated
  with, never to an address taken from `a=rtcp`.

## 2. What the bridge sends

- **C-2.1** While a call is up, the bridge sends a compound RTCP packet
  beginning with a sender report, followed by a source description
  carrying a CNAME. (FR-001)
- **C-2.2** The sender report's SSRC is the SSRC present on the RTP the
  bridge is sending to the carrier — the one the carrier can correlate
  against the media it is receiving. (FR-002, FR-002b)
- **C-2.3** That SSRC is identical across consecutive reports describing
  one uninterrupted stream. (FR-002a)
- **C-2.4** If the source being forwarded restarts (a new SSRC appears on
  the wire), the next report is sent under the new SSRC. Reports never
  continue under an SSRC no longer being transmitted. (FR-002a, US1 §4)
- **C-2.5** Sender packet and octet counts are cumulative for the call and
  never decrease — including across an SSRC change. Octet counts are
  payload bytes, excluding RTP headers (RFC 3550 §6.4.1). (FR-003)
- **C-2.6** The report carries a receiver block describing what the bridge
  observed from the carrier: fraction lost, cumulative lost, highest
  sequence received, interarrival jitter, LSR and DLSR. (FR-011)
- **C-2.7** A leg that has sent no media still reports, stating zero sent,
  rather than falling silent. Where nothing has been sent at all and no
  SSRC exists yet, a receiver report is sent in place of a sender report.
  (FR-005)
- **C-2.8** When the call ends, a BYE naming the reported SSRC is sent
  before the RTCP socket closes. (FR-018)
- **C-2.9** A failure to send anything above — including the BYE — is
  logged and never propagated. It cannot delay or block call teardown.
  (FR-019, FR-020)

## 3. Cadence

- **C-3.1** The base interval is `mean_compound_packet_bytes × 8 ÷ 800`
  seconds — the declared `b=RS:` bandwidth, so behaviour and declaration
  agree by construction. (FR-004)
- **C-3.2** Each individual interval is randomised within ±50% of that
  base (RFC 3550 §6.3.1). (FR-004a)
- **C-3.3** Over a call of two minutes or more, the mean observed interval
  is within 10% of the base, and every individual interval falls inside the
  ±50% band. (SC-002)
- **C-3.4** No member counting and no timer reconsideration occur. The
  cadence depends only on the declared bandwidth and packet size, never on
  observed participants. (FR-004b)
- **C-3.5** A call shorter than one interval ends without having sent a
  periodic report. This is correct and is not logged as a fault.

## 4. What the bridge accepts

Accepted, in order — each step's failure discards the packet:

1. Source IP equals the call's negotiated peer IP. (FR-010a)
2. The packet parses as RTCP.
3. The packet type is one the bridge consumes (SR, RR).

- **C-4.1** A packet from any other source IP is discarded before parsing.
  It never reaches the recorded figures or the metrics. (FR-010a)
- **C-4.2** Acceptance does **not** require the packet to name an SSRC
  already seen on the media stream. A report naming a newly-restarted
  source is accepted. (FR-010b)
- **C-4.3** Malformed, truncated, or unrecognised RTCP is discarded with a
  diagnostic. It never affects media, ends a call, or corrupts recorded
  figures. (FR-010)
- **C-4.4** RTCP types the bridge does not consume (APP, XR, and any
  unfamiliar member of a compound packet) are ignored without disturbing
  the call, and are never mistaken for a type it does consume. (FR-024)
- **C-4.5** Discard diagnostics are rate-limited, so a misdirected or
  hostile sender cannot produce one log line per packet.

## 5. What the bridge reports about the call

- **C-5.1** The far end's loss and jitter appear in the call's end-of-call
  reporting. (FR-006, FR-008)
- **C-5.2** Round-trip time appears where derivable from the far end's
  reports. Where it is not derivable it is absent, not zero. (FR-007)
- **C-5.3** "The far end never reported" is distinguishable from "the far
  end reported zero loss" everywhere either is presented. (FR-009)
- **C-5.4** The bridge's own observed loss, jitter and reordering appear in
  the same reporting, in the same form regardless of which relay
  implementation carried the call. (FR-011, FR-012)
- **C-5.5** Reordered arrivals are reported as reordering, never as loss.
  (FR-013)
- **C-5.6** Loss, jitter and round-trip time are recorded as metrics, under
  names and labels consistent with the existing metrics surface, with no
  label carrying a per-call, per-caller, or otherwise unbounded value.
  (FR-008a, FR-008b)
- **C-5.7** Each far-end report is observable at diagnostic verbosity as it
  arrives, not only in aggregate at the end. (FR-008c)
- **C-5.8** A call running without RTCP (tier 3) is visible as a warning
  and a metric. (FR-017a, SC-006)

## 6. What must not change

- **C-6.1** Audio relaying is byte-for-byte unchanged on the pass-through
  path and sample-for-sample unchanged on the transcoding path.
- **C-6.2** DTMF forwarding behaviour is unchanged on both paths — the
  payload-type relabelling from batch 5 (RTP-03) still applies exactly as
  before.
- **C-6.3** The both-ways media verdict is computed and reported exactly as
  today.
- **C-6.4** `rtp::SsrcTracker`'s detection and its rate-limited logging are
  unchanged. This feature consumes its signal; it does not alter what it
  detects or logs. (FR-021)
- **C-6.5** The internal veth leg to this project's own PJSIP is untouched
  in every respect, including its own answer's unbacked `b=RS:`/`b=RR:`
  declaration. (FR-023, FR-023a)
- **C-6.6** The originated-call path (`agent/origination.rs`) and the
  standalone `ims-call` diagnostic path are untouched. (FR-023)
- **C-6.7** Call teardown performs the same work it does today. No thread
  join, socket write, or wait is added to any hangup path. (FR-020, SC-007)

## 7. Out of contract

Explicitly not provided, and a peer must not expect them: RTCP extended
reports (XR), the AVPF feedback profile, SRTCP, `a=rtcp-mux`, and any
adaptation of the media in response to reported quality — no rate
adaptation, no codec renegotiation, no call teardown on reported loss.
The bridge measures and reports; it does not react. (FR-024)
