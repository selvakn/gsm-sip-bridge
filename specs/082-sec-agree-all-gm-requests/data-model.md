# Data Model: Security-agreement headers on every Gm request

## GmSa (replaces the `gm_state` tuple)

| Field | Meaning |
|---|---|
| `endpoints` | `GmEndpoints`: the local and remote protected ports and addresses |
| `proposal` | `SaProposal`: what we offered |
| `theirs` | `SecurityServerParams`: the offer we selected |
| `security_verify` | the P-CSCF's full `Security-Server` list, joined with `", "`, verbatim |

Created in `register_session` when SAs install successfully. Dropped with the
session. `cleanup()` and `reconnect_transport()` read `endpoints`, `proposal`
and `theirs` from it. It exists exactly when a Gm SA does, so the echo cannot
outlive or precede its SA.

## Sec-agree header block

Derived, not stored: `sec_agree_headers(Option<&str>)` gives `""` for `None`
and otherwise `Require: sec-agree`, `Proxy-Require: sec-agree`,
`Security-Verify: <list>`, each ending in CRLF.

## DialogInfo

Gains `security_verify: Option<String>`, copied from the session at call setup.
