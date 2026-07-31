# Research: SIP Server Mode

**Feature**: 024-sip-server-mode | **Date**: 2026-07-31

All unknowns in the plan's Technical Context are resolved below. No
`NEEDS CLARIFICATION` markers remain.

---

## R-001: Where does the registrar live — inside PJSIP, or beside it?

**Decision**: A **pure-Rust UDP registrar** in `gsm-sip-bridge/src/sip/server/`,
on its own port, built on the SIP primitives already in
`gsm-sip-bridge/src/ims/sip_client.rs`.

**Rationale**:

1. **A PJSIP module could not be tested.** `pjsip-linked` appears nowhere in
   `Makefile` or `.github/workflows/ci.yml`, and neither `Cargo.toml` declares it
   as a `default` feature. CI *does* build PJSIP 2.16 and generate real bindings
   (`ci.yml:66-85`), but `make test` (`cargo nextest run --workspace`) and
   `make lint` (`cargo clippy --workspace --all-targets`) never pass
   `--features pjsip-linked`. Only `docker/Dockerfile:75-76` compiles it, and
   that path runs no tests. Putting an authentication subsystem there would sit
   outside Constitution Principles I and II while nominally satisfying both.
2. **`unsafe` is banned in this crate.** `tools/count-unsafe.sh`, run by
   `make lint`, hard-fails when `gsm-sip-bridge/src` contains any occurrence of
   `unsafe` outside a comment. A `pjsip_module` whose `on_rx_request` is invoked
   from C would need `unsafe` plus panic-unwind discipline across the FFI
   boundary, and would need the nonce and binding tables reachable from a C
   callback as `static` globals.
3. **The primitives already exist.** `SipRequest::try_parse`
   (`sip_client.rs:160`), `build_uas_response` (`:242`, which already echoes
   every `Via` in order and applies the RFC 3261 §8.2.6.2 To-tag rule),
   `parse_digest_challenge` (`:1053`) and `random_hex` (`:19`) cover most of the
   wire handling. `ims/agent.rs:1644` already runs a UDP UAS listener in exactly
   the shape needed.

**Alternatives considered**:

| Option | Why rejected |
|---|---|
| `pjsip_module` sharing pjsua's socket | Untestable in CI; requires `unsafe` in a crate that forbids it. Genuinely nicer on the wire — kept as the escape hatch under R-002. |
| Registrar as an outbound proxy for pjsua (`pjsua_acc_config.proxy[]`) | Solves the two-port issue with no FFI, but means writing a stateless SIP proxy: Via push/pop, Record-Route, ACK and in-dialog BYE routing. Strictly more machinery and more failure modes than the registrar itself. YAGNI. |
| UDP datagram relay on 5060 fronting pjsua | Does not actually achieve one address:port — pjsua's own `Via` and `Contact` still advertise its port, so responses and in-dialog requests bypass the relay. Fake purity, real fragility. |
| `SO_REUSEPORT` on a single port | The kernel load-balances datagrams across the two sockets. Actively broken. |

---

## R-002: The registrar and the calling leg need different ports. Is that safe?

**Decision**: Accept two ports. The registrar takes the phone-default port
(5060); `[sip].local_port` stays pjsua's. When both are equal and the mode is
enabled, **fail at startup** with a message naming the remedy.

**Rationale**: The phone REGISTERs to port A but receives INVITEs sourced from
port B. On the wire this is correct per RFC 3261:

- The INVITE arrives on the phone's **own** listening port, taken from the
  `Contact` it registered. Our source port is irrelevant to delivery.
- Responses follow `Via` sent-by plus `rport`, so they return to port B.
- ACK and BYE follow our `Contact`, which advertises port B.

The one real risk is the "accept SIP only from the proxy" hardening option
present on common handsets — Yealink *Accept SIP Trust Server Only*, Grandstream
*Accept Incoming SIP from Proxy Only*, and the Fanvil equivalent. All of these
compare the source **IP address**, not the port, and the IP is identical in this
deployment. This is documented in `docs/operations.md` as the first thing to
check if a phone registers but never rings, with "turn that option off" as the
remedy and R-001's proxy option as the structural fix if it ever proves
insufficient.

