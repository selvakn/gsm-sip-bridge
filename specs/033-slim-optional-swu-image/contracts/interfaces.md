# Phase 1 Contracts: Slim default image with optional SWu engine

The interfaces this feature exposes/changes. Each has an acceptance check.

## C1 — Docker build contract

- **Input**: `docker build -f docker/Dockerfile [--build-arg INCLUDE_SWU=<bool>] .`
- **Default** (`INCLUDE_SWU` unset or `false`): produces the **slim** image.
  - MUST NOT contain `python3`, `/opt/pydeps`, `/opt/SWu-IKEv2/swu_emulator.py`,
    or `/etc/gsm-sip-bridge/swu-available` (note: `WORKDIR` creates an empty
    `/opt/SWu-IKEv2` dir even in slim — assert the dialer file, not the dir).
  - MUST NOT have the `net-tools` apk package installed.
  - MUST run the strongSwan VoWiFi path unchanged.
  - Size ≤ ~50 MB.
- **`INCLUDE_SWU=true`**: produces the **full** image.
  - MUST contain `python3`, `/opt/pydeps`, `/opt/SWu-IKEv2/swu_emulator.py`, and
    the `swu-available` marker; MUST also have the `net-tools` apk package
    (which installs the real `/sbin/ifconfig` the SWu dialer parses).
  - Strict superset of slim.
- **Both**: MUST NOT install the `bind-tools` or `wget` apk packages, and `dig`
  MUST NOT be present as a command. (Busybox applets `wget`/`route`/`ifconfig`
  remain in the base image in both variants — out of scope; assertions target
  apk packages via `apk info -e <pkg>`, plus `command -v dig` for `dig`.)

**Acceptance**: `docker build` both ways; assert apk-package presence with
`apk info -e <pkg>` and file/command presence via
`docker run --rm --entrypoint sh <img> -c '...'`; assert slim size.

## C2 — Image tag contract

- Slim image published under `:{{version}}`, `:{{major}}.{{minor}}`, `:latest`,
  and `:sha-…` on the regular release.
- Full image published **only** on demand under `:{{version}}-swu`, same image
  name.
- A normal release MUST NOT publish any `-swu` tag. Because `publish.yml`'s
  `tags: ["v*"]` also matches a `v<version>-swu` tag, all three of its jobs are
  guarded with `if: ${{ !endsWith(github.ref_name, '-swu') }}`, so a `-swu`
  tag push runs only `publish-swu.yml`.

**Acceptance**: inspect `publish.yml` (no `INCLUDE_SWU=true` leg; `-swu`-tag
guard on every job) and `publish-swu.yml` (pushes `-swu` tag via
`workflow_dispatch` or a `v*-swu` tag).

## C3 — `CommandRunner::resolve_host` contract

```rust
/// Resolve `host` to its IPv4 addresses via the system resolver.
/// Returns Ok(vec) (possibly empty if no A record) or Err on resolver failure.
fn resolve_host(&self, host: &str) -> std::io::Result<Vec<std::net::Ipv4Addr>>;
```

- Real impl: `(host, 0u16).to_socket_addrs()`, retain `SocketAddr::V4`.
- Test impl: returns canned addresses keyed by host (no network).
- `resolve_epdg_ip` selects the first returned `Ipv4Addr`.

**Acceptance**: unit test `resolve_epdg_ip` with a stub `resolve_host` returning
`[203.0.113.5, 203.0.113.6]` ⇒ picks `203.0.113.5`; empty ⇒ `None`; and the
`epdg_ip`/`epdg_fqdn` override branches still short-circuit correctly.

## C4 — SWu-on-slim fail-fast contract

- **Given** `[vowifi].enabled = true` and `[vowifi].tunnel_engine = "swu"` on a
  slim image (no `swu-available` marker),
- **When** `supervise` starts,
- **Then** it exits with a fatal error **before line discovery** — hence before
  spawning any process and regardless of whether any modem/line is present —
  whose message states SWu is unavailable in this image and names the `-swu`
  image tag.
- On the full image (marker present) the `swu` engine starts as today.
- The check is an **image** property, so it lives in `run()` ahead of discovery,
  not in `start_vowifi_subsystem` (which sits behind line resolution and, on the
  discover-retry path, only logs) — this is what makes SC-005's "100% of the
  time" hold.

**Acceptance**: unit-test `swu_payload_available(marker, dialer)` for
present/absent against temp files; `run()` uses it with the real image paths and
the shared `swu_unavailable_message()`, exiting `ExitCode::FAILURE` before the
`discover` call when the engine is `swu` and the payload is absent. Live-verified
in the built slim image (message present in the binary; payload absent so the
guard fires).

## C5 — SWu code/test/lint invariant (FR-005a)

- The `gsm-sip-bridge` binary MUST contain the `SwuEngine` code in **both**
  variants (no cargo feature gate).
- `make test` and `make lint` MUST continue to compile and cover the SWu path.

**Acceptance**: `grep` confirms no `#[cfg(feature = ...)]` gates `SwuEngine`;
`make test`/`make lint` green with SWu code present.
