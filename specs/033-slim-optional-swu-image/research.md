# Phase 0 Research: Slim default image with optional SWu engine

All findings verified against the codebase and the live image
`ghcr.io/selvakn/gsm-sip-bridge:8.7.1` (119 MB).

## Baseline: where the 119 MB goes

`docker history` + on-disk `du`:

| Component | Size | Only-for-SWu? |
|---|---|---|
| Python interpreter (`/usr/lib/python3.12` + `libpython3.12.so`) | ~36 MB | yes |
| Python deps (`/opt/pydeps`: cryptography 15M, pycryptodome 9.4M, …) | ~34 MB | yes |
| SWu-IKEv2 dialer (`/opt/SWu-IKEv2`) | ~2.2 MB | yes |
| gsm-sip-bridge binary (already `strip=true`, `lto="thin"`) | 14.5 MB | no |
| Alpine base | 7.8 MB | no |
| strongSwan (charon + swanctl + plugins) | ~2.8 MB | no |
| bind-tools (`dig` + libdns/libisc) | ~3 MB | removable |
| net-tools (real `ifconfig`) | small | SWu-only |

**~72 MB (>60%) is Python+SWu.** Removing it from the default image is the win.

## R1 — Build-time SWu inclusion toggle

- **Decision**: One Dockerfile, `ARG INCLUDE_SWU=false`. The Python/SWu payload
  is assembled into a dedicated stage selected by a **stage-alias**:

  ```dockerfile
  # empty payload (no marker) when disabled
  FROM alpine:${ALPINE_VERSION} AS swu-false
  RUN mkdir -p /swu-root

  # real payload when enabled (depends on python-builder)
  FROM python-builder AS swu-true
  RUN mkdir -p /swu-root/opt \
   && cp -a /opt/SWu-IKEv2 /swu-root/opt/ \
   && cp -a /opt/pydeps    /swu-root/opt/ \
   && mkdir -p /swu-root/etc/gsm-sip-bridge \
   && touch /swu-root/etc/gsm-sip-bridge/swu-available

  ARG INCLUDE_SWU=false
  FROM swu-${INCLUDE_SWU} AS swu-payload
  ```

  Runtime stage then always `COPY --from=swu-payload /swu-root/ /`. Because
  BuildKit only builds stages in the selected target's dependency graph, the
  default build never builds `python-builder`/`swu-true` at all — no wasted work.

