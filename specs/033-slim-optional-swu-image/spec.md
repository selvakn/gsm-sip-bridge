# Feature Specification: Slim default image with optional SWu engine

**Feature Branch**: `033-slim-optional-swu-image`
**Created**: 2026-08-10
**Status**: Draft
**Input**: User description: "plan to implement the ARG INCLUDE_SWU split #1(b) and #3"

## Overview

The container image grew from ~26 MB to ~119 MB when the VoWiFi subsystem was
introduced. Over 60% of the current image (~72 MB) is the Python interpreter,
Python dependency tree, and the vendored SWu-IKEv2 dialer — all of which exist
solely to serve the `swu` tunnel engine. That engine is now a **legacy
fallback**: the default engine is `strongswan`, which needs no Python at all.

This feature makes the SWu/Python engine an **opt-in build variant** so the
image most operators pull is slim, while preserving the SWu fallback for
deployments that still depend on it. It also removes runtime tooling that is no
longer needed (a DNS client utility, legacy networking tools, and a download
utility) so both variants shrink.

The SWu engine is **not being removed from the product**. Its code, config
handling, tests, and build path all stay in the repository and continue to be
exercised by the standard build, unit-test, lint, and format pipeline exactly
as today. Only the *default published image* drops the SWu payload. The full
SWu image is **built and published on demand through a separate pipeline, only
when a deployment actually needs it** — it is not published on every release
alongside the slim image.

## Clarifications

### Session 2026-08-10

- Q: How should the slim and full/SWu images be named/tagged, and what should
  today's existing tags (`:X.Y.Z`, `:latest`) resolve to after this change? →
  A: The slim image inherits the canonical image name and the existing
  version/`latest` tags (so `:X.Y.Z` and `:latest` become the slim image). The
  full/SWu image is published on demand under the **same image name** with a
  `-swu` tag suffix (e.g. `:X.Y.Z-swu`). Consumers who need SWu opt in
  explicitly; consumers on floating tags move to slim (and fail fast per FR-004
  if they had configured the `swu` engine).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Slim default image without the legacy engine (Priority: P1)

An operator deploying the bridge with the default (`strongswan`) tunnel engine
pulls the published image and gets a substantially smaller download that
contains only what the strongSwan path actually uses — no Python interpreter,
no Python dependency tree, no vendored SWu dialer.

**Why this priority**: This is the primary value — it eliminates the majority
of the image size for the common, default deployment path, reducing pull time,
storage, and attack surface. It stands alone as a shippable improvement.

**Independent Test**: Build the default image, confirm it runs the VoWiFi
subsystem end-to-end with `tunnel_engine = "strongswan"`, and confirm its size
is roughly half the current image and that no Python interpreter or SWu dialer
is present inside it.

**Acceptance Scenarios**:

1. **Given** the default build with no extra build arguments, **When** the image
   is built and inspected, **Then** it contains no Python interpreter, no Python
   dependency directory, and no SWu-IKEv2 dialer, and its size is materially
   smaller than the current image.
2. **Given** the default image and a config with `tunnel_engine = "strongswan"`,
   **When** the container starts against attached SIM hardware, **Then** the
   VoWiFi tunnel comes up and calls behave exactly as they do today.
3. **Given** the default image, **When** an operator configures
   `tunnel_engine = "swu"`, **Then** the container fails fast at startup with a
   clear message that the SWu engine is not available in this image variant and
   names the variant that provides it.

---

### User Story 2 - Full image built on demand, SWu code kept in the tree (Priority: P2)

An operator who still depends on the `swu` engine (e.g. because strongSwan is
not yet proven against their carrier) can build the "full" image variant that
bundles the Python interpreter, dependencies, and SWu dialer exactly as today,
and it behaves identically to the current image. This full image is produced
**on demand through a separate publishing pipeline, only when it is actually
needed** — it is not published on every release. Meanwhile the SWu engine's
source, configuration, and tests remain in the repository and keep passing the
standard test/lint/format checks, so the fallback never silently rots.

