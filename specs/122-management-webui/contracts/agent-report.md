# Contract: `AgentReport.details` (observability protocol addition)

`control::protocol::AgentState` is `Copy`, and so is `metrics::ingest`'s `AgentRecord`, which is read with `existing.copied()`. The new data includes a `String` and a `Vec`, so it **cannot** go on `AgentState` without dropping `Copy` across ingest. It travels instead as a new sibling field on `AgentReport`:

```rust
pub struct AgentReport {
    // ...existing fields...
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub details: Option<AgentDetails>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentDetails {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub registration_state: Option<RegistrationStateWire>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub registered_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub plmn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub msisdn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub signal: Option<SignalReadingWire>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub active_call_details: Option<Vec<ActiveCallInfoWire>>,
}
```

Mixed versions are safe in both directions. An older agent omits `details`, so the daemon shows unknown. An older daemon ignores the field, because `AgentReport` isn't `deny_unknown_fields` (checked in `control/protocol.rs` at `343c2f2`).

| Field | Sent by | Meaning |
|---|---|---|
| `registration_state` | `Ims`, `Volte` | A closed enum mirroring `ims::RegistrationState`, rendered by the daemon with the same `Display` the CLI uses (FR-012) |
| `registered_at` | `Ims`, `Volte` | Unix seconds of the current registration; `None` when not registered |
| `plmn` | `Ims`, `Volte` | `"MCC-MNC"`, e.g. `"405-857"` |
| `msisdn` | `Ims`, `Volte` | Telephone identity from `P-Associated-URI` of an accepted REGISTER. A nonempty value is the learned number; `Some("")` explicitly clears it. Omitted/`None` means no update, including reports from older agents |
| `signal` | `Ims`, `Volte`, modem-backed lines only | Latest reading (data-model.md `SignalReading`) |
| `active_call_details` | `Ims`/`Volte` (inbound), `Sip`/`VolteSip` (outbound) | Calls in progress; `Some(vec![])` means none. Capped at 4 entries |

## Storage and merge in `metrics::ingest`

- Details are kept in a **separate** `Mutex<HashMap<(AgentKind, String), AgentDetails>>` (`ingest::details()`), so `AgentRecord` stays `Copy`. They are updated under the same `(epoch, seq)` replay check as the state.
- Each `Some` field overwrites the stored value. A `None` field leaves the previous value in place.
- `msisdn` is reported by the carrier-facing agent only. It is updated after successful initial/full registration or an accepted in-place renewal, without an extra request. A renewal that omits `P-Associated-URI` keeps the session's previous identity; a replacement header with no telephone identity clears the learned number. `tel:` identities take priority over SIP identities whose user starts with `+` or whose URI has `user=phone`; unmarked numeric SIP identities (including IMSIs) are not inferred to be phone numbers.
- `active_call_details` replaces that agent's list. The UI merges the lists of the two agents that serve a line (`Ims`+`Sip`, `Volte`+`VolteSip`) by `module_id`.
- A stale agent's `active_call_details` is cleared, the same way `ACTIVE_CALLS` is zeroed today (`metrics::server::refresh_agent_liveness`). Its `signal` is kept, and the UI marks the line stale.
