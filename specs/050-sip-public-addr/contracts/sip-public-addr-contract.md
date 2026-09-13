# Contract: `[sip].public_addr` config field and its effect on SIP/SDP

This bridge's external interfaces touched by this feature are (1) the
`[sip]` section of its config file, and (2) the SIP signaling / SDP it sends
on its PBX/softphone-facing transport. Each row is independently verifiable
— by starting the bridge with the described config and inspecting either
its startup outcome or a live call's SIP/SDP — with no knowledge of internal
implementation required.

## Config-load behavior

| `[sip].public_addr` value | Startup outcome |
|---|---|
| Absent (field omitted) | Starts normally; behavior identical to before this feature |
| A valid IPv4 literal (e.g. `100.111.26.23`) | Starts normally; no DNS resolution performed |
| A valid IPv6 literal (e.g. `::1`) | Fails to start; error names `sip.public_addr` and explains the SIP transport is IPv4-only |
| A hostname that resolves to at least one IPv4 address (e.g. a Tailscale MagicDNS name) | Starts normally; resolved exactly once during startup, the first IPv4 result used |
| A hostname that resolves only to IPv6 addresses | Fails to start; treated the same as a hostname that fails to resolve at all |
| A hostname that fails to resolve (NXDOMAIN, resolver error) | Fails to start; error names `sip.public_addr` and the offending value |
| An empty string or otherwise unparseable value | Fails to start; error names `sip.public_addr` and the offending value |

## SIP/SDP behavior (once running)

| Configuration | SIP Contact/Via | SDP `c=` line (outbound calls this bridge places) | SDP `c=` line (inbound calls this bridge answers, SIP-server mode) |
|---|---|---|---|
| `public_addr` unset | PJSIP's own address discovery (`pj_gethostip()`, unaffected by this feature) — same as today | Same as today | Same as today, **including** after `set_identity` rewrites caller ID for an inbound call |
| `public_addr` set (IP or resolved hostname) | The configured address | The configured address | The configured address, **and still** the configured address after `set_identity` rewrites caller ID for that same call (this is the regression class GitHub issue #77's root-cause analysis identifies — `set_identity` must not silently drop it) |

## Non-effects (explicitly unchanged by this feature)

| Component | Behavior |
|---|---|
| Internal veth-linked leg between Agent A and Agent B (`src/ims/agent/veth.rs`) | RTP destination continues to come from the peer socket address, never the SDP `c=` line — `public_addr` has no effect here, configured or not |
| Carrier-facing IMS/Gm-interface leg (`src/ims/agent`, VoWiFi/VoLTE registration to Jio/Airtel) | No `pjsua-safe::Endpoint`/`Account` involvement at all — `public_addr` has no effect here, configured or not |
| A deployment leaving `public_addr` unset | Zero behavior change from before this feature, including the existing `PJ_GETHOSTIP_DISABLE_LOCAL_RESOLUTION` fix (`2a04eae`) remaining fully in effect |
