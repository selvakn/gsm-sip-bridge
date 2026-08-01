# Option 2: Quectel as Internet Gateway + VoWiFi Call Routing

**Status:** Design doc for dual-purpose Quectel modem setup  
**Use case:** Single Quectel provides internet to network; VoWiFi handles all call signaling and media via ePDG tunnel  
**Rationale:** VoWiFi more stable than VoLTE with Airtel/Vodafone; decouples internet transport from call transport

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│ Host System Running Bridge                                       │
├─────────────────────────────────────────────────────────────────┤
│                                                                   │
│  Quectel Modem (Default Namespace)                               │
│  ├─ LTE connection to carrier                                    │
│  ├─ Internet/Data APN active                                     │
│  └─ Provides WWAN interface (eth1 or wwan0)                      │
│       │                                                           │
│       └──→ Bridge default namespace routing table                │
│            ├─ Host can reach: carrier network, ePDG gateways     │
│            └─ Host routes packets to downstream network devices  │
│                                                                   │
│  WiFi AP (Optional, backhauled by Quectel)                       │
│  └─ Clients connect here for VoWiFi access                       │
│                                                                   │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │ Network Namespace: ims (VoWiFi)                             │  │
│  ├────────────────────────────────────────────────────────────┤  │
│  │                                                              │  │
│  │  strongSwan daemon (charon)                                 │  │
│  │  └─ IPsec → ePDG tunnel (carrier's WiFi gateway)           │  │
│  │                                                              │  │
│  │  vowifi-carrier-agent (Agent A)                             │  │
│  │  ├─ IMS-AKA authentication                                  │  │
│  │  ├─ SIP REGISTER to carrier IMS core                        │  │
│  │  ├─ Answers incoming SIP INVITEs (carrier calling you)      │  │
│  │  ├─ RTP relay ←→ carrier media                              │  │
│  │  └─ Status port: localhost:5076                             │  │
│  │                                                              │  │
│  │  veth pair (xfrm0 side)                                      │  │
│  │  └─ Talks to Agent B in default namespace over TCP JSON     │  │
│  │                                                              │  │
│  └────────────────────────────────────────────────────────────┘  │
│       ↑                                                            │
│       │ veth pair (default namespace side)                        │
│       ↓                                                            │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │ Default Namespace (Telephony Bridge)                        │  │
│  ├────────────────────────────────────────────────────────────┤  │
│  │                                                              │  │
│  │  vowifi-sip-agent (Agent B)                                 │  │
│  │  ├─ PJSIP registration to PBX (one account)                │  │
│  │  ├─ Two-call conference bridging                            │  │
│  │  │  ├─ Leg 1: Incoming from VoWiFi (Agent A)               │  │
│  │  │  └─ Leg 2: Calls to/from PBX (endpoint)                │  │
│  │  ├─ RTP media bridge (transcoding codecs as needed)         │  │
│  │  └─ Codec handling:                                         │  │
│  │       ├─ Carrier → Agent A: AMR-WB (16 kHz) or PCMU        │  │
│  │       ├─ Agent A → Agent B: L16/16000 (uncompressed)       │  │
│  │       └─ Agent B → PBX: G.722 (16 kHz) or PCMU             │  │
│  │                                                              │  │
│  │  Bridge system can also route circuit-switched GSM calls    │  │
│  │  from other modems (if present) to PBX                      │  │
│  │                                                              │  │
│  └────────────────────────────────────────────────────────────┘  │
│                                                                   │
│  Downstream Network (via default namespace routing)              │
│  ├─ WiFi clients (VoWiFi callers)                               │
│  ├─ PBX / telephony equipment                                   │
│  └─ Other devices needing internet                              │
│                                                                   │
└─────────────────────────────────────────────────────────────────┘
```

## Data Flow

### Outgoing Call (Client → Carrier)

```
WiFi Client (SIP INVITE, RTP audio)
    ↓
Quectel internet APN (provides backhaul)
    ↓
Agent B (vowifi-sip-agent)
    ├─ Bridges to Agent A over veth
    └─ SIP INVITE → Agent A
        ↓
    ePDG tunnel (IPsec to carrier's WiFi gateway)
        ↓
    Carrier IMS Core
        ↓
    Destination phone (via carrier network)
```

### Incoming Call (Carrier → Client)

```
Carrier IMS Core
    ↓
ePDG tunnel (IPsec from carrier's WiFi gateway)
    ↓
Agent A (vowifi-carrier-agent)
    ├─ Receives SIP INVITE from carrier
    ├─ Relays RTP ← → carrier media
    └─ Signals to Agent B over veth JSON control channel
        ↓
    Agent B (vowifi-sip-agent)
        ├─ PJSIP two-call conference
        └─ Bridges to PBX or WiFi client
            ↓
        WiFi Client rings, user answers
```

### Internet Routing (All Devices)

```
Downstream device (any device on the network)
    ↓
Default namespace routing table
    ↓
Quectel WWAN interface (eth1/wwan0)
    ↓
Carrier internet APN
    ↓
Internet (e.g., example.com)
```

**Key:** Internet routing and VoWiFi calls are **completely decoupled**. Internet uses carrier's internet APN; VoWiFi uses ePDG tunnel (separate from internet APN).

---

## Configuration

### 1. Bridge Config File (`config.toml`)

```toml
# Enable VoWiFi only (disable VoLTE if previously enabled)
[vowifi]
enabled = true

# Per-line configuration
[[vowifi.lines]]
# Line 0: Primary SIM for VoWiFi
# You can override IMSI/IMEI if needed; otherwise detected from SIM

# Uncomment if using SIM with non-standard IMSI encoding:
# imsi_override = "310410123456789"
# imei_override = "123456789012345"

# Optional: PLMN override (normally auto-detected)
# plmn_override = "310410"  # Airtel India: MCC=310, MNC=410

# Certificate/auth config (if needed for your carrier's ePDG)
# epdg_cert_path = "/path/to/client.pem"
# epdg_key_path = "/path/to/client.key"

# Optional: ePDG server override (normally discovered via DNS SRV)
# epdg_server = "epdg.example.com"

# Codec preferences (optional, defaults shown)
# narrowband_codec = "PCMU"    # 8 kHz fallback for older carriers
# wideband_codec = "AMR-WB"    # 16 kHz preferred

[bridge.sip]
# PBX connection (Agent B listens here)
pbx_uri = "sip:bridge@pbx.example.com:5060"
pbx_user = "bridge"
pbx_password = "secret123"
pbx_proxy = "192.168.1.100:5060"

# Audio settings
preferred_codec = "G722"  # Bridge → PBX codec
audio_sample_rate = 16000

# Optional: enable SRTP for PBX leg if carrier requires it
# use_srtp = false

[logging]
level = "debug"
vowifi = "debug"   # Verbose VoWiFi logs
```

### 2. Network Setup

**Quectel WWAN Interface Configuration:**

```bash
# After Quectel connects, identify its interface:
ip link show  # Look for 'wwan0' or 'eth1'

# Ensure it has a carrier-assigned address:
ip addr show wwan0

# Example output:
# wwan0: <BROADCAST,MULTICAST,UP,LOWER_UP> ...
#   inet 203.0.113.42/24 brd 203.0.113.255 scope global dynamic

# Host can now reach carrier's ePDG via internet APN:
ping epdg.vodafone.co.in  # Should work if internet APN is active
```

**Route Internet to Downstream Devices (example):**

```bash
# Enable IP forwarding on host
sysctl -w net.ipv4.ip_forward=1

# If using WiFi AP for clients:
# Configure AP to use host as default gateway

# If using Ethernet to downstream network:
# Add route: ip route add 192.168.2.0/24 via 192.168.1.100

# Verify:
traceroute 8.8.8.8  # From downstream device, should route through host
```

---

## Why VoWiFi > VoLTE (Airtel/Vodafone Context)

### Stability Factors

| Factor | VoLTE | VoWiFi | Notes |
|--------|-------|--------|-------|
| **APN Independence** | IMS + internet APNs must coexist | ePDG tunnel independent of internet | VoWiFi doesn't compete for LTE resources |
| **Registration** | Modem IMS stack + bridge | Bridge-only (modem IMS disabled) | One registration point = fewer conflicts |
| **Codec Stability** | Carrier's modem-side codec stack | Bridge-controlled RTP | More predictable transcoding |
| **Failover** | Depends on modem firmware | Tunnel survives modem issues | ePDG tunnel can be more resilient |
| **IPv6 Handling** | Router Advertisement unicast quirk | ePDG tunnel sidesteps RA issues | No addr_gen_mode/link-local hacks needed |
| **Airtel/Vodafone bugs** | Seen: hourly registration churn | Seen: fewer auth renegotiations | Better observed uptime in production |

### Known Issues Avoided

- **VoLTE RA bug** (research.md R7): Carriers unicast Router Advertisements to derived link-local. VoWiFi's ePDG tunnel avoids this entirely.
- **Modem IMS conflict**: Only bridge owns IMS-AKA auth; modem's own stack is disabled. VoLTE requires both to coexist.
- **IMEI/IMSI override bugs**: VoWiFi handles overrides in bridge code; VoLTE requires modem AT command parsing.

---

## Deployment Steps

### Step 1: Prepare Quectel

```bash
# Power on Quectel, let it boot
# Verify it detected:
lsusb | grep Quectel

# Check serial port:
ls -la /dev/ttyUSB*  # Usually ttyUSB0 for DM, ttyUSB1 for AT
```

### Step 2: Enable Internet APN

```bash
# Connect to modem AT port (e.g., with minicom):
minicom -D /dev/ttyUSB1

# Verify SIM is ready:
AT+CPIN?
# Response: +CPIN: READY

# Check signal:
AT+CSQ
# Response: +CSQ: 19,0 (good signal)

# Set internet APN (example for Airtel India):
AT+CGDCONT=1,"IPV4V6","airtelwap.com"
# Response: OK

# Activate PDP context (internet):
AT+CGACT=1,1
# Response: OK

# Verify address assigned:
AT+CGPADDR=1
# Response: +CGPADDR: 1,"203.0.113.42" (carrier-assigned IP)

# Bring up interface in Linux (if not auto):
sudo ifup wwan0
# or:
sudo ip link set wwan0 up
```

**Note:** Do NOT activate IMS APN; VoWiFi doesn't use it (ePDG tunnel provides its own path).

### Step 3: Verify Internet Connectivity from Host

```bash
# Host can reach ePDG server (critical for VoWiFi):
ping epdg.vodafone.co.in

# Can reach public internet:
ping 8.8.8.8

# Can route to downstream devices (test from your PC):
ping <host-ip>  # Should work
traceroute 8.8.8.8  # Should route via host's Quectel interface
```

### Step 4: Start Bridge

```bash
# Option A: Docker (if using image)
docker run -d \
  --name gsm-sip-bridge \
  --device /dev/ttyUSB1 \
  --network host \
  -v /home/user/config.toml:/etc/gsm-sip-bridge/config.toml \
  gsm-sip-bridge:latest

# Option B: Native (if building locally)
cd /path/to/gsm-sip-bridge
cargo build --release
./target/release/gsm-sip-bridge --config config.toml

# Option C: systemd service (if installed)
sudo systemctl start gsm-sip-bridge
```

### Step 5: Verify Bridge is Running

```bash
# Check logs:
journalctl -u gsm-sip-bridge -f
# Or from Docker:
docker logs -f gsm-sip-bridge

# Verify VoWiFi initialization:
# Logs should show:
# - "Discovering VoWiFi lines..."
# - "Found SIM with IMSI=31041XXXXXXXXXX"
# - "Starting ePDG tunnel to carrier"
# - "IMS registration: REGISTER sent"
# - "IMS registration: 200 OK"
# - "vowifi-sip-agent bound to localhost:5074"

# Check VoWiFi status (Agent A):
curl http://localhost:5076/status
# Response: {"registered": true, "impu": "sip:...", "next_renewal": 3600, ...}

# Check PJSIP registration (Agent B):
# Bridge will log: "PBX endpoint registered: sip:bridge@..."
```

### Step 6: Test VoWiFi Calling

**Incoming Call Test:**

```bash
# From another phone calling your VoWiFi SIP address
# (or call your PBX extension)

# Bridge should log:
# - "Incoming SIP INVITE from carrier ePDG"
# - "Bridging to PBX leg"
# - "Audio flowing A→B and B→A"

# Check RTP statistics:
# You can query Agent A status port for RTP counters:
curl http://localhost:5076/stats | jq .rtp
```

**Outgoing Call Test:**

```bash
# Place call from WiFi client or PBX extension
# Bridge should log:
# - "SIP INVITE from PBX → Agent B"
# - "Forwarding to Agent A (carrier leg)"
# - "Sending to carrier via ePDG"

# Verify audio is flowing
```

---

## Codec Handling

VoWiFi bridges between three codec domains. Understanding this is key to audio quality:

### Flow Example: Carrier AMR-WB → PBX G.722

```
Carrier sends:     AMR-WB, 16 kHz, 12.65 kbps
    ↓
Agent A receives & relays (no re-encoding)
    ↓
veth to Agent B    Payload codec passed as-is
    ↓
Agent B transcodes AMR-WB → L16 → G.722 (for PBX)
    ↓
PBX receives       G.722, 16 kHz, 64 kbps
```

### Carrier Codec Selection

- **Airtel India:** Prefers AMR-WB (wideband, better quality)
- **Vodafone India:** Prefers AMR-WB, falls back to PCMU (narrowband)
- **Check your carrier:**

```bash
# Monitor bridge logs during registration:
grep "Codec:" logs/vowifi.log

# Or query Agent A status:
curl http://localhost:5076/status | jq .codec
```

### Fallback Strategy

If carrier doesn't support wideband:

```toml
[vowifi]
narrowband_codec = "PCMU"  # 8 kHz, older but more compatible
wideband_codec = "AMR-WB"  # 16 kHz, preferred
```

---

## Troubleshooting

### Issue: VoWiFi Registration Fails

**Symptoms:**  
```
vowifi-carrier-agent: IMS registration failed: 403 Forbidden
```

**Diagnosis:**
1. Check SIM is activated for VoWiFi on carrier account (some carriers require this)
2. Verify certificate/IMEI not blocked by carrier
3. Check IMEI/IMSI:

```bash
# Query from bridge logs or Agent A status:
curl http://localhost:5076/status | jq '.imei, .imsi'

# Verify certificate chain if using mTLS:
openssl s_client -connect epdg.vodafone.co.in:500 -cert client.pem -key client.key
```

**Fix:**
- Contact carrier to ensure SIM is VoWiFi-enabled
- Verify correct ePDG server (`epdg.vodafone.co.in` vs. `epdg.airtel.in`, etc.)
- Check firewall allows UDP 500 (IKE) and 4500 (IPsec NAT-T)

---

### Issue: One-Way Audio

**Symptoms:**  
You can hear incoming caller, but they can't hear you.

**Diagnosis:**
This is usually an Agent A → Agent B RTP stream issue.

```bash
# Check Agent A is receiving RTP from carrier:
curl http://localhost:5076/stats | jq '.rtp_in'

# Check veth link is up:
ip netns exec ims ip link show veth_ims_host

# Check bridge logs for codec mismatch:
grep "Codec mismatch" logs/bridge.log
```

**Fix:**
- Ensure Agent B's codec list includes what Agent A is sending
- Check veth pair isn't down: `ip netns exec ims ip link set veth_ims_host up`
- Restart bridge: systemctl restart gsm-sip-bridge

---

### Issue: Internet Routes but VoWiFi Doesn't Connect

**Symptoms:**  
```
ping 8.8.8.8  # Works
curl http://localhost:5076/status  # Times out or 403
```

**Diagnosis:**
ePDG tunnel is not active.

```bash
# Check strongSwan status:
sudo swanctl --stats
# Should show active IKE_SA and CHILD_SA

# Check XFRM interface:
ip netns exec ims ip link show xfrm0
# Should be UP

# Check IPsec policy:
sudo ip xfrm policy list

# Verify internet APN is active (necessary for ePDG reach):
ip route show default
# Should show via Quectel interface
```

**Fix:**
- Verify internet APN is active on Quectel
- Check firewall allows:
  - UDP 500 (IKE)
  - UDP 4500 (IPsec)
  - ESP protocol (IP 50)
- Restart VoWiFi: `systemctl restart gsm-sip-bridge`

---

### Issue: Inbound Calls Don't Ring

**Symptoms:**  
Incoming call silently dropped, no log entry.

**Diagnosis:**
```bash
# Check Agent B's PBX registration:
grep "PBX endpoint" logs/bridge.log

# Verify PBX can route to bridge:
sip-ping sip:bridge@pbx.example.com  # Should get 200 OK
```

**Fix:**
- Ensure PBX has route for `bridge` extension
- Check PBX firewall allows SIP from bridge IP
- Verify `[bridge.sip]` config has correct PBX credentials

---

## Monitoring

### Key Metrics to Watch

1. **VoWiFi Registration Health:**
   ```bash
   watch -n 5 'curl -s http://localhost:5076/status | jq "{registered, impu, next_renewal}"'
   ```

2. **Call Statistics:**
   ```bash
   curl http://localhost:5076/stats | jq '{calls_active, total_calls, uptime_sec}'
   ```

3. **Network Path:**
   ```bash
   # From host:
   ping -c 1 epdg.vodafone.co.in
   
   # From ims namespace:
   ip netns exec ims ping -c 1 epdg.vodafone.co.in
   ```

4. **RTP Quality:**
   ```bash
   curl http://localhost:5076/stats | jq '.rtp | {packets_sent, packets_lost, jitter_ms}'
   ```

### Alerting

Log these events for monitoring:

```
ERROR: IMS registration failed
WARN: IMS registration renewal failed (will retry)
ERROR: Agent A crash detected
ERROR: Agent B PBX registration failed
WARN: One-way audio (RTP mismatch)
```

---

## Upgrading Bridge

VoWiFi subsystem is decoupled from circuit-switched and VoLTE, so upgrades are low-risk:

```bash
# Backup current config:
cp config.toml config.toml.bak

# Update bridge code:
git pull origin main
cargo build --release

# Restart:
systemctl restart gsm-sip-bridge

# Verify VoWiFi reconnected:
sleep 5 && curl http://localhost:5076/status | jq '.registered'
# Should return: true
```

**No downtime for in-flight calls if:**
- PBX endpoint re-registers cleanly
- PJSIP conference state is preserved
- (In practice: ~2-3 sec call drop during restart)

---

## FAQ

**Q: Can I use the same Quectel for internet + circuit-switched calls too?**

A: Yes. This doc covers VoWiFi only, but you can add circuit-switched (GSM) calls on the same modem. Quectel supports simultaneous GSM/LTE. The bridge will bridge both GSM and VoWiFi calls to your PBX.

**Q: What if carrier network is down but WiFi is up (WiFi-only clients)?**

A: VoWiFi calls will fail because ePDG tunnel requires internet connectivity to reach carrier's gateway. WiFi AP is just the access medium; ePDG still needs carrier's network for SIP/RTP.

**Q: Can I use the same SIM for both VoWiFi and VoLTE?**

A: Not simultaneously (see architecture notes: mutual exclusion check). Pick one. Given your experience, choose VoWiFi.

**Q: What happens if Quectel reboots mid-call?**

A: Internet APN will drop, causing ePDG tunnel to collapse. In-flight VoWiFi call will drop. Quectel will restart, internet APN will come back up, VoWiFi will re-register automatically (within ~30s).

**Q: How do I ensure failover if Quectel fails?**

A: This setup is single-Quectel. For redundancy, add a second modem (EC20 or another Quectel) with VoLTE. Or use a separate WAN link (Ethernet, other carrier). Document: specs/020 covers multi-modem failover.

---

## Files Referenced

- **Bridge config:** `/home/selva/projects/ec20/gsm-sip-bridge/config.toml`
- **VoWiFi code:** `src/vowifi/mod.rs`, `src/vowifi/control.rs`
- **IMS auth/tunnel:** `src/ims/agent.rs`, `src/ims/gm_ipsec.rs`
- **Specs:** `specs/011-vowifi-sip-bridge/`, `specs/013-vowifi-multi-line/`
- **Architecture:** `docs/architecture.md`, `docs/vowifi-bridge.md`
- **Research:** `docs/research.md` (IPv6 RA quirks, ePDG design rationale)

---

## Next Steps

1. **Gather carrier details:** ePDG server, ePDG protocol version (IKEv2 vs. legacy), cert requirements
2. **Provision SIM:** Contact carrier to enable VoWiFi on SIM (some carriers need this explicitly)
3. **Deploy Quectel:** Connect hardware, test internet APN
4. **Configure bridge:** Create `config.toml` with carrier's ePDG server and PBX details
5. **Start bridge:** `systemctl start gsm-sip-bridge`, monitor logs
6. **Test calls:** Inbound and outbound from WiFi clients and PBX

---

**Document Status:** Ready for deployment  
**Last Updated:** 2026-07-25  
**Author:** Claude (based on bridge architecture exploration)
