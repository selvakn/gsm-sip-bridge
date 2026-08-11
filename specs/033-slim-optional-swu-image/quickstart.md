# Quickstart: Slim default image with optional SWu engine

## Build

The verify sections below tag the images `gsm-sip-bridge:slim` / `:swu`, so use
the explicit `docker build` forms. Note `make docker-build` goes through
docker-compose and instead tags the slim image `docker-gsm-sip-bridge:latest` —
set `IMG` accordingly if you build that way.

```bash
# Slim (default) — strongSwan only, ~34 MB
docker build -f docker/Dockerfile -t gsm-sip-bridge:slim .
# or: make docker-build   (tags docker-gsm-sip-bridge:latest via compose)

# Full (on demand) — includes the SWu/Python engine
make docker-build-swu   # tags gsm-sip-bridge:swu
# or: docker build -f docker/Dockerfile --build-arg INCLUDE_SWU=true -t gsm-sip-bridge:swu .
```

## Verify the slim image

```bash
IMG=gsm-sip-bridge:slim   # or docker-gsm-sip-bridge:latest if built via `make docker-build`
# Size
docker images "$IMG" --format '{{.Size}}'
# No Python / SWu payload, and the SWu-only apk packages are absent.
# NOTE: check apk *packages*, not `command -v` — Alpine's busybox always
# provides `wget`/`route`/`ifconfig` applets, so `command -v wget` succeeds in
# the slim image too. Only `dig` (bind-tools) has no busybox applet, so a
# `command -v dig` check is meaningful there.
docker run --rm --entrypoint sh "$IMG" -c '
  set -e
  ! command -v python3 >/dev/null && echo "no python3 OK"
  [ ! -e /opt/pydeps ] && echo "no pydeps OK"
  # WORKDIR creates an empty /opt/SWu-IKEv2 even in slim, so check the dialer
  # file + marker (what the runtime guard actually keys on), not the dir.
  [ ! -e /opt/SWu-IKEv2/swu_emulator.py ] && echo "no SWu dialer OK"
  [ ! -e /etc/gsm-sip-bridge/swu-available ] && echo "no marker OK"
  ! command -v dig >/dev/null && echo "no dig OK (bind-tools gone)"
  ! apk info -e bind-tools >/dev/null 2>&1 && echo "no bind-tools pkg OK"
  ! apk info -e wget       >/dev/null 2>&1 && echo "no wget pkg OK"
  ! apk info -e net-tools  >/dev/null 2>&1 && echo "no net-tools pkg OK"
'
```

## Verify the full image

```bash
IMG=gsm-sip-bridge:swu
docker run --rm --entrypoint sh "$IMG" -c '
  set -e
  command -v python3 >/dev/null && echo "python3 OK"
  [ -e /opt/SWu-IKEv2/swu_emulator.py ] && echo "SWu dialer OK"
  [ -e /etc/gsm-sip-bridge/swu-available ] && echo "marker OK"
  # net-tools installs the REAL /sbin/ifconfig (a ~56KB binary, not the busybox
  # symlink) — that is what the SWu dialer parses. Check the package, not the
  # applet.
  apk info -e net-tools >/dev/null 2>&1 && echo "net-tools pkg OK"
  [ "$(stat -c %s /sbin/ifconfig)" -gt 40000 ] && echo "real ifconfig OK"
'
```

## Fail-fast check (slim + swu)

With a `config.toml` containing `[vowifi] enabled = true` and
`tunnel_engine = "swu"`, starting the **slim** image must exit fast with a clear
message pointing to the `-swu` image — it must not spawn line processes.

## Standard checks (unchanged, must stay green)

```bash
make format
make lint     # whole workspace incl. SWu code and test targets
make test     # SWu engine path still compiled + covered (FR-005a)
```

## Pull (after release)

```bash
# Slim (default) — floating/canonical tags resolve to slim
docker pull ghcr.io/selvakn/gsm-sip-bridge:latest

# Full (only when the on-demand pipeline has published it)
docker pull ghcr.io/selvakn/gsm-sip-bridge:<version>-swu
```

> Note: consumers previously on `:latest`/`:X.Y.Z` who ran the `swu` engine will
> hit the fail-fast on first pull — pin the `-swu` tag instead. Called out in
> release notes.
