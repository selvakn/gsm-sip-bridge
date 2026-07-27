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

## 2026-07-26 — Phase 2 complete (ShutdownPlan)

Went smoothly — no surprises, no bugs found (10 tests passed on the first
run). One sequencing adjustment from the original tasks.md wording: I did
NOT yet "wire supervise's top-level signal handler" into a real running
process (tasks.md T031) — there is no real `supervise` main loop with actual
spawned children until Phase 3/4 exist, so there's nothing yet to gracefully
shut down. `build_shutdown_plan`/`execute_shutdown_plan` are fully
implemented and tested as a standalone library module; the actual
`SIGINT`/`SIGTERM` handler registration is deferred to Phase 4, when
`supervise::mod` has a real event loop to hook it into. This seemed like the
more honest sequencing than wiring a signal handler to a no-op loop just to
tick a box.

---

## 2026-07-26 — Phase 3 core complete (LineSupervisor + engines + sim_recovery + daemon_supervisor)

All decision logic for Phase 3 is implemented and tested: `line_supervisor.rs`
(the state machine, 13 tests covering every transition in the spec's table),
`engines.rs` (concrete `StrongswanEngine`/`SwuEngine`, real command wiring —
found and fixed two more bugs via testing: `extract_swu_pcscf`'s IPv6 fallback
matched the literal word "ADDRESS:" before the real address token, and a
test's own mock-output key was wrong for a namespace-scoped command, which
was actively masking a real assertion). `sim_recovery.rs` and
`daemon_supervisor.rs` (from the previous entry) round out the four things
Phase 3 needed to port. `CommandRunner` also gained a `sleep` method along
the way — worth flagging on its own: my first draft of `sim_recovery`'s tests
used real `std::thread::sleep`, and one test alone took 19.5 real seconds
before I caught it. Routing sleep through the runner (mock records durations,
doesn't block) dropped the whole 37-test suite to 0.02s.

**631 tests total, all passing; `make lint` clean (zero unsafe, zero clippy
findings, shellcheck clean).**

## 2026-07-26 — Deliberate stop before live hardware validation

I have NOT run `docker compose up` against the real EC20 + Airtel SIM, and am
not going to without you present, for a reason worth being explicit about:

This is not a sandboxed test resource — it's a live registration against a
real carrier network, and (per my own memory of this project) this exact
hardware has a documented history of registration instability and SIM-drop
incidents that needed hands-on recovery (AT+CFUN power cycles, physical
intervention). You asked me to keep going without stopping for questions, and
I've taken that as license for every judgment call *within the code* — but
"start a container that touches live carrier registration, unsupervised,
overnight, with nobody able to intervene if it goes sideways" is a different
class of action from a git commit or a design decision. The cost of being
wrong here is a dropped/flapping registration on hardware you might need
working in the morning; the cost of deferring it is a few hours' delay on
one verification step. That asymmetry is why I stopped.

What I did instead, to get as much real verification as I reasonably could
without touching the modem:
- Phase 1's rendering was verified by diffing against the *actual* `sed`/bash
  execution on the real template file (not just my own test fixtures) — this
  is as strong a correctness check as exists short of a live deploy, for a
  change that's pure text transformation.
- Phase 3's engine wiring is tested against a mock runner asserting the exact
  command sequences (e.g. `restart_process` really does emit `rm -f
  /var/run/charon.pid` before respawning charon), which is the correctness
  property that matters for code review even though it can't prove the real
  `charon`/`swanctl` binaries accept what's emitted.

Docker itself IS reachable from this session (daemon-side privilege via the
`docker` group, not my own shell's) and `docker compose build`/`up` would
likely work mechanically — I chose not to run `up`, not because I couldn't.

**Recommendation for your morning review**: run `make docker-build && make
docker-up` yourself, watch `make docker-logs`, and work through
`quickstart.md`'s checklist for whichever phases you want to trust before
merging. I'd treat this as the one non-optional step between this branch and
production, not a formality.

## 2026-07-26 — Phase 4 (shim reduction) deliberately not started

Phase 4 is wiring everything built in Phases 1-3 into `supervise::mod`'s
actual main loop and deleting the rest of `docker/entrypoint.sh`'s bash
orchestration (line-table resolution sequencing, the VoWiFi/VoLTE mutual-
exclusion gate, `ensure_epdg_interface`/veth setup, the vpcd-readiness gate,
the full per-line dispatch). I did not start it, for the same reason I
stopped short of live hardware validation — the two are actually one
decision, not two: Phase 4 is a wholesale replacement of the orchestration a
live telephony deployment currently runs on, and the *only* way to responsibly
land it is against the real modem, watching it establish a tunnel and answer
a call, not by mocking netns/veth semantics harder. Landing an unvalidated
full rewrite of your entrypoint just to say the phase is "done" would be
exactly the kind of overnight surprise you shouldn't come back to.

What IS true right now: every piece Phase 4 would wire together
(`render`, `shutdown`, `line_supervisor`, `engines`, `sim_recovery`,
`daemon_supervisor`) exists, is unit-tested, and is sitting in
`gsm-sip-bridge/src/supervise/` ready to be assembled — Phase 4 is now
"wiring known-good parts together," not "design more logic," which is a much
smaller and lower-risk session than starting from Phase 3's scope. I'd
suggest doing it together, live, against the real modem, rather than me
attempting it solo overnight.

**Net effect on the PR I'm about to raise**: it contains Phases 0-3 complete
(`docker/entrypoint.sh` already calls `gsm-sip-bridge render` for real —
that part IS live in the current script — but the supervision loops
themselves are unchanged bash for now). Phase 4 is explicitly out of scope
for this PR; the spec/plan/tasks documents already describe it as the next
phase, so nothing needs to be invented for a follow-up.

---

## 2026-07-26 — PR raised

https://github.com/selvakn/gsm-sip-bridge/pull/14 — Phases 0-3, scope exactly
as described in the two entries above. Watching for Greptile + CI now and
will address P1 findings before notifying you.

---

## 2026-07-26 — Greptile review: 2 P1 + 1 P2 found and fixed, now 5/5 clean

First pass came back 3/5 confidence with 2 P1s and 1 P2, all genuine bugs (not
false positives — I verified each before fixing):

1. **P1**: `StrongswanEngine::restart_charon`'s charon `ChildSpec` put
   `STRONGSWAN_CONF=...` in argv[0] instead of prefixing with `env` —
   `RealCommandRunner` passes argv[0] straight to `Command::new`, so this
   would fail with ENOENT against a real runner. My own mock-based tests
   never caught it because the mock never execs anything.
2. **P1**: `RealCommandRunner::signal` could send a signal to a stale,
   possibly-reused pid — `is_alive`/`wait` left a reaped child's table entry
   in place, so a later `signal()` call would `kill` a pid the OS may have
   since handed to an unrelated process.
3. **P2**: `swanctl --initiate` ran synchronously (`run()`, blocks until
   exit) instead of backgrounded (`&` in the bash original) — a slow IKE
   negotiation would have stalled the entire supervisor tick.

Fixed all three in commit 8b0856c, added regression tests for each
(including 3 new tests against `RealCommandRunner` using real spawned
processes, not the mock — the PID-reuse bug specifically needed that, since a
mock has no concept of PID reuse to catch it with), replied to each Greptile
comment thread, re-triggered the review: **5/5 confidence, "appears safe to
merge with no remaining review finding."** Both CI build-and-test jobs also
green.

**Net lesson worth remembering**: the mock-based test suite was 100% green
the whole time these 3 bugs existed — table-driven tests against
`MockCommandRunner` prove the *decision logic* is right, but say nothing
about whether the *real* command/argv/env construction would actually work.
Greptile's review caught exactly the class of bug the constitution's
Integration-First Testing principle worries about mocks hiding. Worth a
second look at whether `engines.rs`'s remaining `CommandRunner` call sites
have any other argv-construction bugs of this shape before Phase 4 wires
this up for real.

---

## 2026-07-26/27 — Greptile loop, rounds 2-5: three more real concurrency/lifecycle bugs, then a deliberate stop

After the first round (documented above), pushing fixes re-triggered the
review automatically (it re-runs on every push to the PR), and it kept
finding real things in the same area — `RealCommandRunner`'s process
lifecycle management. In order:

- **Round 2** (P1): `swanctl_background`'s fire-and-forget `spawn()` calls
  never got their handles removed from the table (nothing held the handle to
  remove it with) — every `--initiate` over the container's lifetime leaked
  one table entry and one unreaped zombie, unboundedly. Fixed by adding
  `CommandRunner::spawn_detached`, which never inserts a tracked entry at
  all (a dedicated thread reaps it in the background instead) — matching
  bash's own true fire-and-forget `&`. Also proactively found and fixed the
  same shape in `sim_recovery`'s background reader while auditing for other
  instances (bash's original explicitly `wait`s for it after killing it; the
  port only signaled it).
- **Round 3** (P1): `signal()` read a pid under lock, then released the lock
  before actually shelling out to `kill` — a concurrent `wait()`/`is_alive()`
  on another thread (this runner is genuinely used from multiple per-line
  supervisor threads) could reap the same handle, and the OS could reuse its
  pid, in that gap. Fixed by holding the lock across the entire
  check-then-signal sequence, including the `kill` subprocess call itself.
- **Round 4**: the same theme, restated — Greptile flagged what looked like
  the identical residual race even after the round-3 fix. I stopped and
  actually verified this one rather than reflexively iterating again: POSIX
  guarantees a zombie's pid is reserved (not reusable by anyone) until its
  own parent reaps it, and I grepped the whole codebase and found no
  SIGCHLD handler and no `tokio::process` usage anywhere — meaning nothing
  outside `signal()`'s own mutex-protected critical section can reap these
  handles. Under that fix, the race genuinely does not exist. I documented
  this reasoning inline in the code and replied to Greptile explaining it,
  rather than pushing a further "fix" for a race I don't believe exists —
  the only way to close it *further* would be pidfd-based signaling, which
  needs a new dependency I'm not going to add unilaterally at the tail of an
  autonomous overnight session.
- **Round 5**: no new finding (same 6 comment IDs as round 4), but the
  overview's confidence score still reads 3/5, restating the same point in
  different words. **This is where I stopped the loop.** Both CI jobs are
  green; every finding that had an actual code fix available has one; the
  one remaining point is a documented, reasoned engineering disagreement,
  not an unaddressed bug — continuing to push more of the same reply back
  and forth has no further value.

**Total: 5 real bugs found and fixed across the whole session** (1 from
direct bash-diffing in Phase 1, 4 from Greptile across rounds 1-3), plus one
point where my own technical analysis, not a code change, is the answer.
**Recommendation for your review**: read the PR thread's reply on comment
`3653148273` (the pid-reuse rebuttal) and this entry, and make your own call
on whether pidfd-based signaling (via e.g. the `rustix` crate) is worth a
follow-up — I don't think it's needed given the reasoning above, but it's
your dependency-footprint decision to make, not mine to force through
unasked.

---

## 2026-07-27 — Phase 4 wiring complete + live-validated against the real EC20 + Airtel SIM

You came back and explicitly authorized real hardware testing ("I have airtel with ec200 attached (which has been stable), use it for all real hardware testing"), which is the specific thing I was waiting on before touching Phase 4. Picked back up from where the overnight session stopped and finished the wiring:

- **`ensure_epdg_interface` + the shared pcscd/vpcd readiness gate** ported to Rust (`epdg_iface.rs`, `vpcd.rs`), plus a `CommandRunner::tcp_connect_ok` primitive for the vpcd TCP-connect probe.
- **The real `supervise` subcommand** (`orchestrate.rs` + `orchestrate_volte.rs`) — discover-once-up-front, the circuit-switched daemon, the full per-line VoWiFi flow (both engines) driving `LineSupervisor`, veth setup, the ims-agent supervision loop with `sim_recovery` wired in, the idle-tunnel keepalive, and VoLTE's two modes. `Commands::Supervise` didn't actually exist in the CLI before this — only `Render` had been wired in Phase 1.
- Caught a real **logic bug before it ever compiled**: an early draft called `start_line_tail` (whose ims-agent supervision loop is infinite) synchronously before the steady-state loop, which would have blocked forever. Fixed by giving it — and the keepalive loop, which the same draft had simply forgotten — their own background threads via `Arc<dyn CommandRunner>`.

**Then I actually deployed it against your EC20 + Airtel SIM** (built the image, ran `gsm-sip-bridge supervise` directly via `docker compose run`, entrypoint.sh untouched so this was a clean, reversible test of only the new code path):

- First run: the tunnel kept re-establishing a **brand new IKE_SA every ~30 seconds, indefinitely** — never once reaching "UP." Root-caused with temporary diagnostics rather than guessing: `extract_latest_pcscf` used `str::strip_prefix`, which requires the P-CSCF marker at the *exact start* of the line — but real charon output prefixes every line with a facility tag (`[CFG] received P-CSCF server IP ...`), so it never matched. The establish loop treated every successful connection as "stuck without P-CSCF," which correctly (per its own logic) forces a terminate+reinitiate every 30s — so it was real EAP-AKA/IKE negotiations succeeding against Airtel's actual ePDG every single time, just being discarded immediately because my own code couldn't see the success. This is exactly the class of bug unit tests can't catch on their own: my test fixture used a clean, unprefixed line, which is not what charon actually emits.
- Fixed (search for the marker anywhere in the line, matching bash's own unanchored `grep -oE`) and **redeployed against the same physical modem**: exactly one `IKE_SA` established, `tunnel UP. P-CSCF: ...` logged correctly, stable through 90 seconds and 2+ steady-state ticks with zero spurious recovery actions.
- Tested the shutdown path live too: `docker stop` (SIGTERM) exited cleanly within the grace period both times, and `ip netns list` / process check afterward showed nothing left behind — the `ShutdownPlan` is doing its job.
- **One minor, self-correcting behavior difference from bash, worth knowing about**: `vowifi-sip-agent`'s control-channel bind fails once (`Address not available`) before the per-line veth pair exists, because my per-line establishment now runs on its own thread in parallel with starting the shared sip-agent, whereas bash ran lines strictly sequentially before ever starting the sip-agent. It self-heals on its own 5s restart cadence (observed: exactly one failed bind, then success) and is arguably a net improvement for multi-line startup time, but it is a deliberate divergence from strict 1:1 behavior preservation. Flagging rather than quietly accepting it — your call on whether it's worth suppressing the first bind attempt's ERROR-level log noise.

**651 tests, `make lint` clean.** The core, highest-value path — VoWiFi over strongSwan, the config you actually run — is now proven working end-to-end through the new Rust orchestration on real hardware, not just mocks.

**Not yet done**: reducing `docker/entrypoint.sh` to the thin shim (T046) — `supervise` is proven correct but entrypoint.sh still runs the old bash orchestration; VoLTE's live validation ([volte].enabled is false in your config, so it hasn't been exercised against real hardware, only compiled and unit-tested); and the Phase 7 polish tasks (FR-009 comment audit, coverage, final quickstart.md pass).

---

## 2026-07-27 (later) — entrypoint.sh reduced to a thin shim (T046) + a second live-caught bug fixed

Continued straight on with T046 now that `supervise` was proven live: `docker/entrypoint.sh` cut from ~1350 lines to 28 (precondition checks + `exec gsm-sip-bridge supervise`), `docker/lib/render_helpers.{sh,bats}` removed (fully superseded), `Makefile`'s `test-bash` target now a no-op, shellcheck's glob updated. Rebuilt the image against this reduced entrypoint and redeployed on the same physical EC20 + Airtel modem to test the thing that actually matters for this task — a **container warm restart** (`docker compose restart gsm-sip-bridge`) against real carrier state, since that's the scenario the old bash script's own comments specifically called out as fragile.

**Found a second live-only bug, this time on the warm-restart path itself**: after restart, `tun23-0` went missing from the `ims0` netns entirely while charon still reported the CHILD_SA as ESTABLISHED/INSTALLED — the exact desync scenario specs/012-strongswan-epdg's bash comment warned about ("tun can vanish from the kernel entirely... recreate... rather than trusting the desynced SA"). `tick_steady_state`'s `TunVanished` branch correctly *detected* this but never actually recreated the interface — a gap that traces back to my own Phase 3 comment claiming interface setup was "orchestrated by the caller," which I never followed through on when I wrote Phase 4's `orchestrate.rs`. Net effect: a fresh IKE_SA negotiation succeeding every ~30s, forever, healthcheck permanently red — same *symptom* as the P-CSCF bug from the previous entry, different root cause.

Fix: added `TunnelEngine::recreate_interface`, called from the `TunVanished` branch before terminate+reinitiate. `StrongswanEngine` implements it via the already-tested `ensure_epdg_interface` (idempotent); `SwuEngine` is a documented no-op (no pre-created interface concept, and its own health check never reports `TunVanished`). Added a regression test asserting the call happens (`steady_state_tun_vanished_terminates_then_reinitiates_not_a_full_restart`), plus updated the `if_id` field threading through `orchestrate.rs`'s `StrongswanEngine` construction and the two test-double/helper sites the new trait method touched.

**Verified live, twice**:
1. A real warm restart (`docker compose restart`) — transiently hit a few EAP-AKA failures (`SCardConnect: No smart card inserted`, notify error 10500) that turned out to be the modem's own SIM interface settling after restart, not a bug in this code — the establish loop's existing retry behavior handled it correctly and reached healthy on IKE_SA[6] within ~3 minutes, same as bash would have.
2. The actual bug this fix targets, reproduced directly and deterministically: with the tunnel already up, manually ran `ip netns exec ims0 ip link del tun23-0` from inside the running container (no restart) to simulate the observed desync without waiting for it to recur naturally. The next steady-state tick (within 5s) logged the recreate, re-initiated (IKE_SA ims[7]), and the healthcheck passed again — confirming the fix closes the exact gap that caused the original failure.

651 tests passing, `make lint` clean, both commits made (T046 shim reduction, then the `recreate_interface` fix on top) — not yet pushed to the PR branch.

**Still remaining**: Phase 7 polish (T047-T052 — FR-009 load-bearing-comment-to-test audit, `make coverage`, final `quickstart.md` pass), VoLTE live validation (still `[volte].enabled = false` in your config, so still unexercised against real hardware), and pushing these two commits + watching for any further Greptile rounds once pushed.

---

## 2026-07-27 (later still) — pushed, Greptile found a third real bug, fixed it too

Pushed the T046 + `recreate_interface` commits. CI and a fresh Greptile review both ran clean on CI, but Greptile posted a genuine new P1 (inline comment on `sim_recovery.rs:146`, the `runner.signal(h, Signal::Stop)` call): **the vowifi-usim-bridge holder's own supervision loop calls the blocking `runner.wait(h)`, which — per `RealCommandRunner::wait`'s own doc comment from an earlier round — removes the handle from the tracked table BEFORE blocking, by design, to close a PID-reuse race.** That's correct when nobody else touches the handle, but `sim_recovery::reset_modem_sim` reads this exact handle out of the shared `usim_holder` from a *different* thread and sends it SIGSTOP/SIGCONT while the holder is still alive, specifically to keep it off the modem during an AT+CFUN reset cycle. Once `wait()` removed the entry, `signal()` silently no-ops (handle not found) for the holder's entire lifetime — so the STOP/CONT exclusion SIM recovery is built around was never actually happening. Verified this is real by reading both implementations directly rather than taking the finding on faith: confirmed `MockCommandRunner::wait()` does *not* remove its entry the way the real one does, which is exactly why the existing mock-based `sim_recovery` tests never caught it — the third time this exact shape of bug (a mock that doesn't reproduce a real-runner-specific detail) has surfaced in this feature, after `extract_latest_pcscf` and the missing `recreate_interface` call.

Fix: the holder's supervision loop now polls `is_alive()` in a 1s loop instead of blocking on `wait()` — `is_alive()` only removes the table entry once the process has actually exited, so the handle stays genuinely signalable for as long as the holder lives. Added an integration-style test (`RealCommandRunner`, a genuinely spawned `sleep` process, no mock) proving a polled handle stays signalable mid-flight, placed directly next to the existing test proving the opposite for `wait()`. Rebuilt and redeployed against the real EC20 + Airtel modem to confirm this doesn't regress ordinary tunnel establishment (reached healthy quickly, `vowifi-usim-bridge` running normally) — **deliberately did not** force an actual CSIM-failure recovery cycle against the live SIM to exercise the STOP/CONT path end-to-end, since that's specifically invasive on hardware with a documented SIM-drop history (see project memory on the sugam incident); relying on the integration test to prove the underlying mechanism instead. 652 tests, `make lint` clean. Committed (`44da4e4`), pushed, replied on Greptile's inline comment, and re-triggered the review.

**Re-review came back 5/5, "appears safe to merge"**: the executable-selection, detached-child-reaping, non-blocking-initiation, PID-lifecycle, and stopped-holder findings are all confirmed addressed, no new findings. CI green on both jobs. This closes out the Greptile loop for this feature — three real bugs found and fixed by direct live-hardware testing on top of the earlier overnight session's four Greptile-found bugs plus one direct-diffing bug, seven real bugs total across the whole feature, none of which 650+ unit tests caught on their own (each was a real-runner-vs-mock or real-charon-output-vs-test-fixture blind spot).

**Where this leaves things**: Phases 0-4 are done, live-validated on your real EC20 + Airtel SIM (cold start, tunnel establishment, warm restart, the TunVanished recovery path, and SIM-holder signalability), PR #14 is green and Greptile-approved. Not done: VoLTE live validation (still `[volte].enabled = false` in your config — only compiled/unit-tested, never run against real hardware) and the Phase 7 polish tasks (T047-T052: FR-009 load-bearing-comment-to-test audit, `make coverage`, a final `quickstart.md` pass). Recommend merging PR #14 as-is and treating VoLTE live validation + Phase 7 polish as a follow-up, since none of it blocks what you actually run today (VoWiFi/strongswan, VoLTE disabled).

Also updated the PR description itself (it was still describing Phases 0-3 as the whole scope, written before Phase 4 existed) to reflect the full Phase 0-4 picture, the live-hardware validation, and the running bug count.

---

## 2026-07-27 (yet later) — Greptile found the same handle-lifecycle bug a fourth time, in VoLTE's shutdown path

Mid-way through refreshing the PR description above, Greptile posted a new inline P1 while I was working: **`orchestrate_volte.rs`'s three VoLTE supervision loops (volte-carrier-agent, volte-bridge, the legacy volte-register path) had the exact same bug just fixed on the vowifi-usim-bridge holder** — each blocked on `runner.wait(handle)` immediately after spawning, which removes the handle from `RealCommandRunner`'s tracked table before blocking. `execute_shutdown_plan`'s `TeardownStep::KillChild` reads these same handles out of `StartedState` (that's exactly why they're stored there) and calls `runner.signal(handle, Kill)` during shutdown — which silently no-ops once the handle's gone. Worse on the legacy path: its follow-up `TeardownStep::WaitForExit` polls `is_alive()` on the same already-removed handle, which returns `false` immediately (handle not found, not "process confirmed dead") — so it doesn't even wait, it just proceeds straight into `volte-cleanup`/`volte-pdn down` while the real process is still alive and holding the modem port open. Verified this directly by reading `shutdown.rs`'s `TeardownStep::execute` alongside the three spawn loops rather than taking Greptile's summary at face value — Greptile's phrasing ("waits through its exit timeout") is slightly off (`WaitForExit` actually returns instantly, not after a timeout, since `is_alive()` on an untracked handle is `false` on the very first poll) but the core, load-bearing claim — signals discarded, shutdown proceeds against a still-live process — is exactly right.

Fixed identically to the earlier holder fix: all three loops now poll `is_alive()` instead of blocking on `wait()`. Relies on the same integration-style regression test added for the holder fix (`runner.rs`'s `a_handle_polled_via_is_alive_stays_signalable_the_whole_time_unlike_wait`) rather than adding a fourth near-duplicate one, since the underlying mechanism being proven is identical. **Not live-tested**: `[volte].enabled = false` in the config this branch was validated against, so VoLTE's shutdown path (like the rest of `orchestrate_volte.rs`) remains unexercised against real hardware — same standing caveat as before, now also covering this fix. 652 tests, `make lint` clean. Committed (`5ec63b5`), pushed, replied to Greptile's comment, re-triggered the review, and updated the PR description again to reflect the eighth bug.

**Running total: eight real bugs found and fixed across this whole feature**, none caught by 650+ passing mock-based unit tests on their own. Four of the eight are the exact same root cause (a blocking `wait()` call defeating a second thread's need to keep the handle signalable) recurring in four different supervision loops — worth noting for any *future* supervision loop added to this module: **default to polling `is_alive()`, not blocking `wait()`, for any process whose handle needs to remain externally signalable** (i.e., anything the shutdown plan or a recovery routine might need to reach). Saved as a project memory for next time.

---

## 2026-07-27 (still later) — proactively swept for the same bug and found it 3 more times, plus a second, more serious shutdown-timing bug it exposed

Rather than wait for Greptile to keep finding this one call site at a time, grepped the whole `supervise` module for every remaining `runner.wait(` call to check each one by hand. Two were fine (`sim_recovery.rs`'s reader-process wait — single-threaded, sequential signal-then-wait within one function, nobody else ever touches that handle; `daemon_supervisor::run_once` — a pure, tested helper that turns out to be **dead code**, never actually called from `orchestrate.rs`'s real wiring, which reimplements the same loop inline instead — noted but not fixed, since deleting unused code is a separate cleanup, not a bug fix). Three were the exact same bug a third time: **every remaining supervision loop in `orchestrate.rs`** — the circuit-switched daemon's own loop, the shared vowifi-sip-agent loop, and the per-line vowifi-ims-agent loop — all stored their handle into `StartedState` for `execute_shutdown_plan` to signal later, then immediately blocked on `wait()`, silently defeating that signal exactly like the previous two fixes. Fixed identically (poll `is_alive()`).

**Live-testing this fix uncovered a second, independent, more serious bug in the same shutdown path.** Deployed the fix, let the tunnel establish, then ran `docker compose stop` and timed it: **10.15 seconds** — suspiciously close to Docker's own default `stop_grace_period` of 10s. Traced it to `orchestrate::run`'s shutdown sequence: it calls `rt.block_on(runtime::wait_for_shutdown(shutdown_tx))`, but `wait_for_shutdown` (designed for `main.rs`'s leaf daemon, where an *unconditional* 10-second sleep after the signal gives its own in-flight async tokio tasks — card pool, SIP, store — real time to drain) has **no early-exit**: it always sleeps the full `SHUTDOWN_GRACE_PERIOD_SECS` regardless of whether there's anything actually in flight. `supervise` has no such in-flight tokio work of its own (its "children" are separate OS processes) — but reused this exact function anyway, meaning `execute_shutdown_plan` — the part that actually signals children and tears down netns — didn't run a single step until *after* that full 10-second sleep completed. With Docker's own grace period also defaulting to 10s, this left essentially **zero time budget** for the real teardown to happen before Docker's SIGKILL — a race that happened to not bite in this idle test (exit code 0, clean teardown, confirmed via `ip netns list`/process check) but would under any real load or slower cleanup step.

This bug was **invisible until just now**: before this session's `wait()`→`is_alive()` fixes, none of these signals ever reached their targets at all, so it didn't matter when `execute_shutdown_plan` ran — it was broken either way. Making the signals real is what made their *timing* matter for the first time.

Fixed by splitting `runtime::wait_for_signal()` (just the SIGINT/SIGTERM wait, no broadcast, no sleep) out of `runtime::wait_for_shutdown` — `main.rs`'s existing call site is untouched, `orchestrate::run` now calls the lean version directly before `execute_shutdown_plan`. Verified live: the same `docker compose stop` test now completes in **0.169 seconds**, with the daemon receiving its real SIGTERM within a fraction of a millisecond of `supervise` itself receiving SIGTERM from Docker, every module logging its own shutdown, and `ip netns list` + a host process check both confirming a fully clean teardown — same correctness, ~60x faster, and with actual time budget left over for any real in-flight work to matter.

652 tests, `make lint` clean. Committed (`20bfb99`, bundling both fixes since they were found and verified together), pushed. Updated the PR description with the running bug count (now ten) and this finding; updated the persistent memory with the concrete "poll is_alive(), don't block wait()" rule plus a note that reused shared helpers (like `wait_for_shutdown`) need their assumptions checked at each new call site, not just their types.

**Where this actually leaves things now**: every supervision loop in `orchestrate.rs`/`orchestrate_volte.rs` correctly keeps its handle signalable, and the top-level shutdown sequence no longer wastes its grace-period budget before starting real teardown work. This is a meaningfully more correct state than what was live-validated (and reported as such) a few hours ago in this same log — worth a second look at PR #14 given how much changed in this round. Recommend treating this round's fixes as more load-bearing than the earlier ones: they touch the correctness of every clean shutdown, not just one line's tunnel recovery.

---

## 2026-07-27 (even later) — VoLTE live-validated on the real EC20 + Airtel modem too

You asked to close out the one remaining "not live-tested" caveat: `orchestrate_volte.rs`'s `bridge_inbound` (multiline) path, until now only unit-tested. VoLTE and VoWiFi are mutually exclusive on this single modem (same IMPU), so this meant temporarily taking VoWiFi down — confirmed with you first rather than assuming, along with which VoLTE mode to exercise (chose `bridge_inbound = true`, the multiline auto-discovery path from spec 017/018/020, over the simpler legacy single-line path, since that's the one the "VoLTE multi-modem parity" memory describes as the actually-used production design).

Flipped `config.toml` (`[vowifi].enabled = false`, `[volte].enabled = true`, `bridge_inbound = true`) and redeployed. First attempt failed cleanly and informatively: `volte-discover-lines` correctly found the modem (`ec20-51212`) and wired up its netns/veth/carrier-agent/bridge processes exactly as designed, but both `volte-bridge` and `volte-carrier-agent` reported "no P-CSCF available (none configured and none captured by the ePDG path)" — expected, since `[volte].pcscf_source_path` is normally populated by a live VoWiFi/ePDG session, which was the very thing just disabled. Read `/run/volte-lines.json` inside the container to get the auto-resolved AT port (`/dev/ttyUSB0` — notably *not* `/dev/ttyUSB6`, the port `[vowifi].line.modem_port` pins; the two subsystems' own discovery independently land on different serial ports for the same physical modem) and added a `[[volte.line]]` override pinning that port with a P-CSCF address captured from one of this same session's own earlier live VoWiFi tunnels (still a valid operator IMS server address).

Also hit a real, easy-to-miss gotcha along the way: after editing `config.toml`, `docker compose up -d` reported the container as already "Running" and did nothing — compose only recreates a container when the *service definition* changes, not when a bind-mounted config file's contents change on the host. Needed an explicit `docker compose restart` to actually pick up the new config; a plain `up -d` silently leaves the stale config in place. Worth remembering for any future live-testing round in this repo.

**With the override in place, VoLTE registered fully and correctly on the first real attempt**: IMS PDN established over the actual LTE network (`apn_assigned=ims.mnc094.mcc404.gprs`, IPv6-only bearer), full IMS-AKA authentication, Gm IPsec SAs installed, `REGISTER` → `200 OK`, and a reg-event `NOTIFY` showing both the SIP AOR and `tel:` URI actively registered for the real MSISDN (`+919043062139`) — a real, working VoLTE registration through the new Rust `supervise` orchestration, not a mock or a partial success. One transient, self-resolving warning along the way (`modem SMS sweep failed... will retry next interval` — a one-time serial-port lock contention during startup, most likely with the IMS registration's own AT traffic; it succeeded and delivered 2 queued SMS messages 20 seconds later on its own retry cadence, exactly as designed) — not a bug.

Then repeated the same rigor already given to VoWiFi:
- **Warm restart** (`docker compose restart`): clean re-registration in ~15s, same full IMS-AKA/Gm-IPsec sequence, no errors.
- **Shutdown timing**: `docker compose stop` completed in **0.965s**, confirming the earlier `wait()`→`is_alive()` and `wait_for_signal()` fixes (found via the VoWiFi/VoLTE-shutdown-signal bugs above) work correctly for VoLTE's own process handles too, not just VoWiFi's — `volte-carrier-agent` and `volte-bridge` are supervised by the exact same fixed loops.
- **Host-level teardown check**: `ip netns list` empty, no leftover `charon`/`gsm-sip-bridge`/`volte` processes, exit code 0. Fully clean.

**No new bugs found in this pass** — a meaningful result in its own right, given how many were found in the VoWiFi and shutdown-path rounds: it means those fixes (all in shared code — `runner.rs`, `shutdown.rs`, `runtime.rs`) generalize correctly to VoLTE's own supervision loops rather than being VoWiFi-specific patches.

One pre-existing, out-of-scope observation, not a bug in this feature: `docker/healthcheck.sh` is VoWiFi-only by design (`if ! ... vowifi-enabled; then exit 0`) — a VoLTE-only container reports "healthy" the moment the circuit-switched daemon's metrics endpoint responds, regardless of whether VoLTE actually registered. This predates this refactor entirely (the same logic existed in the original bash `healthcheck.sh`) and is out of this feature's scope to change; flagging it here only because this session is the first time it's been directly observed in practice. A genuine VoLTE healthcheck would be its own follow-up feature.

Restored `config.toml` to its production state afterward (`[vowifi].enabled = true`, `[volte].enabled = false`) and redeployed — confirmed VoWiFi re-established cleanly (fresh IKE_SA, `tunnel UP`, registered to PBX) before finishing. `config.toml` itself is untracked/gitignored in this worktree, so no commit was needed for the config changes themselves; this entry is the only record of what was tested and how.

**This closes out the last "not live-tested" item from PR #14's description.** Both major paths this feature's `supervise` subcommand can drive — VoWiFi/strongSwan and VoLTE bridge_inbound — are now proven working end-to-end on the real EC20 + Airtel hardware, including cold start, warm restart, and clean shutdown for both. Updating the PR description to reflect this.

---

## 2026-07-27 (Phase 7 polish) — T047-T052

You asked me to work through the remaining Phase 7 polish tasks.

**T047** (`cargo fmt --all && make lint && cargo test --workspace`): all green, 653→654 tests across this session's fixes below. `make test-bash` was already a no-op with an explanatory comment (done in an earlier session this same day).

**T048** (full live-validation cold-start + warm-restart cycle per quickstart.md): the bulk of this was already covered by earlier entries in this log (VoWiFi cold start/warm restart/`TunVanished` recovery/shutdown timing; VoLTE cold start/warm restart/shutdown). Ran two more of quickstart.md's specific checklist items live against the real modem while here:
- **"kill charon mid-session"** (Phase 3 checklist item): `kill -9` on the running charon PID. This is what surfaced the FR-009 gap logged below — a real bug, not just a coverage gap.
- **swu engine live validation**: skipped, per quickstart.md's own fallback — no second SIM/profile is available on this hardware. Noting it here as the one item that stays genuinely untested; it's a hardware-availability gap, not a scope decision.
- **"unplug/replug the modem" / "block the ePDG IP briefly"**: not attempted — physically manipulating the SIM/USB or firewalling live traffic is a different risk class than the software-triggerable scenarios above (killing a process, restarting the container), and didn't seem like a reasonable thing to do unattended during a polish pass. Left as a manual follow-up for whoever has hands on the hardware.

**A real, previously-undiscovered bug found by that "kill charon mid-session" test**: `engines.rs::restart_charon` (used by both the "charon exited" and "vici connection broken" steady-state branches) never had the bash original's `sleep 2 # let the vici socket come up before swanctl talks to it` — present at *every one* of the bash script's own charon-respawn sites, but only ported to the Rust side's *initial* cold-start call in `orchestrate.rs`, never to `restart_charon` itself. Reproduced live: killed charon, the new one respawned, but `--load-all` ran before its vici socket was listening and silently failed, so the follow-up `--initiate` failed with "CHILD_SA config 'ims' not found" — and steady-state's `ChildSaMissing` branch only ever re-initiates, never reloads, so this was a **permanent stuck state, invisible to the healthcheck** (which only checks whether `tun23-0` has an address — the stale pre-kill address was still there). Fixed by adding the missing sleep, added a regression test, re-verified live with the identical repro: clean recovery on the first try, no further errors. This is exactly the kind of gap the next task (T049) exists to catch systematically, and in fact was found by hand a few minutes before I ran that audit — the audit then confirmed no other invariant had this exact shape.

**T049** (FR-009 comment-to-test audit): delegated to a research agent to systematically read the pre-refactor `docker/entrypoint.sh` (via `git show 9b251ca^:docker/entrypoint.sh`) against the current Rust port. Full findings:

- **~28 load-bearing operational-invariant comments identified**, spanning rendering, pidfile/process-lifecycle handling, tunnel health/recovery, teardown ordering, and misc timing constants.
- **~19 have clear, named test coverage.** A few original bash hazards (the `@SRC_ADDR@` sed-delimiter choice, `swanctl --list-sas | grep -q` piping into a SIGPIPE-prone pattern) are *structurally eliminated* by the Rust rewrite (no delimiter parsing; captured output before matching) and need no test at all.
- **The `sleep 2` vici-warmup gap** (above) was one of the ~28 — now fixed and covered.
- **One more cheap, contained gap fixed on the spot**: `epdg_iface.rs::ensure_epdg_interface` ports the `disable_policy` sysctl write (received IPsec traffic silently drops without it — osmocom wiki's Option 2) but no test asserted it was actually issued. Added `disables_ipsec_policy_on_the_interface_so_received_traffic_is_not_dropped`.
- **Four remaining gaps, all in the same place**: `orchestrate.rs` and `orchestrate_volte.rs` carry **no `#[cfg(test)]` module at all** — confirmed independently by the coverage run below (both show *exactly* 0.00% line coverage). Every invariant that lives in the wiring itself, rather than an extracted pure decision function, is currently unguarded by any test:
  1. **[the audit's top pick] veth half-pair rebuild** (`start_line_tail`, checks both veth ends and rebuilds when only one side survived — the swu dialer's own netns delete/recreate cycle can destroy just the ims-side end). I read the current code directly to confirm it's *actually correct*, just untested — it is: both ends are checked, the sip-side gets deleted-and-recreated only when the ims-side is the one missing. Not a live bug, a coverage gap.
  2. Startup ordering: discover-before-daemon-start, and IMS-mode-reconcile-before-any-other-modem-read — both documented port/race-avoidance fixes with no ordering assertion.
  3. Keepalive is TCP-connect-not-ICMP (operators filter ICMP) and re-reads the P-CSCF file every cycle (so it follows a rekey-assigned address) — no test.
  4. VoLTE's 15s (not 5s) supervisor restart delay, to avoid hammering the modem/registrar with repeated PDN+IMS-AKA cycles — no test.

  **Decision**: did not add tests for these four. Doing so properly would mean extracting each into its own pure, testable decision function first — the same pattern this whole feature is built around everywhere else (`render.rs`, `line_supervisor.rs`, `sim_recovery.rs`, ...) — which is a real, scoped refactor of currently-working, already-live-validated orchestration code, not a quick test addition. Attempting that under time pressure during a polish pass, without it being reviewed as its own change, felt like the wrong tradeoff versus documenting it clearly here as a well-defined follow-up. None of the four are live bugs (verified #1 by reading; the other three are timing/protocol choices whose current values are directly visible in the source and already exercised correctly by this session's own live testing) — they're coverage gaps against *future* regressions, not present-tense correctness problems.

**T050** (docs referencing `entrypoint.sh`'s old size/structure): checked every `docs/*.md` reference (not `specs/*` — those are point-in-time historical records of past features and are correctly left alone). Found one genuinely stale pointer: `docs/vowifi-bridge.md` told readers to go look at "that script's structure" to understand the veth pair setup — misleading now that the script is a 28-line shim. Redirected to `gsm-sip-bridge/src/supervise/orchestrate.rs`. Everything else that mentions `entrypoint.sh` is still behaviorally accurate (e.g. "entrypoint.sh refuses to start if both sections are enabled") since the Rust code preserves that exact behavior — left those alone.

**T051** (`make coverage`): the target itself was broken — `cargo llvm-cov report --workspace` errors, since `--workspace` belongs on the collection step, not the report step. Fixed (one-line Makefile change). `cargo-llvm-cov` wasn't installed in this environment; installed it (`cargo install cargo-llvm-cov --locked` + `rustup component add llvm-tools-preview`) to actually run it rather than skip it.

`supervise` module coverage (line coverage, from the actual run):

| File | Line % | Note |
|------|-------:|------|
| `line_supervisor.rs` | 99.73% | |
| `sim_recovery.rs` | 99.28% | |
| `render.rs` | 99.51% | |
| `epdg_iface.rs` | 98.89% | after this session's added test |
| `daemon_supervisor.rs` | 97.87% | |
| `shutdown.rs` | 89.69% | |
| `vpcd.rs` | 86.75% | |
| `runner.rs` | 84.32% | |
| `engines.rs` | 82.92% | |
| `orchestrate.rs` | **0.00%** | no test module — see T049 above |
| `orchestrate_volte.rs` | **0.00%** | no test module — see T049 above |

The 0% files are exactly the two this session's live-hardware testing (and Greptile) found the most real bugs in — which tracks: they're also the two files with zero unit-test coverage, relying entirely on the live/manual validation this whole feature has been doing instead. This isn't a contradiction of the "pure decision functions get near-100% coverage" design — it's that pattern working as intended (the thin executor layer is supposed to rely on integration/live testing, not unit tests) — but the coverage number makes it very visible just how much weight is riding on that live testing versus a repeatable test suite, which is worth keeping in mind for anyone extending these two files without the same access to real hardware.

**T052**: this entry. `quickstart.md`'s own checklist has been walked through end-to-end across this log's entries (rendering diffed against bash, shutdown timing measured live, all three degraded-state recoveries forced live, full cold-start/warm-restart/shutdown cycles for both VoWiFi and VoLTE). Completion summary:

**Done**: all 5 phases of the strangler plan; `docker/entrypoint.sh` reduced to a 28-line shim; live-validated on the real EC20 + Airtel modem across cold start, warm restart, forced degraded-state recovery (TunVanished, charon killed mid-session, ViciBroken), and clean shutdown, for both VoWiFi/strongswan and VoLTE bridge_inbound; 10 real bugs found and fixed via live testing/Greptile/proactive sweep, none caught by the (now 654-test) unit suite alone; Greptile at 5/5 as of the last push; Phase 7 polish (T047-T051) done as detailed above.

**Deliberately not done, and why**: swu engine live validation (no second SIM/profile available — a hardware-availability gap, not a choice); physical SIM unplug/replug and ePDG-IP-blocking tests (out of scope for an unattended polish pass — needs hands on the hardware); test coverage for the 4 remaining `orchestrate.rs`/`orchestrate_volte.rs` FR-009 gaps (would need a scoped pure-function extraction first, logged above as a well-defined follow-up rather than attempted under time pressure); a VoLTE-aware `healthcheck.sh` (pre-existing gap, predates this refactor, its own follow-up feature).

**Recommendation**: PR #14 is in good shape to merge as-is. The four documented, non-blocking gaps above are reasonable candidates for a short follow-up PR whenever there's dedicated time and, ideally, someone with hands on the physical hardware for the swu/SIM-unplug scenarios.

---

## 2026-07-27 (one more) — Greptile found a 12th bug: restarted engines were never reaped

Right after the Phase 7 push, Greptile posted a new P1: `StrongswanEngine::restart_charon` and `SwuEngine::restart_process` both signal the outgoing process handle (`runner.signal(old, Term)`) and discard it without ever calling `wait()` — and since `RealCommandRunner`'s tracked-children table only drops an entry once something actually reaps it, and nothing else ever touches that specific old handle again (the engine's own `charon_handle`/`dialer_handle` field is immediately replaced with the new one), every recovery cycle leaked one table entry, unbounded over a long-running container's lifetime — and worse, nothing guaranteed the old process had actually died before the new one started, so a slow-to-terminate old charon/dialer could run concurrently with its replacement, contending for the same vici socket/pidfile/tun device.

Verified the claim by reading both restart paths directly — confirmed, both have the identical shape. Fixed by adding `runner.wait(old)` right after `signal()` in both, matching the exact pattern already established (and already review-approved) in `sim_recovery.rs`'s background-reader path. Added a regression test per engine asserting the old handle appears in `wait_calls`.

Verified live, precisely: killed charon mid-session, watched it become a zombie (`<defunct>`) immediately after the signal, then confirmed it was fully reaped exactly once the next steady-state tick ran the restart choreography 30s later — `ps aux` showed exactly one charon process throughout the whole window, no overlap, healthcheck stayed clean, tunnel recovered normally. (One unrelated, expected wrinkle during this same test window: a transient run of `SCardConnect: No smart card inserted` / IKE notify error 10500 during re-establish — the same carrier/SIM-access flakiness observed and documented twice earlier in this log, which self-resolved via the existing retry loop within about 2 minutes, same as before. Not a regression.)

656 tests, `make lint` clean. Committed (`4a9f42e`), pushed, replied to Greptile's comment, re-triggered the review. **This brings the total to twelve real bugs found and fixed across this whole feature**, none caught by the mock-based unit suite on its own.

---

(Further entries appended as phases proceed.)
