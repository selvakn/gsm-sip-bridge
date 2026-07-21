# Phase 0 Research: Restore Call and SMS Observability Under VoWiFi

All unknowns from the Technical Context are resolved below. Each entry records
what was chosen, why, and what was rejected.

---

## R1. How do the agents get their events to the process that serves `/metrics`?

**Decision**: Reuse the existing CLI control Unix socket
(`[control].socket_path`, default `/tmp/gsm-sip-bridge.sock`) with one new
one-shot command variant, `ControlCmd::Observe`. One short-lived connection per
report, same newline-JSON framing as every other control command.

**Rationale**:

- **Unix sockets are not network-namespaced.** Agent A runs under `ip netns exec
  ims`, which changes the *network* namespace. `ip netns exec` does also unshare
  the mount namespace, but it marks mounts as slave and only bind-mounts
  `/etc/netns/<name>/*` over `/etc/*` — the rest of the filesystem, including the
  socket path, stays the same shared tree. So Agent A can connect to the daemon's
  socket with no veth routing, no port, and no firewall rule. This is the single
  fact that makes a one-target design possible, and it is what makes the
  "relay through Agent B" hop unnecessary.
- The framing, the read/write helpers (`control::protocol::read_cmd` /
  `write_resp`), and the server accept loop already exist and are already
  covered by tests and by the mutation-testing target in the Makefile.
- The existing server handles exactly one command per connection and then closes.
  That is a good fit, not a limitation: each report is an independent
  request/response with no session state, so an agent restart, a daemon restart,
  or a dropped connection needs no resynchronisation logic on either side.
- Reports are routed inside `handle_connection` straight to the metrics ingest
  and never forwarded to `CardPool` over `cmd_tx`, so a burst of call events
  cannot interfere with card control commands or with the pool's mailbox.

**Alternatives considered**:

| Alternative | Rejected because |
|---|---|
| A metrics endpoint per agent, three scrape targets | Directly violates FR-015 (no new scrape targets) and forces `sum()` rewrites across every existing panel, violating FR-016. Agent A would also need its listener reachable across the netns boundary — a veth address plus a host firewall rule, which is precisely the fragile surface that the v6.2.0 incident notes warn about. |
| Relay events A → B → daemon over the existing veth control channel | Adds a hop that buys nothing once R1's netns fact is established, and couples Agent A's observability to Agent B's liveness — a restarting Agent B would silently drop Agent A's registration state. |
| A second, dedicated observability Unix socket | More moving parts than reusing one socket (Principle V) with no benefit; the control socket carries a handful of CLI commands per day and cannot be starved by single-digit events per call. |
| StatsD / OTLP / Prometheus push gateway | A new dependency and a new process for a problem the existing socket already solves. Push gateway additionally has the wrong lifecycle semantics for per-call events. |

---

## R2. Who owns the cumulative counters?

**Decision**: The daemon. Agents send **deltas** for counters (`calls: +1`) and
**absolute values** for gauges (`registered: true`, `active_calls: 2`).

**Rationale**: FR-020 requires counters not to reset or move backwards when a
supervised agent restarts — and `entrypoint.sh` restarts both agents on a 5s
loop, so this is a routine event, not an edge case. If agents held counters and
reported absolutes, every restart would rewind the series to zero and corrupt
every `rate()` over that window. With the daemon holding them, an agent restart
is invisible to the counter: it simply stops contributing for a few seconds.

A daemon restart *does* reset counters to zero, but that is the normal Prometheus
process-restart semantic that `rate()` already handles, and the daemon's own
`uptime_seconds` makes it visible.

**Alternatives considered**: agents holding counters and reporting absolute
totals (rejected per above); agents reporting monotonic per-process counters with
the daemon summing across generations (rejected — needs generation tracking and
retained state per dead agent, for no gain).

---

## R3. How is exactly-once counting guaranteed across three processes? (FR-017)

**Decision**: Structural ownership. Each observable fact has exactly one process
allowed to report it, so double-counting is impossible by construction rather
than by deduplication.