NAT between phone and bridge is out of scope — the spec's stated target is a
single-site LAN.

**Alternatives considered**:

| Option | Why rejected |
|---|---|
| Silently relocate pjsua to a new fixed constant | The existing port ladder is 5070 `VETH_SIP_PORT`, 5071, 5072, 5073, then 5074+ **strided by 4 per VoLTE line** (`volte/discovery.rs:32`). A fixed constant would need collision-hunting inside a ladder that grows with line count, and would add another magic port to a list this project has already been bitten by (`vowifi/mod.rs:57-65`, `volte/bridge.rs:39-45`). |
| Let the registrar pick any free port | Phones must be told where to register; an ephemeral port cannot be provisioned into a handset. |
| Default `[sip].local_port` to 5062 when the mode is on | A default that silently changes another section's meaning is exactly the invisible behaviour strict parsing exists to eliminate. |

---

## R-003: How do all three call paths get this, without IPC?

**Decision**: The registrar is **hosted by whichever component already owns the
outbound call leg**, reusing the existing `register_trunk` arbitration at
`gsm-sip-bridge/src/sip/mod.rs:33-40`.

- Circuit-switched daemon (`SipBridge::register`) hosts it when it would have
  owned the trunk.
- The telephony agent (`vowifi::run_telephony_side`) hosts it otherwise — the
  VoWiFi and VoLTE paths.

**Rationale**: These two seams live in **different processes** —
`supervise/orchestrate.rs:243` spawns the daemon and `:457` spawns
`vowifi-sip-agent` — so they cannot share an in-memory binding table. But only
one of them is ever the PBX-facing party at a time, which is precisely what
`register_trunk` already decides. Reusing that decision means exactly one
process hosts the registrar, with no coordination needed.

Critically, `vowifi-sip-agent` is spawned with **no `ip netns exec` wrapper**
(verified at `orchestrate.rs:452-457`) — it is in the host network namespace,
which is why it can reach the PBX today and why a registrar hosted there is
reachable by phones on the LAN.

**Alternatives considered**:

| Option | Why rejected |
|---|---|
| Circuit-switched only, mutually exclusive with VoWiFi/VoLTE | VoWiFi is the more reliable carrier path in this deployment, so the mode would be unusable in the most common production configuration. Rejected once R-003 showed hosting is only a few lines per seam. |
| A fourth supervised process owning 5060, serving binding lookups over a control socket | Adds a process, a control protocol, and a failure mode, to solve a problem that the existing arbitration already answers. Out of proportion to the scope. |
| Binding lookups over IPC from a shared registrar | Same objection; also introduces latency into the call path for a `HashMap` lookup. |

---

## R-004: Authentication — what exactly, and reusing what?

**Decision**: RFC 2617 digest, **challenging every REGISTER** that arrives
without an `Authorization` header. Reuse `gsm-sip-bridge/src/ims/digest.rs`
unchanged.

**Rationale**:

- `digest::ha1(username, realm, password.as_bytes())` is byte-identical to plain
  RFC 2617 `MD5("user:realm:pass")` for an ASCII password. The function takes
  raw bytes (`digest.rs:16`) because RFC 3310 IMS-AKA needs raw `RES` octets;
  that generality makes it directly reusable here with **no change to
  `digest.rs`**.
- Both response forms already exist and are unit-tested: `response_qop`
  (`digest.rs:39`, RFC 2617 `qop=auth`) and `response_simple` (`:34`, the legacy
  RFC 2069 form). Phones in the field send both, so both are accepted.
- Challenging unconditionally makes the nonce lifecycle trivial: one nonce is
  issued, used once, and dropped. No need to track which phones are
  "already authenticated".
- `parse_digest_challenge` (`sip_client.rs:1053`) is, despite its name, a generic
  `Digest`-prefixed comma-list parser, so it parses inbound `Authorization`
  headers as well as outbound `WWW-Authenticate` ones.

