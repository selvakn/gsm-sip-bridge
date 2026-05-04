# Contract — CLI surface

**Binary**: `gsm-sip-bridge`
**Argument parser**: `clap` v4 derive
**Source of truth**: `gsm-sip-bridge/src/cli.rs`

This contract defines the user-facing CLI. Operators rely on these flags; changes are breaking and must be reflected in the migration guide.

## Synopsis

```text
gsm-sip-bridge --config <PATH> [--verbose] [-s <PATH> -a <ALSA_SPEC>]
gsm-sip-bridge --version
gsm-sip-bridge --help
```

The two debug binaries `gsm-echo` and `sip-echo` are produced by the same crate and follow the same flag conventions.

## Flags

| Flag | Long | Type | Default | Required | Description |
|---|---|---|---|---|---|
| `-c` | `--config` | `PathBuf` | — | Yes | Path to the TOML configuration file. |
| `-v` | `--verbose` | bool | `false` | No | Enables verbose SIP and AT command tracing in logs. Equivalent to `RUST_LOG=debug,gsm_sip_bridge=trace,pjsua_safe=debug`. |
| `-s` | `--serial` | `PathBuf` | — | No | Single-card override: the AT command serial port path (e.g. `/dev/ttyUSB3`). Must be paired with `--audio`. |
| `-a` | `--audio` | `String` | — | No | Single-card override: the ALSA device spec (e.g. `hw:2,0`). Must be paired with `--serial`. |
| | `--version` | bool | — | No | Print version and exit 0. |
| | `--help` | bool | — | No | Print help and exit 0. |

## Single-card override semantics

When BOTH `--serial` and `--audio` are present:
- USB auto-discovery is skipped.
- Exactly one Module is created with the supplied paths.
- The Module ID is derived from the USB serial of the device backing `--serial` if discoverable, else from a hash of the serial-port path.

When ONLY ONE of the two flags is present: the bridge refuses to start with an error naming the missing flag. Mixed mode (auto-discover + override one card) is not supported.

## Secret handling (FR-076)

The CLI MUST NOT accept any sensitive value as an argument. There is no `--password`, no `--webhook-url`, no `--token`. Operators supply secrets only via the config file (literal or `env:VAR_NAME`).

Attempting to add such a flag is a contract violation; the spec checklist enforces this.

## Environment variables

| Var | Purpose | Default if unset |
|---|---|---|
| `METRICS_PORT` | Override the metrics HTTP port. Wins over `[metrics].port` in config. | `9091` |
| `RUST_LOG` | Standard `tracing-subscriber` filter directive. | `info,gsm_sip_bridge=info` |
| User-defined `*_PASSWORD`, `*_TOKEN`, etc. | Resolved by `env:VAR_NAME` references in the config. | — |

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Clean shutdown (received SIGTERM/SIGINT and finished within the grace period). |
| `1` | Generic startup failure (e.g., config file missing, secret env var unset, no functional modules). |
| `2` | Invalid CLI usage (unknown flag, only one of `--serial`/`--audio`). Same as `clap`'s default. |
| `64` | Audio subsystem unrecoverable error during startup (ALSA could not open any device). |
| `65` | SIP subsystem unrecoverable error during startup (PJSIP `pjsua_init` failed). |
| `66` | Persisted store unrecoverable error during startup (DB file unreadable AND not initializable). |
| `137` (= 128+9) / `143` (= 128+15) | Killed by signal (standard shell convention; not produced by the bridge itself). |

The bridge guarantees `0` on graceful shutdown only. Any other exit code indicates the operator should consult logs.

## Help output (informative)

```text
gsm-sip-bridge 5.0.0
Bridges incoming GSM calls on Quectel EC20 modules to a SIP extension.

USAGE:
    gsm-sip-bridge --config <PATH> [OPTIONS]

OPTIONS:
    -c, --config <PATH>      Path to TOML configuration file
    -v, --verbose            Enable verbose SIP and AT command tracing
    -s, --serial <PATH>      Single-card override: AT command serial port
                             (must be paired with --audio)
    -a, --audio <SPEC>       Single-card override: ALSA device spec
                             (must be paired with --serial)
    -h, --help               Print help
    -V, --version            Print version

ENVIRONMENT:
    METRICS_PORT             Override the metrics HTTP port (default: 9091)
    RUST_LOG                 Standard tracing-subscriber filter

For configuration reference, see docs/configuration.md.
For the v4.1.x → v5.0.0 migration, see docs/migrating-from-v4.1.x.md.
```