| Fact | Sole owner | Why that owner |
|---|---|---|
| VoWiFi call started / answered / ended / failed | **Agent A** | It parses every inbound carrier INVITE, including calls that never reach Agent B (control channel down, decline before bridging). Agent B sees only calls that got as far as bridging. |
| VoWiFi call history row | **Agent A** | Same completeness argument; FR-009 requires a row for *every* inbound call. Agent A knows caller, start, duration, and outcome, and reads the same `[bridge].sip_destination` config value Agent B dials. |
| PBX-leg outcome (`sip_calls_total`) | **Agent B** | It owns the PJSIP leg to the PBX; Agent A never sees its result except as a relayed control message. |
| SMS received / forwarded, SMS history row | **Agent B** | It already owns the Discord client and the store handle for this path (`vowifi/mod.rs` → `sms::record_and_forward`); the forwarding outcome exists nowhere else. |
| IMS registration state, tunnel state | **Agent A** | Both are properties of Agent A's netns and its own SIP transport. |
| Circuit-switched calls and SMS | **Daemon** | Unchanged from today. |

**Rationale**: Deduplication across processes would need a shared idempotency key
and retained state on the daemon; ownership needs neither. It also makes the
failure modes legible — if VoWiFi calls stop appearing, exactly one process is
responsible.

**Consequence to accept**: a call that fails *before* Agent A can attribute an
outcome (Agent A itself crashing mid-call) is lost. That is unavoidable without
durable per-call state and is well inside the spec's best-effort loss policy.

---

## R4. What survives the collector being unavailable? (FR-019 family)

**Decision**: Each agent runs one `Reporter` — an unbounded-in-name-only channel
feeding a bounded `VecDeque` (capacity **1024 reports**) drained by a dedicated
sender thread. On send failure the report stays queued. When the queue is full
the **oldest** report is discarded and a local `dropped` counter increments; that
count rides along on the next successful report and becomes
`gsm_sip_bridge_observability_events_dropped_total` on the daemon.

**Rationale**:

- 1024 reports is roughly 3 hours of heartbeats at 10s, or thousands of calls —
  far beyond any routine daemon restart, and bounded at a few hundred KB.
- Dropping the *oldest* keeps the most recent state, which is what gauges care
  about. A stale heartbeat has no value once a newer one exists.
- The sender thread means the call path never blocks on a socket (FR-018): the
  call site does a non-blocking enqueue and returns.
- Buffer contents are deliberately **not** persisted (FR-019b) — a crashed agent
  starts clean, and the next heartbeat re-establishes all gauge state within one
  interval anyway.

**Alternatives considered**: unbounded queue (rejected — the memory-constrained
container failure mode the spec explicitly calls out); disk-backed spool
(rejected — durable delivery was ruled out by the Q1 clarification); dropping
newest (rejected — would discard the freshest state, the opposite of useful).

---

## R5. How do point-in-time indicators recover, and how is a dead agent visible? (FR-021 family)

**Decision**: Each agent sends a full `AgentState` heartbeat every **10 seconds**
regardless of activity (`[metrics].agent_report_interval_seconds`, default 10).
The daemon records the arrival instant per agent. The `/metrics` handler
**evaluates staleness at scrape time**: if an agent's last report is older than
3× the interval, the daemon sets `agent_up{agent}` to 0 and zeroes that agent's
active-call gauge and health gauges.

**Rationale**:

- Bounds staleness to one interval after a daemon restart (FR-021a, SC-009)
  without the daemon having to persist or query anything.
- Expiring at scrape time reuses the pattern already in `metrics_handler`, which
  refreshes `UPTIME_SECONDS` on every scrape. No timer, no extra task.
- Zeroing on expiry is what prevents the stuck-gauge failure: a crashed Agent A
  cannot leave `active_calls` pinned at 1 forever.
- 10s against Prometheus's 15s scrape interval means a scrape essentially always
  has a fresh report, and 3× gives one missed heartbeat of tolerance before an
  agent is declared down.

