# Documentation

Start with the [project README](../README.md) for an overview and the
Docker Compose quick start, then come back here for depth.

## Getting started

| Doc | What it covers |
|---|---|
| [Quick Start](../README.md#quick-start-docker-compose) | Deploy the full stack with Docker Compose |
| [supported-hardware.md](supported-hardware.md) | Which modem/reader models work, which call modes (CS/VoWiFi/VoLTE) each supports, and what's actually been live-tested vs. just architecturally possible |
| [hardware-setup.md](hardware-setup.md) | One-time EC20 prep: enable USB audio, disable ModemManager, permissions |
| [configuration.md](configuration.md) | Every `config.toml` key, with defaults, ranges, and example configs |

## Guides

| Doc | What it covers |
|---|---|
| [operations.md](operations.md) | Day-2 runbook: `card` CLI, database queries/prune/backup, troubleshooting, and the [VoLTE bridge reference](operations.md#host-side-ims-over-lte-volte) |
| [observability.md](observability.md) | Prometheus metrics reference, Grafana dashboard, database schema |
| [ec20-volte-setup.md](ec20-volte-setup.md) | Enabling the EC20's *own* modem-internal VoLTE (MBN profile deactivation, AT commands) — distinct from the host-side VoLTE bridge below |
| [migrating-config-reorg.md](migrating-config-reorg.md) | Upgrading to the restructured `config.toml` (`[audio]`/`[modem_audio]` split, per-line `[[vowifi.line]]`/`[[volte.line]]`) |
| [development.md](development.md) | Building from source, Makefile targets, pre-commit checks |
| [migrating-from-v4.1.x.md](migrating-from-v4.1.x.md) | Upgrading from the C++ v4.1.x to the Rust v5.x |

## Architecture

| Doc | What it covers |
|---|---|
| [architecture.md](architecture.md) | Crate layout, all three call flows (CS, VoWiFi, VoLTE), audio pipeline, multi-card/multi-line design |
| [vowifi-bridge.md](vowifi-bridge.md) | The VoWiFi-to-SIP bridge in depth: two-agent design, codecs, control protocol |
| [omnikey-pcsc-vowifi.md](omnikey-pcsc-vowifi.md) | A VoWiFi line backed by a physical PC/SC reader (e.g. OmniKey AG 3x21) instead of a modem — config, IMSI/IMEI handling, verification checklist, troubleshooting |

## Design notes & engineering history

Kept for the reasoning and findings, not as how-to guides.

| Doc | What it covers |
|---|---|
| [vowifi-epdg-research-notes.md](vowifi-epdg-research-notes.md) | ePDG tunnel, IMS-AKA registration, Gm IPsec debugging, per-carrier findings (historical) |
| [gm-ipsec-xfrm-plan.md](gm-ipsec-xfrm-plan.md) | Design plan for the kernel-XFRM Gm IPsec implementation (implemented) |
| [audio-tuning-log.md](audio-tuning-log.md) | Running log of modem/SIP audio parameter changes and their outcomes |

Per-feature specs, plans, and task breakdowns live under
[`specs/`](../specs/) — most recently `023-omnikey-pcsc-vowifi` for
PC/SC card-reader-backed VoWiFi lines, `015-volte-host-ims` through
`020-volte-line-netns` for the host-side VoLTE bridge (registration, calls,
inbound bridging, multi-modem, per-line network isolation), and
`011-vowifi-sip-bridge` through `014-vowifi-metrics-restore` for the VoWiFi
work.
