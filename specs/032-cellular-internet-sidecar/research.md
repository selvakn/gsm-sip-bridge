# Phase 0 Research: Cellular-internet sidecar

All spec Clarifications were resolved up front (see spec → Clarifications →
Session 2026-08-10). This file records the remaining implementation-level
decisions the plan deferred to planning.

## R1 — QMI dialer: `libqmi` (qmicli) vs quectel-CM vs ModemManager

- **Decision**: Use **`libqmi` (`qmicli`) + BusyBox `udhcpc`** as the dialer,
  driving the modem's `qmi_wwan` netdev in **raw-ip** mode.
- **Rationale**: `libqmi` is a packaged, open-source Alpine dependency (`apk add
  libqmi`), keeping the image small and reproducible. `qmicli
  --wds-start-network` + `udhcpc` is the well-trodden open path for Qualcomm
  modems and needs no out-of-tree build. It uses **only** `/dev/cdc-wdm0`,
  guaranteeing the modem's AT port stays free for the bridge's `AT+CSIM`
  (spec FR-002).
- **Alternatives considered**:
  - *quectel-CM* — Quectel's own C connection manager. Handles dial+DHCP+raw-ip
    framing cleanly, but is an out-of-tree source build (heavier image, extra
    build stage) for no capability the qmicli path lacks here. Kept as a
    documented fallback only.
  - *ModemManager* — full-featured but heavyweight (D-Bus, polls many ports,
    may grab the AT port and contend with the bridge). Rejected: violates the
    "lightweight" + "AT port free" constraints.

## R2 — raw-ip + IP addressing (IPv4 vs IPv6)

- **Decision**: Enable `qmi_wwan` **raw-ip**, dial the internet APN, obtain
  **IPv4** via `udhcpc` on the `wwan*` interface, install the default route.
  Request `IPV4V6` on the context but treat IPv4 as the connectivity that gates
  health; IPv6, if the carrier grants it, is best-effort.
- **Rationale**: Internet APNs on Indian carriers reliably hand out IPv4 (unlike
  the IMS-only IPv6 PDN that made VoLTE's `netcfg.rs` painful). IPv4 + DHCP is
  the simplest correct path and matches the reachability probe (DNS over v4).
- **Alternatives considered**: IPv6-primary (as VoLTE does) — rejected here; the
  RA/`addr_gen_mode` gymnastics in `src/volte/netcfg.rs` are specific to the
  IMS PDN and unnecessary for a normal internet APN.

## R3 — Reachability probe implementation

- **Decision** (from spec): a **DNS resolution** of an operator-configurable
  hostname via an operator-configurable public resolver, executed through the
  cellular link. Implemented with BusyBox `nslookup <host> <resolver>` (fall
  back to `getent hosts`), exit 0 on a resolved answer, nonzero otherwise. The
  script also writes a one-line status to `/run/internet-status`.
- **Rationale**: survives carriers/APNs that block ICMP; proves routing **and**
  name resolution, which the PBX/ePDG hostnames actually need. No HTTP dependency.
- **Defaults**: host `one.one.one.one` (or `dns.google`), resolver `1.1.1.1`
  (fall back `8.8.8.8`) — both operator-overridable so locked-down carrier
  networks can point at a reachable target.
- **Alternatives considered**: ICMP ping (false negatives under ICMP blocking);
  HTTP 204 captive check (heavier, external HTTP dependency). Both rejected.

## R4 — Healthcheck wiring & timing

- **Decision**: Docker `HEALTHCHECK`/compose healthcheck runs
  `internet-healthcheck.sh` with `start_period: 90s`, `interval: 10s`,
  `timeout: 5s`, `retries: 1` (a single failed 10s probe flips unhealthy quickly;
  the 90s grace absorbs a slow first attach).
- **Rationale**: matches spec Q2 (90s grace, 10s interval); `depends_on:
  condition: service_healthy` then blocks the bridge until the first success.
- **Alternatives considered**: longer retries/backoff — unnecessary given the
  bridge waits indefinitely anyway and self-heal handles mid-run drops.

## R5 — Opt-in wiring: Compose **override file** vs `profiles`

- **Decision**: Ship the sidecar and the bridge's `depends_on` in a **separate
  override file** `docker/docker-compose.cellular-internet.yml`, included only
  when the operator wants same-card internet:
  `docker compose -f docker/docker-compose.yml -f docker/docker-compose.cellular-internet.yml up`.
- **Rationale**: guarantees **default off** and no dependency in the base file
  (spec FR-005 / SC-003). A Compose **profile** cannot conditionally *inject* a
  `depends_on` into the always-on bridge service, and a non-profiled service that
  `depends_on` a profiled one drags the dependency in implicitly — defeating
  "default off." The override cleanly merges the `depends_on` onto the bridge
  and adds the service, all in one opt-in file.
- **Alternatives considered**: `profiles: [cellular-internet]` on an `internet`
  service — rejected for the injection/implicit-start reasons above. A
  bridge-entrypoint poll (Option B from clarification) — rejected by the chosen
  answer (Compose healthcheck + depends_on).

## R6 — Networking model: host networking (shared default namespace)

- **Decision**: The sidecar runs `network_mode: host` + `privileged` (like every
  service in the stack), manages the host's `qmi_wwan` interface and default
  route, and the bridge (also host-networked) simply uses that default route.
- **Rationale**: The whole stack is already host-networked and privileged
  (docker-compose.yml header). The VoWiFi ePDG tunnel auto-selects its outer
  transport from the host routing table (`docker/strongswan/swanctl-epdg.conf.template`),
  so once the sidecar installs the default route the tunnel rides it with zero
  bridge changes. Both containers touch the same physical modem via **different**
  device nodes (sidecar `/dev/cdc-wdm0`, bridge `/dev/ttyUSB*`) through the
  existing `/dev:/dev` mount — no contention.
- **Alternatives considered**: `network_mode: service:internet` for the bridge —
  rejected; the bridge needs host scope for USB hot-plug discovery and its own
  per-line netns/veth/XFRM surgery, which a shared container netns complicates.

## R7 — Self-heal on drop (FR-007)

- **Decision**: The entrypoint runs a supervise loop: after the initial dial it
  polls link/session state (and reuses the same DNS probe); on loss it tears down
  and re-runs `wds-start-network` + `udhcpc`, updating the status file. On modem
  re-enumeration it waits for `/dev/cdc-wdm0` to reappear, then re-dials.
- **Rationale**: gives Story 2 (independent lifecycle, self-heal) without any
  external orchestration; `restart: unless-stopped` covers hard process death.

## R8 — Sidecar-local observability only (FR-008, spec Q1)

- **Decision**: Status is container **logs** + a readable **status file**
  (`/run/internet-status`, surfaced by the healthcheck). **No** Prometheus metric,
  **no** Discord alert, no dependency on the bridge's observability stack.
- **Rationale**: an internet-down alert can't traverse the down link anyway, and
  the sidecar is kept intentionally lightweight (operator's explicit call).

## R9 — Lint/build integration (Constitution IV)

- **Decision**: Place scripts in `docker/cellular-internet/` and extend `make
  lint` so `shellcheck -x` covers `docker/cellular-internet/*.sh` in addition to
  `docker/*.sh`; add a `docker-build-internet` Makefile target.
- **Rationale**: keeps the constitution's shellcheck gate meaningful for the new
  scripts and keeps all build/lint operations behind `make`.