**Alternatives considered**: daemon queries agents at scrape time (rejected —
couples scrape latency to two agents, one across a netns boundary, and a hung
agent would hang the scrape); rebuild from events only (rejected by the Q3
clarification — registration state would stay wrong until the next event, up to
an hour away).

---

## R6. What is "tunnel up", concretely?

**Decision**: Agent A's own view — a P-CSCF assignment is present (its
`pcscf_source_path` file has been read successfully) **and** Agent A's SIP
transport toward that P-CSCF is alive. Exported as
`gsm_sip_bridge_vowifi_tunnel_up`, documented as an inside-the-netns liveness
proxy rather than an IKE/ESP SA state.

**Rationale**: The tunnel is established by charon under `entrypoint.sh`
supervision, outside any Rust process. What Agent A can observe truthfully and
cheaply is whether it has a P-CSCF and whether it can talk to it — which is also
the thing an operator actually cares about ("can we receive calls?"). Reporting
that honestly beats reporting a number we would have to invent.

**Alternatives considered**: querying charon's SA state over vici (rejected *for
now* — a vici client in Agent A is a meaningful lift, and vici lives in the
default netns while Agent A does not; worth revisiting if operators find the
proxy signal insufficient); parsing `ip xfrm state` (rejected — brittle
shell-out, and presence of an SA does not imply a usable path).

---

## R7. What module identity do VoWiFi calls and SMS carry? (FR-011a)

**Decision**: `modules::discovery::derive_module_id` applied to the USB serial of
the modem at `[vowifi].modem_port`, resolved through sysfs
(`/sys/class/tty/<tty>/device/…` walked up to the USB device's `serial`
attribute). Falls back to the literal `vowifi` with a warning if resolution
fails.

**Rationale**:

- Uses the **same function** the circuit-switched path uses, so when one modem
  serves both transports the ids are identical — which is what makes a per-module
  panel show that card's complete traffic (FR-011a).
- Deterministic and stable across restarts, since it derives from hardware
  identity rather than from anything runtime-assigned.
- Keeps the IMSI out of labels and rows (FR-011b).
- Note the modem carrying the SIM may be a **VoWiFi-only module** that
  `scan_modules` deliberately skips (no circuit-switched audio path). Deriving
  the id directly from the port rather than from the discovery results means the
  identity works in that case too.

**Alternatives considered**: IMSI-derived (rejected by the Q2 clarification — a
sensitive subscriber identifier in every label and row); the current fixed
`vowifi` string (rejected — collapses all cards to one series and cannot
distinguish two modems); asking the daemon for the id (rejected — the daemon may
never have discovered that modem).

---

## R8. How does the transport dimension reach the existing metrics and tables?

**Decision (metrics)**: Widen the six existing call/SMS metric vecs with a
`transport` label; every circuit-switched call site passes `"cs"`, every VoWiFi
one passes `"vowifi"`.

**Decision (schema)**: A v2 → v3 migration adds `transport TEXT NOT NULL DEFAULT
'cs' CHECK (transport IN ('cs','vowifi'))` to `calls` and `sms`, and recreates
the two views to include it. Existing rows take the default, which is factually
correct — the VoWiFi path has never written a row.

**Rationale**: A Prometheus metric has one fixed label set across all its series,
so there is no way to have VoWiFi and circuit-switched share a metric name
without both carrying the label. The `DEFAULT` clause makes the backfill
(FR-011c) free — SQLite applies it to every existing row during `ALTER TABLE`,
with no data-migration pass. The existing `init_schema` match on
`meta.schema_version` already provides the migration shape to follow, and
`test_migration_sql.rs` the test shape.

**Consequence flagged in plan.md**: widening the label set changes series
identity for existing circuit-switched metrics, which collides with SC-006 as
literally written. Values and panels are unaffected. See plan.md § Spec Delta.

**Alternatives considered**: separate metric names for VoWiFi (rejected —
existing panels would keep showing nothing, defeating FR-016); a new table for
VoWiFi history (rejected by FR-010, and would double every query).
