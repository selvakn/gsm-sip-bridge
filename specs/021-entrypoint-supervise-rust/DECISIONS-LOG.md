# Decisions Log — for Selva's morning review

This feature was run autonomously overnight per your instruction ("don't wait for
questions, make reasonable recommendations and go with that"). Every consequential
judgment call I made without asking is recorded here, most recent last. Please
skim this before merging.

---

## 2026-07-26 — Session start

**Scope authorized**: `/speckit-plan` → `/speckit-tasks` → `/speckit-implement` for
all 5 phases (0-4) of `specs/021-entrypoint-supervise-rust/`, real hardware testing
against the EC20 + Airtel SIM where needed, frequent commits, a PR at the end, a
Greptile review loop fixing P1s, then a Discord notification.

**Environment check before starting** (this matters for what "live-validated" can
mean from this session):
- This worktree sees `/dev/ttyUSB0`..`ttyUSB6` and I'm in the `dialout`/`docker`
  groups, but there is **no sudo** in this sandboxed session (`no new privileges`
  flag set) — matches the existing memory note about needing the privileged
  Docker container for `CAP_NET_ADMIN` work.
- `origin` remote → `git@github.com:selvakn/gsm-sip-bridge.git` (gh authenticated
  as `selvakn`). A second remote `sugam-direct` points at the physical host with
  the modem attached over SSH.
- **Decision**: I will do everything that's testable in pure Rust/bash locally in
  this sandbox. For anything that needs `CAP_NET_ADMIN`/`CAP_SYS_ADMIN` (real
  netns/veth/charon/pcscd against the physical modem), I will use the existing
  privileged docker-compose setup (`docker/docker-compose.yml`) if it's drivable
  from this session without sudo (it should be — `docker` group membership, not
  root, is what matters for `docker compose up`), and will clearly flag in this
  log anything I could *not* actually exercise against the physical Airtel SIM
  so you can do that verification pass yourself.

## Clarifications resolved during /speckit-plan (not asked, decided from codebase evidence)

1. **Concurrency model for the supervisor**: OS threads (`std::thread::spawn`) +
   blocking `std::process::Command`/`Child`, matching every existing piece of
   multi-line orchestration in the tree (`vowifi/mod.rs`, `vowifi/usim_bridge.rs`,
   `ims/agent.rs`). Tokio is only used for the axum metrics/control HTTP surface
   today. Introducing async tasks for the supervisor would add a second
   concurrency shape for equivalent work — rejected as unjustified complexity
   under Constitution V.
2. **`render` output shape**: pure functions in a new `supervise::render` module,
   exposed via a `gsm-sip-bridge render <asset>` subcommand that prints to
   stdout (same shape as the existing `config vowifi-shell-env` verb), snapshot
   tested with `insta`. Snapshots live in `gsm-sip-bridge/src/supervise/snapshots/`
   (insta's default next-to-source convention).
3. **`CommandRunner` scope** (this was an explicit clarification you answered):
   owns spawn/signal/liveness of long-running children, not just transient leaf
   commands — see spec Clarifications section, 2026-07-26.

## Architecture decisions

- New `gsm-sip-bridge/src/supervise/` module (not a new crate) — see plan.md
  "Project Structure" for the file breakdown (`runner.rs`, `render.rs`,
  `shutdown.rs`, `line_supervisor.rs`, `daemon_supervisor.rs`, `sim_recovery.rs`).
- `CommandRunner` trait splits into: transient ops (`run(argv) -> Output`,
  `read_file(path) -> String`) and long-running child ownership
  (`spawn`/`signal`/`is_alive`). Log-scraping (charon.log P-CSCF extraction,
  CSIM-failure detection) stays as **pure string → data functions**, fed by
  `read_file` — this keeps the actually-interesting parsing logic testable with
  zero runner involvement at all, same as the existing `extract_latest_pcscf`.
- `insta` added as a dev-dependency; `bats-core` added as a system/CI test tool
  (not a Cargo dependency). Both need `deny.toml`/lint awareness — logged as a
  Phase 0/1 task.

## Constitution note

The project constitution (`.specify/memory/constitution.md`) has a
**NON-NEGOTIABLE** "Integration-First Testing" principle: mocks are permitted
*only* for things impractical to run in CI (explicitly: hardware), and every
mock site needs a written justification comment. The `CommandRunner` mock in
`LineSupervisor`/daemon-supervisor tests is exactly that carve-out (stands in
for charon/pcscd/swanctl/a live modem) — I will put the justification comment
at every `MockCommandRunner` use site, not just in the plan.

---

## 2026-07-26 — Phase 0 complete (bash safety net)

**Tooling gap found and worked around**: this sandbox has neither `shellcheck` nor
`bats-core` installed, and no sudo/apt to install them. I downloaded a static
`shellcheck` binary and cloned `bats-core` into my scratchpad (not committed to the
repo) to actually run and validate both locally, rather than just writing the Makefile
targets on faith. **Decision**: `make lint`/`make test` gate shellcheck/bats behind
`command -v` (matching the existing `cargo-deny` idiom already in this Makefile) —
they run if present, warn-and-skip if not, so CI/dev machines need to install them
(standard `apt install shellcheck bats` or equivalent) but the Makefile itself doesn't
hard-fail in an environment without them. **You should install both on whatever CI
runs `make lint`/`make test`** for this gate to actually bite — right now, absent
that, the gate silently no-ops. Flagging this clearly since it's the one place
"green CI" could be quietly weaker than it looks.

**Micro-deviation from strict 1:1 extraction** (documented, not asked): the three
`render_line_*` functions write to hardcoded `/etc/...` paths, which meant they
couldn't be exercised by bats in a no-root sandbox (or in a hypothetical no-root CI
runner) without touching the real filesystem. I added `RENDER_HELPERS_ROOT`, an
env var defaulting to empty string, prepended to every such path — with the default
empty, production behavior is byte-for-byte unchanged (`/etc/...`, exactly as
before); bats tests set it to a tmpdir. This is the one place Phase 0 isn't a pure
"move the text, change nothing" extraction — it's a minimal, additive testability
seam. Flagging in case you'd rather I hadn't.

**Shellcheck findings**: exactly one, an info-level false positive (`SC2153` on
`VPCD_PORT`, confused by a similarly-named local `vpcd_port` elsewhere — shellcheck
can't see that `VPCD_PORT` is actually assigned via `eval "$SHELL_ENV"` from the Rust
binary's `config vowifi-shell-env` output). Suppressed inline with a comment
explaining why, rather than disabling the check globally.

**Result**: `docker/lib/render_helpers.sh` (extracted, unchanged logic) +
`docker/lib/render_helpers.bats` (11 tests, all passing, including the Greptile PR #2
P-CSCF-picks-last-line-overall regression case as its own named test) +
`docker/entrypoint.sh` now sources the extracted file instead of defining the
functions inline. `make fmt && make lint && make test` all green. Committed.

---

## 2026-07-26 — Phase 1 complete (Rust rendering + CommandRunner foundation)

**Real bug found and fixed via direct diffing against actual `sed`**: I didn't
just trust my hand-written Rust port of the `swanctl-epdg.conf.template`
substitution — I ran the *actual* `sed` pipeline from `entrypoint.sh` against
the *actual* template file in `docker/strongswan/`, and diffed its output
byte-for-byte against my Rust `render swanctl-epdg` output. They disagreed:
bash's `sed -e "/local_addrs.*@SRC_ADDR@/d"` is **order-sensitive** — it only
deletes a line where `local_addrs` appears *before* `@SRC_ADDR@` — but my first
implementation used an unordered "does this line contain both substrings"
check. The template's own header comment happens to mention `@SRC_ADDR@`
*before* explaining "the local_addrs line", so my version wrongly deleted
that documentation line while bash correctly kept it. Fixed (now checks
ordering, matching sed's `.*` semantics) and pinned with a named regression
test. This is exactly the kind of subtlety the spec's byte-for-byte
requirement (FR-003) exists to catch, and it would NOT have been caught by
testing only against a simplified hand-written fixture — I verified all 5
rendered assets against the real bash execution, not just my own test
fixtures, before calling Phase 1 done. Diffs are all `IDENTICAL`.

**`unsafe` block found and removed**: my first `CommandRunner::signal`
implementation called raw `libc::kill(2)` inside an `unsafe` block. This
project's `make lint` (`tools/count-unsafe.sh`) hard-fails on ANY `unsafe` in
`gsm-sip-bridge/src` (stricter than the FFI crates, which get a 5% ratio
allowance) — caught immediately by running the full gate, not just `cargo
test`. Fixed by shelling out to the `kill` CLI utility instead (`Command::new
("kill").args([flag, pid])`), which is also more faithful to the bash
original's own `kill -TERM/-KILL/... "$pid" 2>/dev/null || true` convention,
and adds no new dependency. Lesson for later phases: always run `make lint`
after adding process-control code, not just `cargo test` — a green test run
does not imply a green lint run in this repo.

**What landed**: `gsm-sip-bridge/src/supervise/` module with `runner.rs`
(`CommandRunner` trait, `RealCommandRunner`, test-only `MockCommandRunner`
with per-use `MOCK JUSTIFICATION` comments per the constitution) and
`render.rs` (5 pure rendering functions, 14 tests incl. `insta` snapshots).
New `gsm-sip-bridge render <asset>` CLI subcommand. `docker/entrypoint.sh`
now calls the Rust binary instead of the bash heredocs/`sed`; the
superseded `render_line_*` bash functions and their bats coverage were
deleted from `docker/lib/` (only `extract_latest_pcscf` remains there,
pending its own Phase 3 port). `make fmt && make lint && cargo test
--workspace` (579 tests) all green.

**Not yet live-validated against the real EC20 + Airtel SIM**: I verified
correctness by diffing Rust output against the actual bash/sed execution on
the real template file (as strong a check as is available without deploying),
but have not yet run the built image against the physical modem — that's
still queued, alongside Phases 2-4, before I'd call this feature done. Noting
this explicitly so "all green in cargo test" isn't mistaken for "verified on
hardware."

---

(Further entries appended as phases proceed.)
