# Contract: `[outbound]` configuration schema

**Feature**: 025-outbound-calling

Operator-facing contract, enforced by `#[serde(deny_unknown_fields)]` plus
`config/build.rs::build_outbound` and the cross-section rules in `build()`.
`gsm-sip-bridge/tests/test_config_docs.rs` checks every key here appears in
`docs/configuration.md`.

---

## Shape

```toml
[outbound]
enabled = false   # bool
```

Deliberately minimal, per spec.md's resolved clarifications: no allow-list
(FR-011), no path preference (FR-007), no per-card selection knob — enabling
the feature is the only operator-visible decision.

## Defaults

| Key | Default | Notes |
|---|---|---|
| `enabled` | `false` | Disabled by default (FR-001); existing deployments are byte-for-byte unaffected (FR-017). |

## Validation

None beyond structural parsing. Unlike `[sip_server]`, `[outbound]` has no
cross-section requirement to check at config-build time: circuit-switched
modems are discovered at runtime from whatever USB hardware is plugged in
(`modules::discovery`), not declared in config, so "at least one carrier
path exists" is not a fact `config::build` can observe. A deployment with
`[outbound].enabled = true` and no line ever idle simply refuses every
outbound request at runtime (FR-009) — the same place it would refuse one
if a line existed but never came up.

## Interaction with existing sections

- **`[sip_server]`**: no schema change. The registrar's `INVITE` branch
  behavior changes (302 redirect instead of 403) purely based on
  `[outbound].enabled`, at runtime, not at config-validation time — a phone
  that is refused today is refused identically with `[outbound]` absent or
  `enabled = false` (FR-017).
- **`[bridge]`**: no schema change. The PBX-trunk account's UAS INVITE
  handling is likewise gated at runtime on `[outbound].enabled`.
