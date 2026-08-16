# Phase 1 Data Model: Reliable SMS Delivery

No database schema changes (see research.md Decision 3). This feature is entirely about wiring and sharing existing runtime types across two call sites, plus one new shared piece of state. No new persisted entities.

## Existing types reused as-is

- **`InboundMessage`** (`volte::sms`) — `route: MessageRoute`, `sender: String`, `body: String`, `modem_index: Option<u32>`. Already generic across both bearers; unchanged.
- **`MessageRoute`** (`volte::sms`) — `OverRegistration | ThroughModem`. Already exists; this feature is what finally makes `OverRegistration` get constructed in production code (see research.md Decision 2), not just tests.
- **`Dedupe`** (`volte::sms`) — bounded, in-memory, non-persisted duplicate-suppression window keyed on `sender + body`. Unchanged internally; this feature changes *who owns the instance* (see below), not its logic.
- **`Disposition`** (`volte::sms`) — `Handle | AcknowledgeOnly`. Unchanged.

## Changed ownership / lifetime

- **Per-line shared `Dedupe`**: today, `run_modem_reader` constructs a private `Dedupe::default()` local to its own loop. This feature changes it to accept an externally-owned `Arc<Mutex<Dedupe>>`, so the same instance can also be consulted by the registration-message handler (`ims::agent::handle_message`) for the same line. One instance per line (per OS process — `vowifi-ims-agent --line N` / `volte-carrier-agent --line N`), created alongside that line's existing `modem_lock: Arc<Mutex<()>>` at the same call site.

## New parameters threaded through existing functions

No new structs. Existing function signatures gain a shared-dedupe handle:

- `ims::agent::handle_message(sink, req, control_addr)` → gains a `dedupe: &Arc<Mutex<Dedupe>>` parameter (or is reached via an existing params struct — `DispatchParams`/`InboundParams` already thread comparable per-line state through, e.g. `modem_lock`).
- `volte::sms::run_modem_reader(modem_port, control_addr, modem_lock)` → gains a `dedupe: Arc<Mutex<Dedupe>>` parameter, replacing its internal `Dedupe::default()`.
- `volte::sms::sweep_modem_storage(modem_port, control_addr, dedupe)` → `dedupe` becomes `&mut Dedupe` reached through the shared `Mutex` guard rather than an owned local.

## Observability (FR-009)

No new column. A `route` field (`"registration"` | `"modem"`, i.e. `MessageRoute::as_str()`) is added to the existing `tracing::info!` call sites at the point each message is relayed (`handle_message`'s `"received SIP MESSAGE"` event, and the per-message relay inside `sweep_modem_storage`). Purely additive to existing log lines — no new event names, no schema impact.
