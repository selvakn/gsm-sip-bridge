# CLI Interface Contract: SIP Audio Echo Server

**Binary**: `sip-echo`

## Options

| Flag | Long Form | Argument | Description |
|------|-----------|----------|-------------|
| -c | --config | PATH | Path to INI configuration file (default: `config.ini` in working directory) |
| -v | --verbose | - | Enable verbose SIP/media logging |
| -h | --help | - | Print usage and exit |
| | --version | - | Print version and exit |

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Clean shutdown (SIGINT/SIGTERM) |
| 1 | Configuration file not found or unreadable |
| 2 | Configuration validation error (missing required field) |
| 3 | SIP registration failed after all retries |
| 4 | PJSIP library initialization error |

## Log Format

All log lines are written to stdout with the format:

```
YYYY-MM-DDTHH:MM:SS.sss LEVEL message
```

Where LEVEL is one of: INFO, WARN, ERROR.

## Significant Log Events

| Event | Level | Example |
|-------|-------|---------|
| Startup | INFO | `sip-echo v0.1.0 starting` |
| Config loaded | INFO | `config loaded from config.ini` |
| Registering | INFO | `registering as echo-test@pbx.example.com` |
| Registered | INFO | `SIP registration successful` |
| Incoming call | INFO | `incoming call from sip:user@remote.com` |
| Call answered | INFO | `call answered, echo active` |
| Call ended | INFO | `call ended (duration: 12s)` |
| Registration lost | WARN | `SIP registration lost, re-registering` |
| Shutdown | INFO | `shutting down, de-registering` |
| Config error | ERROR | `config error: missing required field 'server'` |