**Why this priority**: Preserves the existing escape hatch so the slimming
change carries no functional risk for deployments that still need SWu, and keeps
the SWu code path continuously verified without paying for it on every release
build. Required for the P1 change to be safe to ship, but secondary to
delivering the slim default.

**Independent Test**: With SWu code untouched in the tree, run the standard
unit-test/lint/format pipeline and confirm the SWu path is still covered and
green; separately, trigger the on-demand full-image build, run it with
`tunnel_engine = "swu"`, and confirm SWu tunnel establishment works exactly as
it does in the current image.

**Acceptance Scenarios**:

1. **Given** the repository after this change, **When** the standard
   unit-test, lint, and format pipeline runs, **Then** the SWu engine's code and
   tests are included and pass exactly as before — nothing about the SWu path is
   deleted or excluded from CI checks.
2. **Given** an on-demand full-image build, **When** the image is built and
   inspected, **Then** it contains the Python interpreter, the Python dependency
   tree, and the SWu-IKEv2 dialer.
3. **Given** the full image and a config with `tunnel_engine = "swu"`, **When**
   the container starts against attached SIM hardware, **Then** the SWu tunnel
   comes up and calls behave exactly as they do today.
4. **Given** the full image and a config with `tunnel_engine = "strongswan"`,
   **When** the container starts, **Then** the strongSwan path works exactly as
   in the slim image (the full image is a strict superset).
5. **Given** a normal release, **When** CI publishes images, **Then** it
   publishes the slim image and does **not** publish the full/SWu image; the
   full image is published only when the separate on-demand pipeline is
   triggered.

---

### User Story 3 - Drop unused runtime tooling from both variants (Priority: P3)

Runtime **packages** that exist only to support shelled-out helpers — the DNS
client package used for a single lookup, and a download package not referenced
by any runtime code — are removed from both variants, and the legacy networking
package is confined to the full image (only the SWu dialer needs it). The DNS
lookup the bridge performs is handled without depending on an external DNS
client program.

Scope note: this removes standalone apk *packages*, not the base image's
busybox applets. The `dig` command has no busybox applet, so it is genuinely
gone; `wget`, `route`, and `ifconfig` remain available as busybox applets in
both variants (the full image additionally installs the real `ifconfig` binary
that the SWu dialer parses). The value is a smaller package set, not the removal
of those commands.

**Why this priority**: An incremental trim (a few MB) that applies to both
variants. Independent of the SWu split and lower value than it, but cleanly
separable and worth doing.

**Independent Test**: Build either variant, confirm the removed apk packages are
not installed, and confirm the bridge still resolves the carrier ePDG hostname
and brings tunnels up correctly.

**Acceptance Scenarios**:

1. **Given** either image variant, **When** its installed apk packages are
   inspected, **Then** the DNS client package (`bind-tools`) and the download
   package (`wget`) are not installed, and `dig` is not present as a command;
   the legacy networking package (`net-tools`) is not installed in the slim
   variant. (Busybox applets for `wget`/`route`/`ifconfig` may still exist —
   this scenario is about apk packages.)
2. **Given** either image variant and a config that resolves the ePDG endpoint
   by hostname, **When** the container starts, **Then** the hostname resolves
   and the tunnel establishes exactly as it does today.
3. **Given** either image variant, **When** the VoWiFi supervision performs its
   periodic DNS-based health/keepalive checks, **Then** those checks behave
   equivalently to the current behavior.

---

### Edge Cases

- **Requesting SWu in the slim image**: startup must fail fast with an
  actionable message, never silently fall back or crash obscurely. Because this
  is an image property, not a line-table one, it is checked before line
  discovery so it holds even with no modem attached and on the discover-retry
  path (SC-005).
- **DNS resolution equivalence (not byte-for-byte parity)**: the native
  resolution path must return a valid A record for the ePDG hostname. It need
  not select the *same* address the previous DNS client utility did when the
  name has multiple A records — the in-process resolver applies RFC 6724
  ordering rather than server order, and any returned ePDG A record is a valid
  entry point. Known benign deltas: a literal-IP `epdg_fqdn` now resolves to
  itself (previously unresolved), and resolution is bounded by the system
  resolver's own timeout rather than a CLI flag.
