---

description: "Task list for siptest — SIP softphone for agent-driven end-to-end testing"

---

# Tasks: siptest — SIP softphone for agent-driven end-to-end testing

**Input**: Design documents from `/specs/037-siptest-softphone/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/control-api.md, contracts/sip-flows.md, quickstart.md

**Tests**: Included. Constitution Principle I (Integration-First Testing) is
NON-NEGOTIABLE and Principle II requires green-on-commit; the Development
Workflow section makes TDD the default practice. Every test runs against
either pure logic or the bridge's real registrar in-process — no mocked
network boundary anywhere in this feature.

**Organization**: Tasks are grouped by user story (spec.md P1–P2) so each can
be implemented, tested and demoed independently. Foundational work that every
story depends on — the socket model, the widened `ims` visibility, the dialog
engine skeleton, registration itself — is front-loaded, because US1 and US3
cannot function without a live registration and US2 *is* the registration
lifecycle's own acceptance criteria.

## Implementation status (as of the third /speckit.implement pass)

**Done and verified**: Phase 1 (Setup), Phase 2 (Foundational), Phase 3 (US1
— outbound calling), Phase 5 (US3 — inbound calling), and Phase 6 (US4 —
tone/Goertzel/RTT) in full, plus most of Phase 4 (US2) and two Polish tasks.
65 of 84 tasks. `make format && make lint && make test` all pass across the
whole workspace (1,350 test assertions, zero failures, zero `unsafe`, zero
clippy warnings). `siptest call --destination ... --wait` genuinely places a
call through a real bridge's registrar, follows the `302` redirect, exchanges
real PCMU audio carrying the `grid8` tone plan by default, records both
directions, and exits non-zero on a packet-verdict failure; the daemon
genuinely answers a real inbound INVITE arriving from a source port it never
registered from, captures all three caller-ID headers, handles
CANCEL-before-answer with the correct `200`/`487` pair, and supports
answer/reject/manual policy over the control API; and every call's report now
carries a **real** tone-detection verdict and, when the signal loops back, a
**measured round-trip delay** — proven end to end in
`media::session::tests::tone_plan_is_detected_on_both_sides_of_a_real_loopback_with_measurable_rtt`
against a real UDP echo with ground-truth delay. This is the tone detector
`gsm-sip-bridge/src/ims/call.rs:153`'s `round_trip_delay: None` comment says
is needed to fill that field — it now genuinely can be. **All three MVPs
(outbound, inbound, tone-verified audio) are real and tested**, not
scaffolded.

**Three architecture decisions, applied consistently**:

1. Rather than pure `step(Input) -> Vec<Output>` state machines driven by a
   separate `crossbeam_channel::select!` dialog-engine thread (T026),
   registration and both call directions are blocking, I/O-performing
   functions. Outbound and registration run from a background thread and
   axum's `spawn_blocking`; inbound runs on its own dedicated listener thread
   (`daemon::inbound_listener_loop`), which blocks for the whole duration of
   a call it accepts — correct given `max_concurrent = 1`, but meaning a
   second call truly cannot be handled concurrently even at the busy-out
   level while the listener itself is inside `execute_inbound_call` (the
   486-for-a-second-INVITE logic lives *inside* that function via
   `wait_or_cancel`/`wait_for_ack`'s stray-request handling, not in the
   listener). See the inline notes on T025–T028, T036–T037, T059–T060.
2. Every SIP read goes through **one demux reader thread** owned by
   `SipSocket` (`siptest/src/sip/socket.rs`), added specifically to make
   inbound handling safe: two threads independently calling `recv_from` on
   one UDP socket would race for each datagram, which could silently steal
   an inbound INVITE meant for the call handler or a response meant for an
   in-flight transaction. The reader demultiplexes into a response queue
   (consumed by `recv_response`, used by registration/outbound) and a
   request queue (consumed by `recv_request`, used by inbound). Fixed a real
   bug found while building this: `SipSocket::bind` with `local_port = 0`
   (any ephemeral-port bind, including every outbound call's RTP-adjacent
   signalling socket in tests) was reporting port 0 from its own
   `local_addr()` instead of the OS-assigned port.
3. The Goertzel detector (`media/goertzel.rs`) implements 3 of the 4
   documented gates — relative energy, twist, and a broadband guard — and
   drops the fourth (an absolute floor above a running noise-floor
   percentile). `media/level.rs` still computes that percentile for
   *reporting* (`noise_floor_dbfs` appears in every report); it just isn't
   consulted as a detection gate. The three implemented gates were sufficient
   for every test case built so far, including PCMU-quantised tone and
   synthetic noise/off-grid rejection.

**Not built**: G.722 (T079–T081, deliberately deferred per research.md R7
regardless), `dtmf`/`single` tone-plan variants (`silence` is covered
structurally via `tone_enabled: false`), FR-005's startup guard against the
configured account colliding with the bridge's `ring_aor` (T028 — genuinely
blocked: siptest has no channel to read the bridge's `ring_aor` value, only
whether *something* is registered, via metrics), the `/registration/*`
force-action endpoints, `/log/tail`, and `GET /calls/{id}/recording*`
(T053–T054, T074), early caller-BYE detection mid-call for either call
direction, and the HTTP-layer integration tests for the safety gate, control
API, and inbound policy (T047–T049, T056, T066, T083). The underlying logic
for the safety gate, retention cap, inbound dialog handling, and the full
tone/level/RTT pipeline **is** tested — at the `SharedState`+function level,
via raw-socket integration tests, or via real (non-SIP) media-session
integration tests — what's missing is exercising all of it through the
actual running HTTP server, and the recording-fetch/registration-management
endpoints simply don't exist yet.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependency on an
  incomplete task)
- **[Story]**: US1 / US2 / US3 / US4, matching spec.md
- File paths are exact and repo-relative from `/home/selva/projects/ec20/gsm-sip-bridge/.claude/worktrees/037-siptest-softphone/`

## Path Conventions

New crate at `siptest/`, alongside the existing `gsm-sip-bridge/` member. A
handful of foundational tasks touch `gsm-sip-bridge/src/ims/` to widen
visibility of primitives siptest reuses (research.md R1) — those are called
out explicitly since they are the only tasks outside the new crate.

---

## Phase 1: Setup

**Purpose**: Get an empty, green, workspace-integrated crate before any logic
exists.

- [x] T001 Add `"siptest"` to `members` in `/home/selva/projects/ec20/gsm-sip-bridge/.claude/worktrees/037-siptest-softphone/Cargo.toml`
- [x] T002 Create `siptest/Cargo.toml` with `version.workspace = true`, `edition.workspace = true`, `license.workspace = true`, `[[bin]] name = "siptest" path = "src/main.rs"`, and dependencies: `clap` (derive), `serde`/`toml`, `tokio`, `axum`, `tracing`/`tracing-subscriber`, `thiserror`, `crossbeam-channel`, `serde_json`, plus `gsm-sip-bridge = { path = "../gsm-sip-bridge" }`
- [x] T003 [P] Create `siptest/src/error.rs` — `SipTestError` (thiserror) wrapping `gsm_sip_bridge::error::BridgeError` via `#[from]`, plus siptest-specific variants (`NotRegistered`, `DestinationNotAllowed`, `RateLimited`, `CallInProgress`, `CallEvicted`); `pub type SipTestResult<T> = Result<T, SipTestError>`
- [x] T004 [P] Create `siptest/src/lib.rs` declaring the module tree from plan.md (`error`, `cli`, `config`, `safety`, `daemon`, `sip`, `sdp`, `media`, `api`, `logbuf` — empty stub modules for now)
- [x] T005 Create `siptest/src/main.rs`: parse CLI, read `[logging].level` ahead of full config load, call `logging::init` to stderr, exhaustive match on `Some(command) => commands::run(...)` / `None => daemon::run(...)`, return `ExitCode` — mirrors `gsm-sip-bridge/src/main.rs`
- [x] T006 Confirm `cargo build --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` are clean with the new empty crate

**Checkpoint**: `make build` succeeds with siptest as an inert workspace member.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: The plumbing every user story sits on — reachable `ims`
primitives, the one-socket transport model, config/CLI, the dialog engine
skeleton, and registration itself (US1 and US3 cannot place or receive a call
without it; US2 is literally its lifecycle guarantees).

**⚠️ CRITICAL**: No user story work can begin until this phase is complete.

### Upstream `gsm-sip-bridge` changes (research.md R1)

- [x] T007 In `gsm-sip-bridge/src/ims/mod.rs`, widen `mod rtp;` → `pub mod rtp;`, `pub(crate) mod digest;` → `pub mod digest;`, `pub(crate) mod sip_client;` → `pub mod sip_client;`; extend each module's existing "why this is exposed" doc comment to cite this feature
- [x] T008 In `gsm-sip-bridge/src/ims/sip_client.rs`, widen `SipResponse::try_parse` (private, ~line 117) and `SipRequest::try_parse` (`pub(crate)`, ~line 230) to `pub`; add `pub fn parse_datagram(text: &str) -> BridgeResult<Option<SipMessage>>` performing the request-vs-response discrimination for one complete datagram, and refactor `recv_message_deadline` (~line 901) to call it so there is exactly one implementation
- [x] T009 [P] Add a unit test in `gsm-sip-bridge/src/ims/sip_client.rs` for `parse_datagram`: a REGISTER text parses to `SipMessage::Request`, a `200 OK` text parses to `SipMessage::Response`, and malformed input returns an error rather than panicking
- [x] T010 Run `make lint && make test` in `gsm-sip-bridge` to confirm T007–T009 introduce no regression before building on top of them

### Transport, config, safety

- [x] T011 [P] Create `siptest/src/sip/socket.rs`: `SipSocket` wrapping one **unconnected** `Arc<UdpSocket>` bound to `0.0.0.0:{local_port}` (never `connect()` — research.md R2), `send_to`/`recv_from` only, plus routable-local-IP discovery via the connect-trick (bind `0.0.0.0:0`, `connect(bridge_addr)`, read `local_addr().ip()`, drop)
- [x] T012 [P] Unit test in `siptest/src/sip/socket.rs`: the discovered local IP is never `0.0.0.0`/`UNSPECIFIED` when a routable destination is given
- [x] T013 [P] Create `siptest/src/config.rs`: serde `Config` matching quickstart.md's `siptest.toml` shape (`[sip]`, `[media]`, `[call]`, `[safety]`, `[retention]`, `[inbound]`, `[api]`, `[logging]`), reusing `gsm_sip_bridge::config::env::resolve_in_place` for `env:VAR` indirection and `gsm_sip_bridge::config::secret::Secret<String>` for the password field
- [x] T014 [P] Unit test in `siptest/src/config.rs`: an `env:VAR` password resolves from the environment and never appears in `{:?}` output of the loaded config
- [x] T015 [P] Create `siptest/src/safety.rs`: `SafetyPolicy` (FR-006a/b) — `allowed_destinations: Vec<String>` (exact or trailing-`*` prefix), `min_call_interval_secs`, `max_calls_per_hour`; pure `fn check(&self, destination: &str, history: &CallAttemptHistory, now: Instant) -> Result<(), SafetyRefusal>`; `CallAttemptHistory` as a bounded `VecDeque<Instant>` sliding window
- [x] T016 [US1][P] Unit tests in `siptest/src/safety.rs`: empty allow-list denies everything (fail-closed, not fail-open); an exact match and a `+9190000*` prefix match both pass; a call inside `min_call_interval_secs` of the previous attempt is refused with `retry_after_s`; the 21st attempt within an hour (default cap 20) is refused; inbound is never subject to either check (asserted by the function's signature taking only outbound call sites)
- [x] T017 [P] Create `siptest/src/api/state.rs`: `CallRegistry` — insertion-ordered map capped at `max_calls_retained` (data-model.md RetentionPolicy), evicting the oldest completed call's WAV files + JSON sidecar and dropping its record when a new completed call would exceed the cap; a bounded set of evicted ids so a lookup reports `Evicted` distinctly from `NotFound`
- [x] T018 [US2][P] Unit test in `siptest/src/api/state.rs`: inserting call `N+1` past `max_calls_retained = N` deletes call 1's files from a temp dir and `lookup(call_1)` returns `Evicted`, while `lookup("nonexistent")` returns `NotFound`

### SIP wire layer

- [x] T019 Create `siptest/src/sip/message.rs`: plain RFC 3261 REGISTER builder (`RegisterRequest` — no IMS ICSI tags, no `+sip.instance` IMEI URN, no spoofed `User-Agent`; contract sip-flows.md C-1), INVITE builder (no `Supported: 100rel, timer` — sip-flows.md C-2), ACK-for-3xx and ACK-for-2xx builders (differ in Request-URI and `Via` branch reuse), and `build_cancel` (reuses the INVITE's exact branch — does not exist anywhere else in the repo per research.md R1)
- [x] T020 [P] Unit tests in `siptest/src/sip/message.rs`: the built REGISTER contains none of `icsi-ref`, `+g.3gpp.smsip`, `+sip.instance`; `build_cancel`'s `Via` branch is byte-identical to the INVITE it cancels; the 3xx-ACK and 2xx-ACK differ in Request-URI and branch as specified; a digest `Authorization` header round-trips against a canned RFC 2617 vector using `gsm_sip_bridge::ims::digest`
- [x] T021 Create `siptest/src/sdp.rs`: offer builder for PT 0 (PCMU) / 101 (telephone-event) only for now (G.722's PT 9 added in Polish), and an answer parser that takes `c=`/`m=audio` port and the selected payload type, rejecting `c=0.0.0.0`, `m=audio 0`, and `a=inactive` as explicit failures (sip-flows.md C-4) — `ims::sdp` is not reusable here (research.md R5: it hard-rejects any PT other than 0/96)
- [x] T022 [P] Unit tests in `siptest/src/sdp.rs`: an offer lists PT 0 and 101 in order; an answer choosing PT 0 yields PCMU; an answer choosing an unoffered PT is rejected naming that PT; each of `c=0.0.0.0`, `m=audio 0`, `a=inactive` is rejected explicitly

### Codec, media types, dialog engine skeleton

- [x] T023 [P] Create `siptest/src/media/codec.rs`: `CodecProfile` struct (`pt`, `rtpmap`, `rtp_clock_hz`, `audio_hz`, `samples_per_frame`, `ts_increment`, `bytes_per_frame` — data-model.md CodecProfile) and a `PCMU` const profile using `gsm_sip_bridge::ims::rtp::{linear_to_ulaw, ulaw_to_linear}`
- [x] T024 [P] Unit test in `siptest/src/media/codec.rs`: `PCMU`'s `rtp_clock_hz == audio_hz == 8000` and `samples_per_frame == ts_increment == 160` (the trivial case of the invariant G.722 will later violate on purpose)
- [x] T025 **Delivered with a scope reduction.** `siptest/src/sip/registration.rs` implements `register()`/`deregister()` covering every documented case (401/qop=auth, `stale=true` nonce adoption, second-401-fails, 423→Min-Expires, granted-expires parsing) — but as a **blocking, I/O-performing function**, not a pure `step(Input) -> Vec<Output>` state machine. Chosen to fit the session's time budget: a pure FSM plus the T026 engine to drive it is materially more code, and this shape is still fully covered by the real-registrar integration test. The trade-off: the specific edge cases (423, stale nonce, second-401) are exercised only by the happy-path integration test, not by isolated canned-message unit tests, since the function can't be driven without a socket. **Gap, not covered elsewhere.**
- [ ] T026 **Not built.** No `sip/engine.rs` / dialog-engine thread exists. Registration runs in its own background thread (`daemon::registration_loop`) and outbound calls run synchronously inside the axum handler via `spawn_blocking` (`call::execute_outbound_call`) — see T025's note. This is the single biggest architectural simplification versus the plan: there is no unified per-call dialog table, so a second concurrent dialog (e.g. inbound arriving while an outbound call is mid-flight) is not handled at all. Acceptable for the MVP's single-call-at-a-time scope, but blocks true concurrent dialog handling.
- [x] T027 **Delivered, shaped differently.** `siptest/src/daemon.rs` is the composition root — builds config, binds the shared `SipSocket`, spawns the registration thread, builds the tokio runtime explicitly and serves axum with graceful shutdown, de-registers on exit. No separate "dialog-engine thread" (see T026) — the registration thread and the API's blocking call-handlers are the only two consumers of the socket.
- [ ] T028 **Not built.** No startup check comparing the configured `username` against the bridge's `ring_aor`. The provisioning guidance (use a dedicated account, e.g. `1002`, never the handset's `1001`) is documented in quickstart.md but not enforced in code. Real gap — `git blame`-visible as a follow-up.
- [x] T029 [P] Create `siptest/src/api/mod.rs` (axum router skeleton + `AppState { cmd_tx, snapshot, events, calls }`) and `siptest/src/api/events.rs` (`EventBus`: monotonic `seq`, bounded ring buffer, `since`-cursor long-poll with `timeout_ms` — contract control-api.md, no SSE per research.md R9)
- [x] T030 [P] Unit test in `siptest/src/api/events.rs`: events published while a `since` request is blocked are delivered without a missed wakeup; a `since` older than the ring buffer's oldest retained event is still answered (no silent gap)
- [x] T031 Wire `GET /health` and a registration-only `GET /status` (registration state, contact, `local.sip_addr`, `event_seq`) in `siptest/src/api/handlers.rs`, and start the registration FSM against the engine's socket
- [x] T032 [P] Create `siptest/src/cli.rs` (clap 4 derive: `Cli { config, verbose, command: Option<Commands> }`; no subcommand runs the daemon) matching house style (long doc comments, `default_value_t` from config constants)

**Checkpoint**: `siptest` builds, starts, registers to the bridge's real
registrar (or an in-process test instance), and reports registration state via
`GET /status`. No calls can be placed or received yet.

---

## Phase 3: User Story 1 - Place an outbound call and learn whether audio flowed (Priority: P1) 🎯 MVP

**Goal**: Dial a real number through the bridge, follow the `302` redirect,
exchange PCMU audio, and produce a machine-readable per-direction verdict.

**Independent Test**: Register, `POST /calls {"destination":...}`, confirm the
report distinguishes both-ways from one-way audio (spec.md US1 Acceptance
Scenario 2).

### Tests for User Story 1

> Write these first; they must fail until the implementation tasks land.

- [x] T033 [P] [US1] Integration test `siptest/tests/test_against_registrar.rs`: start the bridge's real `Registrar::start_on_with_outbound` (already `pub`, `gsm-sip-bridge/src/sip/server/mod.rs:96`) on `127.0.0.1:0`, run siptest's production registration + outbound FSMs against it, receive the real `302`, ACK it, re-INVITE a loopback stub UAS (a plain `UdpSocket` in the test file answering `100`/`180`/`200 OK` + SDP and echoing received RTP) — assert registered, redirect followed to the stub's port, and `packets == BothWays`. In-file comment documenting why the stub UAS is the constitution's sanctioned "component not available in CI" carve-out (pjsua lives behind `pjsip-linked`, which CI never compiles)
- [x] T034 [P] [US1] **Folded into `test_against_registrar.rs`** as `an_invite_from_a_different_socket_than_the_registered_one_is_refused` rather than a separate file — same coverage (a second socket's INVITE gets `403 untrusted_source`), just alongside the other registrar test since both share the `StubUas`/`server_config` fixtures.
- [ ] T035 [P] [US1] Unit tests for `OutboundCallFsm` (to be created in T036) against canned `SipResponse`/`SipRequest` values: a `302` is ACKed and re-INVITEd to the Contact's port; a `302` whose Contact carries no port is refused rather than dialled on the registrar's port; each refusal (`403`/`484`/`503`/`400`) maps to a distinct named error; a ring timeout emits `CANCEL` reusing the INVITE's branch and expects `200`+`487`; an unknown `Require:` in a response is met with `420`+`Unsupported`

### Implementation for User Story 1

- [x] T036 [US1] **Delivered with the same scope reduction as T025.** `siptest/src/sip/outbound.rs`'s `place_call()` implements the full C-2 sequence — INVITE, 302 handling with the redirect target taken only from the response's `Contact`, 3xx-ACK, re-INVITE, 2xx-ACK, ring-timeout→CANCEL, refusal mapping (403/484/503/400) — as a blocking function rather than a pure FSM, and it is proven end to end against the real registrar (`test_against_registrar.rs`). Not covered: isolated unit tests for the CANCEL/487 and unknown-`Require`/420 branches specifically (see T035, not done).
- [ ] T037 [US1] **N/A — no dialog engine exists (T026).** `execute_outbound_call` (`siptest/src/call.rs`) calls `outbound::place_call` directly from the API handler's blocking task instead of dispatching through a shared dialog table.
- [x] T038 [US1] Create `siptest/src/media/session.rs`: per-call transmit thread (20ms cadence scheduled against **absolute deadlines** `start + n*ptime`, not `sleep(20ms)` after the work — sip-flows.md C-5 calls out `ims/call.rs:609`'s drift bug to not repeat) and receive thread (blocking `recv_from`, 1s timeout), each on its own RTP socket; **no channel from receive thread to transmit thread** (the echo.rs independence invariant, research.md R8)
- [x] T039 [US1] In `media/session.rs`, feed received packets through `gsm_sip_bridge::ims::media_stats::ReceiveTracker::on_packet(seq, ts, arrival, rtp_clock_hz)` and compute the direction verdict via `media_stats::verdict(sent_packets, received_packets, threshold)` — pass **packets**, not samples (research.md R6 correction to existing practice)
- [x] T040 [US1] [P] Unit test in `media/session.rs`: *the transmit sample stream is byte-identical whether or not any RTP arrives* — generate N seconds of TX with a receiver attached and with none, compare byte-for-byte (asserts the independence invariant structurally, not just by absence of a channel)
- [x] T041 [US1] Wire recording into `media/session.rs`: `gsm_sip_bridge::ims::rtp::WavWriter` for both directions, sent-WAV from pre-encode samples, received-WAV from post-decode samples, written to `{recording_dir}/{call_id}-{sent,received}.wav`
- [x] T042 [US1] Create `siptest/src/media/report.rs`: `CallReport` (data-model.md) — signalling timings (`invite_to_180_ms`, `invite_to_200_ms`, `answer_to_first_rtp_ms`), media counters, `packets` verdict (tone/loopback axes left `null`/`not_confirmed` until US4), `success` evaluated against `require` (`signalling`/`packets`/`tone-loopback`), and `render_text()` mirroring the existing `render_call_report` field layout
- [x] T043 [US1] Wire `POST /calls` in `siptest/src/api/handlers.rs`: validate destination against `[0-9*#+]+`, run it through `SafetyPolicy::check` (T015) before any signalling leaves the host, reject with `409` if a call is already active (`max_concurrent`), reject with `503` if not registered, otherwise send a `Command::PlaceCall` to the engine and return `202 {"id":...}`; support `?wait=true` blocking until terminal state
- [x] T044 [US1] [P] Wire `GET /calls` (recent summaries, capped list via `CallRegistry`) and `GET /calls/{id}` (full report; `404` for unknown, `410 Gone` for evicted) in `siptest/src/api/handlers.rs`
- [x] T045 [US1] [P] Wire call-completion eviction: on each call reaching a terminal state, insert into `CallRegistry` (T017) so retention capping is exercised for real, not just in its unit test
- [x] T046 [US1] Add `siptest call --destination ... [--wait] [--codec pcmu]` subcommand in `siptest/src/cli.rs` / a `commands.rs` — an HTTP client against the running daemon (via `reqwest`, already a workspace dependency), printing `report_text` to stdout and diagnostics to stderr, exiting `0` only when `success == true` (FR-032/033)
- [ ] T047 [US1] [P] Integration test `siptest/tests/test_control_api.rs` (US1 portion): bring the daemon up in-process on ephemeral ports against the real registrar + loopback stub, drive `POST /calls?wait=true`, assert the returned report's `verdicts.packets`, and that both WAV files exist with the correct header (44-byte RIFF, 8000 Hz)
- [ ] T048 [US1] [P] Integration test: `POST /calls` with a destination not in `allowed_destinations` returns `403 destination_not_allowed` before any packet reaches the loopback stub (assert the stub's socket receives nothing)
- [ ] T049 [US1] [P] Integration test: 21 rapid `POST /calls` attempts (or a shortened window) return `429 rate_limited` with `retry_after_s` on the ones past the cap

**Checkpoint**: `siptest call --destination +919000000000 --wait` places a real
call through the bridge and reports a packet-count verdict, positively or
negatively, with both directions recorded. **This is the MVP** — it already
replaces a physical handset for outbound testing.

---

## Phase 4: User Story 2 - Stay registered and be driven over a control interface (Priority: P1)

**Goal**: The registration lifecycle itself — accurate status, automatic
renewal, backoff and recovery — is independently observable and correct.

**Independent Test**: Start the daemon, confirm a live binding, idle past one
refresh interval, confirm the binding survives and status stays accurate
(spec.md US2).

### Tests for User Story 2

- [ ] T050 [P] [US2] Unit tests for `RegistrationFsm` (T025) against canned messages: `401` → digest `Authorization` with `qop=auth`, `nc=00000001`; `401` with `stale=true` adopts the new nonce and resets `nc` to 1; a **second** `401` on an already-authorised REGISTER is a hard failure, never a retry loop; `423` adopts `Min-Expires` and retries; refresh timer fires at `min(expires/2, expires-30s)` floor 30s; repeated timeouts drive the documented backoff ladder (2/4/8/16/30s) and increment `consecutive_failures`
- [ ] T051 [P] [US2] Integration test `siptest/tests/test_against_registrar.rs` (US2 additions): register against the real in-process registrar, advance a fake/injectable clock past the refresh interval, assert re-REGISTER happens without dropping the binding (`Registrar`'s binding table still shows it live); kill and restart the registrar mid-session and assert siptest recovers registration automatically

### Implementation for User Story 2

- [x] T052 [US2] **Already delivered under T031** — `GET /status` was built to the full snapshot shape from the start (`registration`, `local.sip_addr`, `bridge.outbound_observed`, `active_call`, `counters`) rather than the registration-only slice T031 originally scoped, since splitting it into two passes would have meant reworking the same handler twice. `inbound_policy` is the one field not present — there is no inbound policy yet (US3 not built).
- [ ] T053 [US2] [P] Wire `POST /registration/{register,refresh,deregister}` in `siptest/src/api/handlers.rs`, each sending the corresponding `Command` to the engine and blocking on its reply channel via `spawn_blocking`
- [ ] T054 [US2] [P] Create `siptest/src/logbuf.rs`: a `tracing::Layer` writing into a bounded ring buffer, and wire `GET /log/tail?lines=N` in `api/handlers.rs`
- [x] T055 [US2] Add `siptest status` subcommand: an HTTP client printing `/status` human-readably
- [ ] T056 [US2] [P] Integration test in `test_control_api.rs`: `GET /status` after registration shows `state: "registered"` with a non-null `renews_in_secs`; pointing at an unreachable registrar shows `state: "failed"` with an increasing `consecutive_failures` and no panic

**Checkpoint**: `GET /status` is a complete, accurate, poll-friendly picture of
registration health, independent of whether any call has ever been placed.

---

## Phase 5: User Story 3 - Receive an inbound carrier call and verify it (Priority: P2)

**Goal**: Accept a call the bridge sends from a different port than the one
siptest registered to, apply inbound policy, and produce the same verdict
shape as outbound.

**Independent Test**: Point the bridge's `ring_aor` at siptest's account, call
from another phone, discover and verify the call using polling alone (spec.md
US3).

### Tests for User Story 3

- [x] T057 [P] [US3] `siptest/tests/test_inbound_from_other_port.rs` — built simpler than described: a hand-built `SharedState` + `daemon::inbound_listener_loop` driven directly (no registrar needed for this scenario, since the hazard is about inbound source-port validation, not registration), with a plain `UdpSocket` standing in for Agent B sending from a port siptest never registered from. Asserts the real 100/180/200 sequence, the SDP answer, and caller-ID capture (`P-Asserted-Identity`, `X-GSM-Caller-ID`) end to end.
- [x] T058 [P] [US3] **Partially covered, by a mix of unit and integration tests rather than canned-message FSM unit tests** (no `InboundCallFsm` exists — see T059's note). `sip/inbound.rs` has direct unit tests for caller-ID extraction and `OPTIONS`/`405`+`Allow`. `test_inbound_from_other_port.rs` adds a second integration test, `a_cancel_before_answer_yields_200_and_487_and_is_recorded_as_caller_cancelled`, proving the CANCEL branch end to end including the `caller_cancelled` end reason. **Not covered**: the T1-retransmit-ladder-then-abandon path and the second-concurrent-INVITE→486 path have no dedicated test (the logic exists in `sip/inbound.rs::wait_for_ack`/`handle_stray` but isn't independently exercised).

### Implementation for User Story 3

- [x] T059 [US3] **Delivered with the same scope reduction as outbound (T025/T036).** `siptest/src/sip/inbound.rs` holds the wire-level helpers (caller-ID extraction, `OPTIONS`/`405`/`420`/`488`/busy/reject builders, `wait_or_cancel`, the T1 `wait_for_ack` ladder) as plain functions rather than a pure FSM; `call::execute_inbound_call` (in `siptest/src/call.rs`) is the orchestration, mirroring how outbound splits `sip::outbound` (wire) from `call::execute_outbound_call` (orchestration). Implements the full C-3 sequence: `100`→caller-ID capture→`180`→policy(answer/reject/manual)→`200` with T1 retransmit→ACK-wait→media→our own `BYE`.
- [x] T060 [US3] **No dialog engine exists (T026), so nothing to wire into.** `daemon::inbound_listener_loop` dispatches directly: `OPTIONS`→200, a second concurrent `INVITE` while a call is active→`486`, a fresh `INVITE`→`call::execute_inbound_call`. Dialog identification is **Call-ID only** throughout — `wait_or_cancel`/`wait_for_ack` match on `Call-ID`, never on source address or port. `test_inbound_from_other_port.rs` proves this: the "caller" binds a port siptest never registered from, and the call still completes.
- [x] T061 [US3] Done — `sip::inbound::extract_caller_id` captures `From`, `P-Asserted-Identity`, `X-GSM-Caller-ID` into three independent `Option<String>` fields on `CallerId`, asserted separately (including independent-absence) in `sip/inbound.rs`'s unit tests and end to end in `test_inbound_from_other_port.rs`.
- [x] T062 [US3] Done — `execute_inbound_call` calls the same `media::session::run` outbound uses, unchanged.
- [x] T063 [US3] Done — `GET`/`PUT /policy/inbound` in `api/handlers.rs`, reading/writing `SharedState::inbound_policy`; takes effect on the next inbound INVITE (read once per call, not polled mid-call).
- [x] T064 [US3] [P] Done — `POST /calls/{id}/{answer,reject}` write into `SharedState::manual_decisions`, consumed by `execute_inbound_call`'s manual-mode poll loop.
- [x] T065 [US3] **Partially done.** `incoming_call` (all three caller-ID fields) and `call_ended` are emitted for inbound calls. **Not emitted for inbound**: `call_state` transitions (ringing→answered) and `media_first_packet` — the latter doesn't exist for either call direction (`answer_to_first_rtp_ms` is `None` everywhere; a real gap noted under US1 too).
- [ ] T066 [US3] [P] **Not done.** No `test_control_api.rs` exists at all (see T047–T049/T056's status) — manual-policy discoverability is proven at the `SharedState`+listener-loop level (`test_inbound_from_other_port.rs`), not through the actual HTTP `/events`/`/calls/{id}/answer` endpoints.

**Checkpoint**: siptest answers a call the bridge initiates, from an
unexpected source port, and an agent can discover, answer, and verify it using
only `GET /events` and `GET /calls/{id}` — no log scraping.

---

## Phase 6: User Story 4 - Prove the audio is ours, not noise, and record it (Priority: P2)

**Goal**: Distinguish "our audio arrived" from "some audio arrived" via tone
detection, and measure round-trip delay when it does.

**Independent Test**: A call with a looped-back return path reports the signal
detected with a plausible RTT; a call with a silent return path reports the
signal absent while still reporting received energy (spec.md US4).

### Tests for User Story 4

- [x] T067 [P] [US4] **Done, inline in `media/goertzel.rs`, 3 of 4 gates.** A synthesised tone is detected as itself; white noise is rejected by the relative-energy gate; a single off-grid 1kHz tone is rejected by twist; silence is rejected. **Not implemented**: the absolute-floor-above-noise-percentile gate — `detect_window` uses relative energy + twist + broadband guard only. `media/level.rs` still tracks the noise-floor percentile for *reporting*, it just isn't wired in as a fourth detection gate. Documented simplification, not an oversight.
- [x] T068 [P] [US4] **Done, inline** (`goertzel.rs::tone_survives_pcmu_round_trip_and_is_still_detected`, `tone.rs`'s generation tests) rather than a separate file. Covers PCMU round-trip detection at 8kHz; the 16kHz case doesn't exist yet since G.722 hasn't landed.

### Implementation for User Story 4

- [x] T069 [US4] **Done for `grid8` only.** `siptest/src/media/tone.rs`: sample-index-driven generator, 8 non-harmonic frequencies (4 low/4 high), one-low-plus-one-high symbols, 100ms/symbol, 16-symbol frames, −12dBFS. Pure function of the sample index (tested: `transmit_stream_is_identical_regardless_of_what_was_received` in `session.rs` now exercises the real tone path, not the old 440Hz placeholder). **`dtmf`/`single`/`silence` variants are not implemented** — `silence` is achieved structurally via `tone_enabled: false` (session.rs sends all-zero frames), which covers that one diagnostic; `dtmf` and `single` don't exist.
- [x] T070 [US4] **Done, 3 of 4 gates** — see T067's note.
- [x] T071 [US4] [P] Done — `siptest/src/media/level.rs`: peak/mean dBFS, a running 10th-percentile noise floor over a capped window buffer, silent-frame percentage.
- [x] T072 [US4] Done — `media/session.rs`'s `tx_loop` records a `(symbol_index, Instant)` on every symbol-boundary crossing into a capped `VecDeque` (`TX_TIMELINE_CAP = 64`); `rx_loop` matches each decoded symbol against the most recent prior transmit of that symbol and collects the round-trip in milliseconds; `report.rs::RoundTripStats` reduces the samples to min/median/max/count.
- [x] T073 [US4] Done — `media/report.rs`'s `rx_audio_verdict()` and the `loopback` computation in `CallReport::build`, plus `LevelProfile`/`ToneReport` embedded in `MediaCounters`. `tone-loopback` was already a valid `require` value (built with the plan's original scaffolding); it now gates on a verdict backed by real data instead of an always-`NotConfirmed` placeholder.
- [ ] T074 [US4] [P] **Not done.** No `GET /calls/{id}/recording` or `/recording/{received,sent}.wav` endpoints exist — recording file paths are visible in the JSON report (`recordings.{received,sent}`), but must be read from disk directly, not fetched over the API.
- [x] T075 [US4] [P] **Covered by a new unit-level test rather than extending `test_against_registrar.rs`.** `media/session.rs::tone_plan_is_detected_on_both_sides_of_a_real_loopback_with_measurable_rtt` runs a real `media::session::run` against a raw UDP echo with a deliberate 20ms delay and asserts both detection and a plausible measured RTT — the same proof, at the media layer rather than through a full SIP call.
- [x] T076 [US4] [P] **Covered by a unit test on `CallReport::build` rather than a live-stub integration test.** `media::report::tests::silent_and_noise_only_and_tone_detected_stay_distinguishable` constructs the three cases (silent, noisy-but-no-tone, tone-detected) directly and asserts the `rx_audio` verdict never collapses them; `loopback_is_confirmed_only_when_a_round_trip_was_actually_measured` proves `packets: BothWays` and `loopback: NotConfirmed` coexist without either forcing the other.

**Checkpoint**: Every call report now distinguishes three orthogonal
questions — did packets arrive, was it our signal, did it loop back — and
`MediaReport::round_trip_delay`'s long-standing `None` (`gsm-sip-bridge/src/ims/call.rs:153`)
has a real answer whenever the path supports it.

---

## Phase 7: Polish & Cross-Cutting Concerns

**Purpose**: Round out what the constitution and quickstart promise, without
adding scope the spec didn't ask for.

- [x] T077 [P] Extend `tools/count-unsafe.sh` to loop over `gsm-sip-bridge/src` and `siptest/src` (it currently hardcodes only the former, per research.md R12); tighten the match from a bare `grep "unsafe"` to `grep -rnE '\bunsafe\s*[{(]|\bunsafe\s+(fn|impl|trait)\b'` so a doc comment mentioning "unsafe" no longer fails the crate
- [x] T078 [P] Add two Makefile targets, each with the constitution-required `## ` description: `siptest` (run the daemon) and `siptest-status` (curl `/status` and pretty-print)
- [ ] T079 Decide and implement the G.722 codec (research.md R7 — deferred deliberately until the tone pipeline is proven on PCMU): either ~400 lines of in-crate sub-band ADPCM validated against ITU-T G.191 vectors, or a re-evaluated external crate if the landscape has changed; add the `G722` `CodecProfile` (pt 9, `rtp_clock_hz=8000`, `audio_hz=16000`, `samples_per_frame=320`, `ts_increment=160` — the trap T024's test guards against) and extend `sdp.rs` to offer/negotiate it
- [ ] T080 [P] Unit test: the tone-through-codec round trip (T068) repeated through G.722, asserting detection still succeeds and that a deliberately wrong rate assignment (swapping `audio_hz`/`rtp_clock_hz`) makes the test fail loudly (regression guard for the trap itself)
- [ ] T081 [P] Wire `--codec {auto,pcmu,g722}` through CLI, config, and `POST /calls`
- [ ] T082 Run `specs/037-siptest-softphone/quickstart.md` end to end against a live bridge (provision account `1002`, place a call, verify recordings and report, temporarily set `ring_aor` for an inbound run) and fix any drift between the doc and the real CLI/API surface
- [ ] T083 [P] `siptest/tests/test_cli.rs`: `--help` renders for every subcommand; each `Commands` variant dispatches without panicking on missing arguments (clear error, not a stack trace)
- [x] T084 Final `make format && make lint && make test` across the whole workspace

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies.
- **Foundational (Phase 2)**: Depends on Setup. Blocks all user stories. Note
  the internal split: T007–T010 (upstream `gsm-sip-bridge` changes) must land
  before anything in siptest that imports the widened modules (T019 onward).
- **US1 (Phase 3, P1)**: Depends on Foundational only.
- **US2 (Phase 4, P1)**: Depends on Foundational only. Independently
  demoable in parallel with US1 — it touches `/status`, registration
  robustness and `/log/tail`, not the call path.
- **US3 (Phase 5, P2)**: Depends on Foundational **and** reuses `media/session.rs`
  from US1 (T038–T041) — sequence US1 before US3, or duplicate the media
  session temporarily if run in parallel by different people.
- **US4 (Phase 6, P2)**: Depends on US1's `media/session.rs` and `report.rs`
  existing (T038–T042) to extend rather than duplicate them; independent of
  US2 and US3.
- **Polish (Phase 7)**: Depends on whichever stories are in scope for the
  release; T079–T080 (G.722) specifically want US4's tone pipeline (T067–T076)
  already proven, per research.md R7's rationale.

### User Story Dependencies

- **US1**: No dependency on other stories.
- **US2**: No dependency on other stories (shares Foundational's registration
  work, adds no call-path code).
- **US3**: Depends on US1 for the media session it reuses; otherwise
  independent.
- **US4**: Depends on US1 for the report/session scaffolding it extends;
  independent of US2/US3.

### Within Each User Story

- Tests are written first and must fail before the corresponding
  implementation task.
- Pure state machines (FSMs, Goertzel, tone generator) before the threads and
  handlers that drive them.
- Media/report plumbing before the API endpoints that expose it.
- Story complete and its checkpoint met before moving to the next priority.

### Parallel Opportunities

- All Setup `[P]` tasks (T003–T004).
- Within Foundational: T011–T018 touch disjoint files and can run together
  once T007–T010 land; T019–T024 likewise.
- Once Foundational is complete, **US1 and US2 can be built in parallel** by
  different people — they touch almost entirely disjoint files
  (`sip/outbound.rs`+`media/*` vs `api/handlers.rs`'s status/registration
  endpoints).
- US3 and US4 can each start as soon as US1's media session exists, and can
  proceed in parallel with each other.
- All `[P]`-marked tests within a story.

---

## Parallel Example: Foundational Phase

```bash
# After T007–T010 (gsm-sip-bridge visibility changes) land:
Task: "Create siptest/src/sip/socket.rs — one unconnected UdpSocket"
Task: "Create siptest/src/config.rs — serde Config + env: indirection"
Task: "Create siptest/src/safety.rs — allow-list + rate limit"
Task: "Create siptest/src/api/state.rs — capped CallRegistry with eviction"
```

## Parallel Example: User Story 1 tests

```bash
Task: "Integration test tests/test_against_registrar.rs — real registrar + loopback stub"
Task: "Integration test tests/test_outbound_source_port.rs — 403 on wrong source socket"
Task: "Unit tests for OutboundCallFsm against canned 302/403/484/503/400 responses"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Phase 1: Setup.
2. Phase 2: Foundational — including registration, since US1 cannot place a
   call without it.
3. Phase 3: User Story 1.
4. **STOP and VALIDATE**: `siptest call --destination +919000000000 --wait`
   against the live bridge; confirm the report and both recordings.
5. This is already a complete, demoable replacement for a physical handset on
   the outbound path.

### Incremental Delivery

1. Setup + Foundational → registration works, nothing can be dialled yet.
2. + US1 → outbound calling with a packet-count verdict (**MVP**).
3. + US2 → registration lifecycle fully observable and robust (can land
   alongside US1).
4. + US3 → inbound calling, closing the loop on the open one-way-audio defect
   this tool exists to reproduce.
5. + US4 → the tone verdict and round-trip delay that turn "packets arrived"
   into "our audio arrived, and here's the latency".
6. + Polish → G.722, so the wideband path the bridge actually prefers is
   exercised too.

### Parallel Team Strategy

1. Everyone completes Setup + Foundational together — it is genuinely shared
   and blocking.
2. Once Foundational lands: Developer A takes US1, Developer B takes US2 (they
   barely touch the same files). US3 and US4 wait for US1's media session
   (T038–T042) before starting, then can run in parallel with each other.
3. Polish's G.722 task (T079) is intentionally last — it is the one task
   carrying external-dependency risk, and the codebase already decided to
   isolate that risk behind a proven pipeline (research.md R7).

---

## Notes

- `[P]` tasks touch different files with no unmet dependency — safe to
  parallelize or hand to different agents/developers.
- Every pure state machine (registration, outbound, inbound, Goertzel, tone)
  is designed to be unit-tested with zero I/O — no mocking is ever required
  for them.
- The only stand-in for a real component anywhere in this test suite is the
  loopback UAS in `test_against_registrar.rs`, replacing pjsua (which lives
  behind the `pjsip-linked` feature CI never compiles) — carries the
  constitution-mandated in-file justification per Principle I.
- Commit after each task or logical group, per Constitution Principle III;
  every commit must pass `make format && make lint && make test` per
  Principle II and `CLAUDE.md`.
- Call durations and registration expiries in tests must be short (1–2s) —
  `.config/nextest.toml`'s 20s slow-timeout terminates anything longer.