- **python3 apk package**: gated in the runtime stage with an ARG-conditioned
  `RUN`:
  `RUN if [ "$INCLUDE_SWU" = "true" ]; then apk add --no-cache python3; fi`
  (`ARG INCLUDE_SWU` re-declared in the runtime stage; ARGs don't cross stages).
  `ENV PYTHONPATH=/opt/pydeps` is harmless when the dir is absent, so it can stay
  unconditionally.

- **Rationale**: `COPY` cannot be conditional on an ARG, but choosing the *source
  stage* by ARG is the canonical BuildKit idiom and keeps a single Dockerfile
  (Principle V). Marker file (`/etc/gsm-sip-bridge/swu-available`) gives the
  binary a cheap, explicit variant signal (see R2).

- **Alternatives considered**: (a) two separate Dockerfiles — rejected,
  duplicates the whole runtime stage and drifts. (b) `target`-based multi-stage
  with two final stages — rejected, more surface than the alias trick for the
  same result.

## R2 — Runtime variant detection (FR-004 fail-fast)

- **Decision**: The full image ships a marker file
  `/etc/gsm-sip-bridge/swu-available` (created in `swu-true`). At startup, when
  `[vowifi].tunnel_engine == "swu"`, the bridge checks for the marker (and the
  dialer script) and, if absent, exits with a fatal error naming the `-swu`
  image. The check lives in `run()` **before line discovery**, not in
  `start_vowifi_subsystem`: it is a property of the *image*, not the line table.
  Behind line resolution it would miss two cases the review caught — a slim
  deployment with no modem attached (no lines ever resolve) and the
  discover-retry caller (which only logs, never exits) — either of which would
  leave the bridge running VoWiFi-less instead of failing. Hoisting it satisfies
  SC-005's "100% of the time".

- **Testability**: factor `fn swu_payload_available(marker: &Path, dialer:
  &Path) -> bool` (does the `.exists()` I/O but takes its paths by argument) and
  unit-test it against temp paths; `run()` passes the real
  `/etc/gsm-sip-bridge/swu-available` + `/opt/SWu-IKEv2/swu_emulator.py`. This
  mirrors the existing direct-`.exists()` checks (e.g. `/dev/net/tun`) while
  staying unit-testable.

- **Rationale**: A marker is unambiguous and independent of apk layout. Checking
  the dialer script too guards against a partially-built image.

- **Alternatives considered**: probing for the `python3` binary — rejected, a
  future unrelated python dependency would give a false positive. Compile-time
  cargo feature — rejected, would violate FR-005a (SWu code must stay compiled
  and tested in every build).

## R3 — Native DNS resolution (drop bind-tools)

- **Current**: `resolve_epdg_ip()` in `orchestrate.rs:900` runs
  `runner.run(&["dig", "+short", <fqdn>, "A"])` and takes the first
  dotted-quad line. Runs in the container's main netns, before per-line netns
  setup.

- **Decision**: Add `fn resolve_host(&self, host: &str) -> io::Result<Vec<Ipv4Addr>>`
  to the `CommandRunner` trait. Real impl: `(host, 0u16).to_socket_addrs()`
  (musl getaddrinfo, honors `/etc/resolv.conf`), keep only `V4` addresses.
  `resolve_epdg_ip` calls `runner.resolve_host(&fqdn)` and takes the first V4.
  Remove `bind-tools` from the runtime apk set.

- **Equivalence (FR-006) — approximate, not byte-for-byte**: both approaches
  return an A record for the FQDN, and any returned A record is a valid ePDG
  entry point, so the swap is safe. Recorded deltas (documented at the
  `resolve_host` seam):
  - **Order**: getaddrinfo is `AF_UNSPEC` with RFC 6724 sorting vs `dig`'s
    server order — with multiple A records the selected (first) address may
    differ.
  - **Literal IP**: a literal-IP `epdg_fqdn` now resolves to itself; `dig`
    returned nothing (queried it as a name). Benign improvement.
  - **Timeout**: bounded by the system resolver's `/etc/resolv.conf` timeout
    (~5s × 2 on musl) rather than a CLI flag — comparable to `dig`'s default.
  The override paths (`epdg_ip`, `epdg_fqdn`) are otherwise unchanged.

- **Testability**: the test `CommandRunner` double implements `resolve_host` to
  return canned addresses, so `resolve_epdg_ip` becomes directly unit-testable
  (it currently is not) — a net improvement, no new mock type.

- **Rationale**: getaddrinfo is already linked (musl); no new dependency. Keeps
  the `CommandRunner` seam (Principle I) so tests stay hermetic.

- **Alternatives considered**: a resolver crate (hickory-dns) — rejected, adds a
  dependency and its own resolv.conf parsing for no benefit. In-process
  `to_socket_addrs` directly at the call site without a seam — rejected, would
  make `resolve_epdg_ip` non-hermetic in tests.

## R4 — #3 runtime-package removals (verified)

Verified by grepping the vendored SWu dialer and image scripts:

Package removal only — the Alpine base's **busybox** provides `wget`, `route`,
and `ifconfig` applets that remain in both variants. Verified in the built
images (slim: `wget`/`route`/`ifconfig` all resolve to `/bin/busybox`; `dig`
missing). So only `dig` is fully gone; the rest is a package trim.

- **bind-tools** (`dig`): only consumer is `resolve_epdg_ip`. After R3, nothing
  uses it. charon's `--enable-resolve` writes `resolv.conf` itself, no `dig`.
  busybox has no `dig` applet, so `dig` is genuinely absent afterwards.
  → **Remove the package from both variants.**
- **net-tools**: load-bearing for the SWu dialer's **`/sbin/ifconfig`** call
  (`swu_emulator.py:1601,3176`), whose net-tools output format it parses —
  installing net-tools overwrites busybox's `/sbin/ifconfig` symlink with the
  real 56 KB binary (confirmed: real in full, busybox symlink in slim). Its
  `route add` calls (`:1550–1562`) resolve to busybox `/sbin/route` regardless,
  because net-tools ships `route` at `/bin/route`, which loses to `/sbin` in
  PATH — so `route` is NOT the reason. strongSwan/`supervise` use only `ip`
  (iproute2). → **Gate the package to the full image only** (install alongside
  python3 under the `INCLUDE_SWU` conditional).
- **wget**: no reference in SWu scripts or in `entrypoint.sh`/`supervise`; the
  only "wget" strings in the image are docstrings inside `/opt/pydeps`
  (requests/cffi metadata). Stage 1 (`pjsip-builder`) uses wget but that is a
  build stage, not runtime. → **Remove the package from both variants.** (The
  busybox `wget` applet remains, so this is a package trim, not a capability
  removal — no attack-surface claim.) Keep `ca-certificates` (tiny, low-risk;
  out of scope to remove).

## R5 — CI publishing (FR-008/FR-008a)

- **Regular release (`publish.yml`)**: the `bridge` matrix image already builds
  `docker/Dockerfile` with no `--build-arg`, so with `INCLUDE_SWU` defaulting to
  `false` it now produces the **slim** image under the existing tags
  (`{{version}}`, `{{major}}.{{minor}}`, sha) and `latest`. No structural change
  needed beyond confirming the default and updating the release-notes blurb. The
  `-internet` sidecar matrix entry is untouched.

- **On-demand full image (`publish-swu.yml`, NEW)**: `workflow_dispatch` with a
  `version` input (and a `v*-swu` tag trigger). Mirrors the bridge build/merge
  jobs but passes `build-args: INCLUDE_SWU=true` and pushes the **same image
  name** with tag `${version}-swu` (per-platform `${version}-swu-linux-amd64/arm64`
  then a merged `${version}-swu` manifest). Distinct GHA cache scope so it never
  clobbers the slim cache.

- **Provenance (Greptile P1)**: the build MUST come from the exact commit of the
  corresponding slim release, so `:${version}-swu` cannot diverge from
  `:${version}`. A version-number match against `Cargo.toml` is not enough —
  this repo leaves `Cargo.toml` at the released version until the next bump, so a
  branch HEAD (or an arbitrary `v*-swu` tag) can carry the same number while
  being different code. The `version` job therefore requires the `v${version}`
  release tag to exist and validates the `Cargo.toml` at *that tagged commit*;
  the `build` job checks out `refs/tags/v${version}` rather than the triggering
  ref. This replaced the earlier default-branch-only guard, which the tag-based
  build subsumes.

- **Rationale**: keeps the common release cheap and fast; the full image is
  built only when someone triggers it (FR-008a). Same-name-`-swu`-tag matches
  the clarified tagging decision.

- **Alternatives considered**: adding `INCLUDE_SWU=true` as a third matrix leg on
  every release — rejected, that publishes the full image on every release
  (violates FR-008). Separate image *name* — rejected by the clarification
  (Session 2026-08-10 chose same name + `-swu` suffix).

## R6 — SWu Rust code, tests, lint (FR-005a)

- **Finding**: There is **no cargo feature gating the SWu engine** — `SwuEngine`
  in `supervise/engines.rs` and its dispatch in `orchestrate.rs` compile
  unconditionally into `gsm-sip-bridge`. `make lint`/`make test` already cover
  the whole workspace including the SWu path.

- **Decision**: Change **nothing** here. The image variant differs only by
  payload/apk packages, never by what the Rust binary contains. This satisfies
  FR-005a with zero code change: SWu stays compiled and tested in both variants,
  and a regression in it fails CI regardless of which image is published.

- **Rationale**: the binary is identical across variants; only the *filesystem
  around it* changes. Simplest possible design (Principle V).

## Makefile

- **Decision**: Add `docker-build-swu` (`docker build -f docker/Dockerfile
  --build-arg INCLUDE_SWU=true -t gsm-sip-bridge:swu .`). Keep `docker-build`
  (slim, via compose) as the default. Required minimum targets untouched.