**Sub-decisions**:

- **`qop`**: advertise `qop="auth"`; accept both forms back. `auth-int` is not
  offered and is rejected — REGISTER has no body, as `digest.rs:26-27` notes.
- **`algorithm`**: accept absent or `MD5`; reject `MD5-sess` and `SHA-256`.
  No handset in the target set requires them, and adding SHA-256 later is a
  self-contained change.
- **Replay**: with `qop`, require a strictly increasing nonce-count per nonce;
  without `qop`, the nonce is single-use and consumed on success. Both reject
  replay.
- **Stale**: a well-formed response against an unknown or expired nonce gets
  `401` with `stale=true`, so the handset silently retries rather than prompting
  a human for a password. A well-formed response against a *live* nonce that is
  simply wrong gets `401` **without** `stale`.
- **HA2 URI**: computed from the client's own `uri` parameter verbatim, per
  RFC 2617 — handsets disagree on whether they send `sip:realm` or the
  Request-URI. A mismatch is logged at DEBUG, not rejected.
- **Enumeration**: an unknown username and a wrong password produce
  byte-identical `401`s. Only the metric labels distinguish them (FR-009).
- **Nonce table bound**: capped (256 entries, oldest evicted) so an
  unauthenticated peer cannot grow it without limit.

**Alternatives considered**: accepting any REGISTER without a challenge was
offered to the user and declined — it would let anyone on the LAN silently take
over the line.

---

## R-005: One binding, or the RFC's contact set?

**Decision**: **One binding per address-of-record.** A second REGISTER for the
same account replaces the stored binding rather than adding to a set.

**Rationale**: RFC 3261 §10.3 models an AOR as a *set* of contacts precisely so
a proxy can fork to all of them. This bridge deliberately does not fork — it
places exactly one `Call` toward exactly one account, preserving the existing
single-`active_call` model in `SipBridge` (`sip/mod.rs:19`). Storing a set whose
extra members could never be used would be state without a consumer. Constitution
Principle V; recorded as a justification comment at the type.

**Consequence, accepted**: a user with a desk phone and a softphone on the same
account gets calls at whichever registered most recently. Documented in
`quickstart.md`; ringing several at once is listed as out of scope in the spec's
Assumptions.

---

## R-006: What else do IP phones send a registrar?

**Decision**: Answer every request with a definite response; refuse what is
unsupported rather than ignoring it.

| Method | Response | Why |
|---|---|---|
| `REGISTER` | 401 / 200 / 423 / 400 | The feature. |
| `OPTIONS` | `200 OK` + `Allow:` | Handsets use it as a keepalive. Unanswered, Yealink and Grandstream mark the server dead and **drop the binding** — the mode would work then silently stop. |
| `INVITE` | `403 Forbidden` + WARN | Phone-originated dialling is out of scope (FR-020). Silence would make the handset retransmit for 32 s and show the user a timeout rather than a refusal. |
| `SUBSCRIBE` | `489 Bad Event` | Message-waiting and busy-lamp subscriptions are not supported. |
| anything else | `405 Method Not Allowed` + `Allow:` | RFC-correct default. |

**Rationale**: This corrects an early assumption that the registrar needed only
REGISTER handling. Silently dropping datagrams causes retransmit storms and, for
`OPTIONS`, registration flapping — the failure mode most likely to make the
feature look broken in production.

---

## R-007: Which existing code must be made reachable?

**Decision**: Widen two module declarations and one method's visibility.

- `gsm-sip-bridge/src/ims/mod.rs:28` — `mod digest;` → `pub(crate) mod digest;`
- `gsm-sip-bridge/src/ims/mod.rs:37` — `mod sip_client;` → `pub(crate) mod sip_client;`
- `gsm-sip-bridge/src/ims/sip_client.rs:160` — `SipRequest::try_parse` is
  `pub(super)` → `pub(crate)`

**Rationale**: Both modules are private to `crate::ims` today, so a registrar
under `crate::sip` cannot reach them at all. Without this widening the
alternative is a second SIP parser and a second digest implementation in the
same binary — precisely the duplication Constitution Principle V targets. The
change is mechanical, has zero behavioural effect, and is isolated in its own
commit so it can be reviewed as such.

