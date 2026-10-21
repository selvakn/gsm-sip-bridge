# Contract: configuration

| Section | Key | Type | Default | Meaning |
|---|---|---|---|---|
| `[vowifi]` | `sms_report_pcscf_route` | boolean | `true` | Send the SMS delivery report with the P-CSCF as its first `Route` (TS 24.341 Annex B.6). Off restores the previous route set (Service-Route entries only). |
| `[volte]` | `sms_report_pcscf_route` | boolean | inherits `[vowifi]` | Same meaning for the VoLTE path; absent means "use `[vowifi]`'s value", matching `sms_delivery_report`. |

`In-Reply-To` has no setting. `docs/configuration.md` gets one row per section.