- **Published-image consumers**: consumers on floating/canonical tags
  (`:latest`, `:X.Y.Z`) now receive the slim image; a consumer that had
  configured the `swu` engine will hit the FR-004 fail-fast on first pull. This
  switch is intentional and MUST be called out in the release notes; consumers
  needing SWu must pin the `-swu` tag.
- **On-demand full image not yet built**: an operator who needs the SWu engine
  when no full image has been published must have a clear, documented way to
  trigger the separate pipeline (or build locally); the regular release does not
  produce a full image, and that must not be mistaken for the fallback being
  gone.
- **SWu code kept but unpublished**: because the SWu path stays in the tree and
  in CI checks, a regression in it must still surface as a failing
  test/lint/format run even though no full image is published on that release.
- **SWu *payload* rot while unpublished**: the full image's Python payload
  (interpreter, deps, and an unpinned upstream git dependency) is only assembled
  on demand, so a break there would otherwise be discovered only when an
  operator first needs the fallback. CI MUST build the payload stage on a
  schedule (and on default-branch merges) so such rot surfaces routinely, not
  during an incident.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The build MUST support a single toggle that selects whether the
  SWu/Python tunnel engine is included, defaulting to **excluded** (slim).
- **FR-002**: The slim (default) image MUST NOT contain the Python interpreter,
  the Python dependency tree, or the vendored SWu-IKEv2 dialer.
- **FR-003**: The slim image MUST run the VoWiFi subsystem via the `strongswan`
  engine with no functional difference from the current image's strongSwan path.
- **FR-004**: When the slim image is configured to use the `swu` engine, the
  bridge MUST fail fast at startup with a clear message stating that SWu is
  unavailable in this variant and pointing to the `-swu`-tagged full image that
  provides it.
- **FR-005**: The full image variant, when built, MUST bundle the Python
  interpreter, Python dependencies, and SWu dialer, and MUST behave identically
  to the current image for both `swu` and `strongswan` engines.
- **FR-005a**: The SWu engine's source code, configuration handling, and tests
  MUST remain in the repository. The standard unit-test, lint, and format
  pipeline MUST continue to cover the SWu path over the whole workspace exactly
  as today — no SWu code or test may be deleted, feature-gated out of the test
  build, or otherwise excluded from CI checks.
- **FR-006**: The DNS lookup the bridge performs for VoWiFi supervision MUST be
  performed without depending on an external DNS client program, producing
  equivalent results to the current behavior.
- **FR-007**: The runtime image MUST NOT install the `bind-tools` (DNS client)
  or `wget` (download) apk packages in either variant, and MUST NOT install the
  `net-tools` package in the slim variant; `net-tools` MUST remain in the full
  variant because the SWu dialer parses the real `ifconfig` binary it provides.
  Each removal MUST be verified against actual runtime usage before it is
  dropped. This governs apk packages only; base-image busybox applets of the
  same name are out of scope.
- **FR-008**: On a normal release, the project's continuous-integration
  publishing MUST produce and publish only the slim image; it MUST NOT publish
  the full/SWu image as part of the regular release flow.
- **FR-008a**: The full/SWu image MUST be producible on demand through a
  separate, explicitly-triggered publishing pipeline, so it can be published
  only when a deployment actually needs it. It MUST be published under the **same
  image name** as the slim image, with a `-swu` suffix on the version tag (e.g.
  `:X.Y.Z-swu`). The slim image MUST retain the canonical version and `latest`
  tags (i.e. `:X.Y.Z` and `:latest` resolve to the slim image).
- **FR-008b**: Continuous integration MUST build the full image's SWu/Python
  payload stage on a schedule and on default-branch merges (no publish
  required), so a break in that stage — including its unpinned upstream git
  dependency — is caught routinely rather than at the moment an operator first
  dispatches the on-demand `-swu` image.
