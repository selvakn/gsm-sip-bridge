# Phase 0 Research: Reliable SMS Delivery

No open `[NEEDS CLARIFICATION]` markers remain in the spec (both resolved during `/speckit-specify`). This document instead records the technical decisions made while translating the spec's requirements into a concrete approach, including one gap discovered during research that changes scope.

## Decision 1: Reuse `volte::sms`'s modem-storage sweep for VoWiFi rather than building a parallel mechanism

**Decision**: Wire the existing `volte::sms::run_modem_reader` (currently spawned only by `commands/volte.rs`'s `volte-carrier-agent` and the `volte::bridge` diagnostic path) into `ims::agent::run_inner` — the entry point for `vowifi-ims-agent` — for any line where `!config.pcsc_reader` (i.e. a real modem, not a PC/SC-only card).

**Rationale**: This is the exact problem VoLTE already solved (specs/017-volte-inbound-bridge US5): a registration that advertises voice but not messaging capability can still have the carrier deliver texts into the modem's own storage instead of over the registration. `run_modem_reader`, `sweep_modem_storage`, `Dedupe`, `decide`, and `InboundMessage`/`MessageRoute` are already generic — they take a modem port, a control address, and a lock, and relay through the same `ControlMessage::SmsReceived` shape the IMS-registration path already uses (`ims::agent::handle_message`). Building a second, VoWiFi-specific copy would duplicate logic the Constitution's Simplicity principle argues against.

**Alternatives considered**:
- A new `vowifi::sms` module mirroring `volte::sms` — rejected: pure duplication of already-tested logic for no behavioral difference.
- Switching to event-driven detection (subscribing to the modem's unsolicited new-message notification instead of polling) — rejected per the resolved FR-010 clarification: the existing ~20s poll is the required mechanism, partly because the AT port already has a documented history of wedging under concurrent/interleaved traffic (see `config::mod::VowifiConfig::imei_override` doc comment), and introducing a new AT interaction pattern on that port is exactly the kind of risk this feature should avoid, not add.

## Decision 2: Cross-bearer duplicate suppression (FR-003) requires new wiring, not just VoWiFi's addition — a gap exists today, including for VoLTE

**Finding**: `MessageRoute::OverRegistration` — the enum variant meant to represent "this message arrived over the IMS registration" — is constructed **only in `volte::sms`'s own unit tests**, never in production code. The production registration-message handler, `ims::agent::handle_message` (shared by both `vowifi-ims-agent` and `volte-carrier-agent`, since the latter also calls `ims::agent::serve_inbound`), relays every inbound `MESSAGE` unconditionally — it never calls `decide()`/consults a `Dedupe`. Meanwhile `run_modem_reader` constructs and owns its **own private** `Dedupe` internally (`let mut dedupe = Dedupe::default();` inside the function), which only suppresses a message being re-read across repeated sweep passes — it shares no state with the registration path.

The result: today, if the same message is delivered over **both** bearers for the same line, the operator sees it **twice** — for VoLTE as much as for VoWiFi-after-this-fix. The unit tests in `test_volte_sms.rs` (e.g. `the_same_message_arriving_on_both_routes_is_recorded_once`) prove the `decide`/`Dedupe` *logic* is correct, but nothing in production actually invokes it from both call sites with shared state. This is a genuine, previously-unnoticed gap, not an assumption this feature can safely inherit as "already solved."

**Decision**: Introduce one shared `Arc<Mutex<Dedupe>>` per line, owned by the process that runs both halves (already always the same process for both VoWiFi and VoLTE, once the VoWiFi wiring above lands: `vowifi-ims-agent --line N` and `volte-carrier-agent --line N` each run `serve_inbound` and the modem-sweep thread together). Thread it into:
- `ims::agent::handle_message` (and its callers/`InboundParams`/`DispatchParams`), tagging the message `MessageRoute::OverRegistration` and running it through `decide()` before relaying.
- `volte::sms::run_modem_reader`/`sweep_modem_storage`, accepting the shared `Dedupe` as a parameter instead of constructing its own.

**Rationale**: This is the minimal change that makes FR-003 actually true, reusing the already-correct pure logic rather than redesigning it.

**Alternatives considered**:
- Persist a dedupe table in SQLite so suppression survives a restart — rejected: the existing bounded, in-memory, non-persisted window is a deliberate, already-documented design choice (absorbing a same-session retransmission, not meant to catch a genuine repeat message hours later); Constitution Principle V favors keeping it that way rather than adding persistence for a case the design already excludes on purpose.

## Decision 3: Record which bearer delivered a message via structured logging, not a new database column

**Decision**: Satisfy FR-009 ("record which bearer... so delivery-path behavior remains observable") by adding a `route` field (`"registration"` or `"modem"`) to the existing `tracing` events already emitted at the relay point (`received SIP MESSAGE` in `handle_message`, and the per-message relay logic in `sweep_modem_storage`), rather than adding a new `sms` table column.

**Rationale**: The `sms` table's existing `transport` column (`cs`/`vowifi`/`volte`, added in schema v3/v4 for specs/014) already answers "which subsystem" a message came through. FR-009 only asks that the *bearer within* that subsystem be observable/diagnosable — a lower bar than durable, queryable history — and this project's existing structured-tracing + metrics stack already serves that purpose for comparable questions elsewhere (e.g. `MessageRoute::as_str()` already exists specifically to make the route "observable" per its own doc comment). Adding a schema column and a migration (v5) for this is more moving parts than the requirement demands.

**Alternatives considered**: Add a `bearer` column to `sms` (schema v5 migration, `SmsRecord`/`insert_sms` changes) — rejected for now as heavier than FR-009 requires; revisit only if the operator later wants to query historical bearer mix rather than just see it in logs at the time.

## Decision 4: `pcsc_reader` lines are out of scope for the modem sweep

**Decision**: The modem-sweep thread is only spawned when `!config.pcsc_reader`.

**Rationale**: A `pcsc_reader` line has no modem/cellular attach at all — the SIM sits in a PC/SC reader used only for EAP-SIM authentication over Wi-Fi — so there is no legacy cellular bearer to poll. This matches FR-004 exactly and mirrors the existing `if config.pcsc_reader { ... } else { AtCommander::open(&config.modem_port) ... }` gate already present in `ims::agent::run_inner` for the same reason.

## Testing approach

Per the project's established precedent for this exact mechanism (`test_volte_sms.rs`) and the Constitution's Integration-First principle (real components; mocks only for hardware impractical to run in CI): the pure decision logic (`Dedupe`, `decide`, the new shared-dedupe wiring, route tagging) gets tests using real in-process types — no mocking of `Dedupe` itself. Actual AT-port I/O against a real EC20 modem is exercised via the existing `UnixStream`-pair mock-serial harness (`tests/test_at_commander.rs`'s `create_mock_serial`) where the unit under test can be pointed at an already-open stream; the thread-spawn wiring in `run_inner` (which opens a real device path) is verified by code inspection and manual on-device testing, consistent with how the VoLTE equivalent was verified — full simulated-hardware integration tests for the spawn path itself are not part of this project's existing test strategy and are not introduced here.
