# Contract: `gsm-sip-bridge healthcheck` (extended)

Extends the existing contract (`commands::healthcheck::evaluate`/`run`, and the Docker
`HEALTHCHECK` that invokes it) — every current healthy/unhealthy determination for metrics
endpoint state and resolved-line faults (`Health::LinesUnhealthy`) is unchanged.

## Current behavior (unchanged)

- Metrics endpoint unreachable → `Health::MetricsEndpointDown` (unhealthy).
- `[vowifi].enabled = false`, or enabled with zero resolved lines → `Health::Healthy` (or
  `Health::CircuitSwitchedDisabled` if `[cs].enabled = false` too).
- Any resolved line failing its own tunnel/registration probe → `Health::LinesUnhealthy`.

## New behavior

A resolution file whose `failed` list contains an entry for an explicitly configured override (a
`modem_port`/`modem_serial` pin or `pcsc_reader` entry) that has gone **terminal** — i.e. its
bounded retry window elapsed without the line resolving (`data-model.md`: only terminal failures
are ever written to `failed`, so presence there is sufficient, no separate "is it still retrying"
check is needed) — is treated as a fault, not as "nothing to report":

- If this is the *only* problem (metrics endpoint fine, every resolved line healthy): overall
  result is unhealthy, not `Health::Healthy`/`Health::CircuitSwitchedDisabled` as it is today.
- Reported through the same `Health::LinesUnhealthy`-style mechanism already used for a resolved
  line's own faults, so a scrape/console consumer sees one consistent shape rather than a second,
  parallel unhealthy category to special-case.
- A configured line still inside its retry window (not yet in `failed`) does **not** flip health —
  matches FR-008 while avoiding flagging ordinary startup churn as an outage.

## Exit code

`healthcheck`'s existing exit-code convention (`0` healthy, non-zero unhealthy — whatever the
current `run()` already maps `Health` variants to) is extended to the new fault the same way any
other `LinesUnhealthy` fault already maps, so Docker's `HEALTHCHECK` starts failing for this
condition using the exact mechanism it already has, no new Dockerfile change required.

## Backward compatibility

A deployment where every configured override always resolves on the first `discover` pass (today's
common case, and every existing integration test's fixture) never populates a configured-line
`failed` entry, so `healthcheck`'s output is unchanged for it.