`build_uas_response` (`sip_client.rs:242`) is already `pub`, but its parameter
list cannot express `WWW-Authenticate`, `Min-Expires`, `Allow`, or a
`Contact` carrying `;expires=`. A `build_uas_response_with_headers(…, extra:
&[(&str, &str)])` variant is added and the existing function delegates to it
with an empty slice — additive, with no behaviour change on the IMS path.

---

## R-008: How does pjsua place a call without registering anywhere?

**Decision**: Add `Account::local(endpoint, id_uri, display_name)` to
`pjsua-safe/src/account.rs`.

**Rationale**: `Call::make` (`pjsua-safe/src/call.rs:98`) requires an `Account`,
and the only constructor is `Account::register` (`account.rs:29`), which always
sets `acc_cfg.reg_uri` and fires a REGISTER. Server mode needs an identity to
call *from* without registering *to* anything.

The implementation is a sibling branch inside the **existing** `unsafe` block
(`account.rs:38-78`): `pjsua_acc_config_default`, set `acc_cfg.id`, leave
`reg_uri` zeroed and `cred_count = 0`, then `pjsua_acc_add`. **No new `unsafe`
block is introduced**, so `count-unsafe.sh`'s ratio is essentially unchanged.

Not `pjsua_acc_add_local`: it derives the account identity from a transport,
whereas we want a specific `sip:{ring_aor}@{listen_addr}:{listen_port}` so the
handset sees a `From` it recognises as its own server.

The stub build returns a synthesised `Account` with `registered: false`. That
flag matters: `Drop` (`account.rs:209-215`) must not call
`pjsua_acc_set_registration(id, 0)` on an account that never registered.

---

## R-009: Testing without PJSIP, a modem, or a phone

**Decision**: Drive the real registrar over **real loopback UDP sockets** from
the test process. **Zero mocks.**

**Rationale**: The registrar is pure Rust with no PJSIP dependency, so it
compiles and runs under `make test` exactly as it does in production. The test
binds it to `127.0.0.1:0`, reads back `local_addr()`, and speaks real SIP to it
from a second `UdpSocket` with a read timeout. Precedents already in the tree:
`ims/transcode.rs:669-731` and `ims/agent.rs:1813-1931`.

This is what makes Constitution Principle I straightforwardly satisfiable: there
is no external service to stand in for, so no mock and therefore no
justification comment is needed anywhere in this feature.

Expiry and nonce lifetime take `now: Instant` as a parameter so they can be
tested at arbitrary simulated times without `sleep`, which matters under the
20 s per-test timeout in `.config/nextest.toml`.

**Remaining untestable-in-CI surface**: the `Endpoint`/`Account`/`Call` triple,
already stubbed and already the status quo. This feature adds exactly one
function there, whose stub body is a log line and an `Ok`.

---

## R-010: Should settings that do nothing in this mode be errors or warnings?

**Decision**: **Startup errors.**

**Rationale**: With the mode enabled, `[sip].server`, `[sip].username`,
`[sip].password` and `[bridge].sip_destination` have no effect whatsoever. This
project's config layer already treats "a key that silently does nothing" as the
failure mode worth being strict about — it is the stated reason
`deny_unknown_fields` was adopted (`config/raw.rs:1-23`,
`docs/migrating-config-to-strict-parsing.md`). An operator who leaves a PBX
address in place while enabling server mode has almost certainly misunderstood
the mode; telling them at startup costs one restart, whereas a warning in a log
they are not watching costs a debugging session.

The same reasoning covers `[sip].transport` being restricted to `udp` in this
version: the registrar is UDP-only, and a TCP calling leg against a UDP
registrar is a mismatch with no upside.

`[sip].port`, `[sip].display_name` and `[sip].tls_verify` remain tolerated:
`display_name` is genuinely used (it feeds the local account's `From`), and the
other two are inert but indistinguishable from their defaults.
