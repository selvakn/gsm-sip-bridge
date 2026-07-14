# Documentation

Start with the [project README](../README.md) for an overview and the
Docker Compose quick start, then come back here for depth.

## Getting started

| Doc | What it covers |
|---|---|
| [Quick Start](../README.md#quick-start-docker-compose) | Deploy the full stack with Docker Compose |
| [hardware-setup.md](hardware-setup.md) | One-time EC20 prep: enable USB audio, disable ModemManager, permissions |
| [configuration.md](configuration.md) | Every `config.toml` key, with defaults, ranges, and example configs |

## Guides

| Doc | What it covers |
|---|---|
| [operations.md](operations.md) | Day-2 runbook: `card` CLI, database queries/prune/backup, troubleshooting |
| [observability.md](observability.md) | Prometheus metrics reference, Grafana dashboard, database schema |
| [ec20-volte-setup.md](ec20-volte-setup.md) | Enabling VoLTE on the EC20 (MBN profile deactivation, AT commands) |
| [development.md](development.md) | Building from source, Makefile targets, pre-commit checks |
| [migrating-from-v4.1.x.md](migrating-from-v4.1.x.md) | Upgrading from the C++ v4.1.x to the Rust v5.x |

## Architecture

| Doc | What it covers |
|---|---|
| [architecture.md](architecture.md) | Crate layout, CS and VoWiFi call flows, audio pipeline, multi-card design |
| [vowifi-bridge.md](vowifi-bridge.md) | The VoWiFi-to-SIP bridge in depth: two-agent design, codecs, control protocol |

## Design notes & engineering history

Kept for the reasoning and findings, not as how-to guides.

| Doc | What it covers |
|---|---|
| [vowifi-epdg-research-notes.md](vowifi-epdg-research-notes.md) | ePDG tunnel, IMS-AKA registration, Gm IPsec debugging, per-carrier findings (historical) |
| [gm-ipsec-xfrm-plan.md](gm-ipsec-xfrm-plan.md) | Design plan for the kernel-XFRM Gm IPsec implementation (implemented) |
| [audio-tuning-log.md](audio-tuning-log.md) | Running log of modem/SIP audio parameter changes and their outcomes |

Per-feature specs, plans, and task breakdowns live under
[`specs/`](../specs/) — most recently `011-vowifi-sip-bridge` and
`012-strongswan-epdg` for the VoWiFi work.