- **FR-009**: Documentation MUST state that the slim image is the default
  published artifact, that the full/SWu image is built on demand via the
  separate pipeline, how to trigger/obtain it, and how to choose between them.
- **FR-010**: When the `swu` engine is configured but no SWu payload is present,
  the bridge MUST fail fast before line discovery (so the check is independent
  of modem/line presence and the discover-retry path), unless the operator sets
  an explicit override for running natively off a host build. The fatal message
  MUST name the concrete `-swu` image tag and the override.

### Key Entities *(include if feature involves data)*

- **Image variant**: a container artifact under one shared image name,
  distinguished by tag: "slim" (default, strongSwan-only, published on every
  release under the canonical `:X.Y.Z` and `:latest` tags) or "full" (includes
  the SWu/Python engine, built and published only on demand under a `:X.Y.Z-swu`
  tag). Distinguished at build time by whether the Python/SWu payload is present.
- **Tunnel engine selection**: the deploy-time configuration value
  (`strongswan` or `swu`) that must be compatible with the chosen image variant.
- **On-demand publishing pipeline**: the separate, explicitly-triggered pipeline
  that produces and publishes the full/SWu image when a deployment needs it,
  distinct from the regular release flow.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The default published image is at least 55% smaller than the
  current ~119 MB image (target on the order of ~45–50 MB).
- **SC-002**: The default image contains zero Python interpreter files, zero
  Python dependency files, and no SWu dialer files (verifiable by inspection).
- **SC-003**: Every VoWiFi behavior exercised today with the `strongswan` engine
  passes unchanged on the slim image — no regression in tunnel establishment,
  call setup, or health checks.
- **SC-004**: The full image, when built on demand, reproduces current behavior
  for the `swu` engine with no regression.
- **SC-004a**: The standard unit-test, lint, and format pipeline continues to
  build and cover the SWu engine's code and tests over the whole workspace, and
  passes — no SWu code or test is removed from or excluded from CI checks.
- **SC-005**: Selecting the `swu` engine on the slim image produces a clear,
  actionable startup failure 100% of the time (never a silent or obscure
  failure).
- **SC-006**: Carrier ePDG hostname resolution and the supervision DNS checks
  succeed on both variants with no `bind-tools`/`dig` present (resolution is
  in-process).
- **SC-007**: A normal release publishes only the slim image under the canonical
  `:X.Y.Z` and `:latest` tags; the full/SWu image is published solely when the
  separate on-demand pipeline is triggered, under a `:X.Y.Z-swu` tag on the same
  image name.

## Assumptions

- The `strongswan` engine is the default and is functionally sufficient for the
  primary supported carriers; the `swu` engine remains a fallback rather than a
  routine path.
- Retaining the SWu fallback (rather than deleting it outright, option #1a) is
  the desired risk posture for now; full removal is a separate future decision.
- The SWu engine's code and tests stay in the tree and stay in the standard
  CI checks; the change is purely about what the *default published image*
  contains and when the full image is published, not about removing code.
- Publishing the full/SWu image on demand (rather than on every release) is
  acceptable because SWu is a rare fallback; operators needing it will trigger
  the separate pipeline. The mechanism/trigger for that pipeline is an
  implementation detail for the plan phase.
- The cellular-internet sidecar image is a separate artifact and out of scope
  here; only the main bridge image is affected.
- The slim image inherits the canonical name and `:X.Y.Z`/`:latest` tags; the
  full image uses a `-swu` tag suffix on the same name (see Clarifications
  2026-08-10). Floating tags therefore resolve to slim, which is acceptable
  because it is called out in the release notes and SWu consumers pin `-swu`.
- The single runtime DNS lookup and periodic DNS-based supervision checks are
  the only consumers of the external DNS client utility; any additional consumer
  discovered during implementation is handled under FR-007 before removal.
- The legacy networking tools were required only by the SWu path and/or are not
  used by the strongSwan path; this is verified before removal per FR-007.
