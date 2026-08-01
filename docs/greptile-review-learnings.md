# Greptile Review Learnings

Distilled from **every Greptile review this repo has received**: 67 inline
findings (58 P1, 8 P2, 7 security-tagged) across PRs #1–#21, plus the PR-level
review summaries and the round-by-round narrative in
`specs/021-entrypoint-supervise-rust/DECISIONS-LOG.md`.

**Who this is for**: coding agents (and humans) working in this repo. Read
[Part 1](#part-1--pre-pr-self-review-checklist) *before* opening a PR — it is
the cheapest way to not get the same finding a ninth time. Read
[Part 2](#part-2--the-recurring-defect-classes) when you touch one of the named
subsystems. Read [Part 3](#part-3--running-the-greptile-loop) when you are
driving the review loop.

> **The single most important fact in this document**: across
> `specs/021-entrypoint-supervise-rust` alone, **ten real bugs** were found —
> and **650+ passing unit tests caught none of them**. Every one lived in the
> gap between a mock and the real thing, or between a test fixture and real
> device/daemon output. A green `make test` is not evidence about the code paths
> this repo's bugs actually live in.

---

## Part 1 — Pre-PR self-review checklist

Run this over your own diff before pushing. Each line is a defect class that
Greptile has *actually* found here, at least once, and usually more.

### Process & handle lifecycle (biggest cluster — 10+ findings)

- [ ] Every long-running child whose handle must stay reachable by a *second*
      thread (shutdown plan, recovery routine, SIM recovery) is supervised by
      **polling `is_alive()`**, never by blocking `wait()`. `RealCommandRunner::wait`
      removes the handle from the tracked table *before* blocking, by design — so
      a later `signal()` silently no-ops and shutdown proceeds against a live process.
- [ ] Every fire-and-forget spawn uses `spawn_detached` (never inserts a tracked
      entry, dedicated reaper thread), not `spawn` with a discarded handle —
      a discarded handle leaks a table entry *and* a zombie, per invocation, forever.
- [ ] Every restart path **reaps the old child before spawning its replacement**
      (signal → wait → spawn), not signal → spawn.
- [ ] `argv[0]` is an executable, never `KEY=value` — `Command::new` takes
      `argv[0]` literally and fails `ENOENT`. Use `env` as `argv[0]` for env prefixes.
- [ ] Any check-then-signal sequence holds the child-table lock across *both*,
      including the `kill` subprocess call.
- [ ] Shutdown cannot race a respawn: a supervisor must not be able to register
      a replacement child *after* the shutdown plan snapshots state. Gate
      respawn on the shutdown flag, and make the flag check atomic with recovery.
- [ ] A SIGSTOPped process **keeps its fds and `TIOCEXCL` locks**. Stopping a
      modem holder does not release the serial port. Neither does killing a
      process you never `wait()`ed for.

### Recovery loops that detect but never fix, or never escalate

- [ ] Every readiness deadline has an *action* at expiry — exit non-zero so the
      container restarts, or genuinely retry. "Log a warning and fall through to
      `wait`" leaves the container alive, so Docker's restart policy never fires
      and the service is silently dead. (Found 3× in `entrypoint.sh`.)
- [ ] Every process that matters after first success is still supervised after
      first success — charon, the SWu dialer, the IMS agent. "It came up once" is
      not a lifecycle.
- [ ] Every state a detector can enter has a path *out*: a detected
      `TunVanished` must actually call `recreate_interface`; a `GivenUp` module
      must be able to transition back to healthy on rediscovery **and** on the
      normal retry path.

### Latching state (set once, never reset)

- [ ] Any `bool`/flag set on one event and cleared on one other event: enumerate
      **all** exit paths. `has_active_call` was set on `CallInProgress` and cleared
      only on `Hangup` — SIP rejection, dial timeout, network drop, and error
      branches all latched it true forever, permanently deferring that slot.
- [ ] Do not mark "notified" optimistically. Mark it only when delivery is
      *confirmed*; on exhausted retries, reset so the incident can re-fire.
- [ ] Any async callback that resolves shared incident state carries a
      **generation/epoch**. Otherwise an in-flight callback for incident N
      resolves incident N+1 and permanently suppresses it.
- [ ] Persisted "how to undo this" state (e.g. a displaced-CID restore file)
      survives *every* intermediate path: mid-attach shutdown, attach error,
      renewal after a carrier drop, a later attach that finds no prior binding.
      PR #6 produced **five** distinct variants of this one bug.

### Config validation vs. runtime reality

- [ ] Every configured maximum is checked against the **real** runtime capacity,
      not an arbitrary ceiling (`max_lines` accepted 64; the image builds 8 vpcd slots).
- [ ] Every derived resource (`base + k*index` for ports, veth addresses, if_ids)
      **fails loudly on overflow** instead of silently falling back to the base
      value — silent fallback means two lines share one port/address.
- [ ] Port-collision validation covers **hard-coded** ports too, not just the
      other configurable one (server listen port vs. the 5072/5073 telephony agents).
- [ ] Exactly one owner binds each listening port. When two subsystems can both
      be enabled, arbitration must name a single owner — otherwise the loser gets
      `EADDRINUSE` and retries forever.
- [ ] A value read from TOML is actually *used* on every path. Config that the
      entrypoint overrides with independent env defaults is config that lies.
- [ ] Every override bypassing a derived value gets the **same validation** the
      derived path had (whitespace/digits/length for IMSI, IMEI).
- [ ] Every manifest/IPC serializer round-trips **all** fields its consumers
      reconstruct from. A dropped `msisdn` becomes `None` in two agents at once.

### Auth & security

- [ ] **Verify credentials before mutating any state.** Never advance a nonce
      counter, consume a one-shot nonce, or touch a binding before the digest
      compares equal. Otherwise an attacker poisons a victim's replay counter
      with a forged high `nc`.
- [ ] Replay state is keyed by **(account, nonce)**, not nonce alone. A digest
      valid for *any* configured account must not be able to advance another
      account's counter.
- [ ] Partial/degraded auth inputs (`qop=auth` with no `nc`/`cnonce`) must not
      route to a path that skips the replay guards. Reject, don't fall back.
- [ ] Teardown/deregistration requests carry the **same** authorization and
      negotiated security headers the successful request did — and their
      responses are *checked*, not logged as success unconditionally.
- [ ] No fallback path escapes the negotiated protection. A reconnect to the
      original endpoint that no longer matches the installed XFRM selector sends
      the request in the clear or not at all. Fail closed.
- [ ] A **failed query is not an empty result.** Converting a failed
      `ip xfrm state` dump into `""` let an "is all this state ours?" check
      classify a half-empty inventory as `AllOurs` and flush a stranger's IPsec.
      Any failed inventory read must veto the destructive action.
- [ ] Zero real subscriber/device identifiers (MSISDN, IMSI, IMEI, phone
      numbers) in docs, incident reports, config examples, or commit messages —
      git history is permanent. Sanitize *before* the branch is pushed.
- [ ] Third-party CI actions pinned to an **immutable commit SHA**, never a
      movable tag — they run with access to the checkout, env, and build caches.

### Metrics & observability

- [ ] A label-set change is applied at **every** `with_label_values` call site.
      A missed teardown path raises a cardinality error at runtime and can kill
      the worker instead of clearing a gauge.
- [ ] Metrics are registered in the process that actually serves `/metrics`.
      Per-line gauges registered in a short-lived agent process are never scraped.
- [ ] Distinct semantic outcomes get distinct label values (`SlotDisappeared`
      vs. `NonReady` both emitting `"skipped-non-ready"` makes hot-unplug
      un-alertable).
- [ ] Every bounded queue's *upstream* is bounded too — a 1024-entry `VecDeque`
      behind an unbounded channel is not backpressure.
- [ ] Every retry-on-no-ack carries an **idempotency key**. A lost
      acknowledgement is indistinguishable from an unapplied request, so the
      retry double-applies.
- [ ] Threshold-crossing transitions are evaluated on **ingest** (push), not on
      `/metrics` scrape (pull). Scrape-driven evaluation misses any incident that
      recovers between two scrapes, and never fires at all with no scraper
      configured. (Staleness/liveness *is* correctly a pull question — the
      distinction matters.)

### Concurrency & shared devices

- [ ] One writer per SQLite database, or `SQLITE_BUSY` is retried/waited on —
      WAL still permits only one writer, and an unretried busy error was being
      logged as an insert error, permanently dropping call history.
- [ ] Stateful multi-APDU card sequences (SELECT then READ) run inside a
      **PC/SC transaction**, not `ShareMode::Shared`. Concurrent probes
      interleave and read the wrong file.
- [ ] Device selection is bound to the **line's identity** (match on IMSI), not
      "the first reader that isn't vpcd" — otherwise line 1 authenticates with
      line 0's SIM.
- [ ] Only one process holds a serial port at a time, *including during
      teardown*. Cleanup must wait for the child to actually release the modem
      before issuing its own AT traffic — polling for 5s and proceeding anyway is
      still a race.
- [ ] No blocking sleep or synchronous network call on a thread that must keep
      dispatching. A 120s registration-renewal backoff on the only dispatch
      thread means inbound INVITEs get no `100 Trying` and the carrier drops the
      call; a synchronous `swanctl --initiate` stalls the whole supervisor tick;
      a synchronous `send_and_recv` in cleanup adds a 5s transport timeout to
      shutdown on exactly the failure path where no response is coming.

### Parsers — leniency where it matters, strictness where it matters

- [ ] Protocol parsers accept **all valid forms**, not just the common one.
      Rejecting RTP with the padding or header-extension bit set (both valid,
      RFC 8285) silently drops the media and produces one-way audio.
- [ ] Log/output scrapers are **unanchored** unless the real emitter is anchored.
      `strip_prefix` on a charon P-CSCF line never matched, because real charon
      prefixes every line with a facility tag (`[CFG] ...`) — the test fixture
      was a clean unprefixed line. Cost: an infinite 30s re-establish loop, found
      only on live hardware.
- [ ] "Most recent overall", not "most recent within a preferred family". A
      family-preference pass over the whole history returned a stale IPv4 P-CSCF
      after an IPv6-only re-auth.
- [ ] Ports of `sed`/`grep` preserve **order-sensitivity**: `/local_addrs.*@SRC_ADDR@/d`
      means "in this order", not "contains both".
- [ ] Modem responses reject command echoes and implausible values (an IMEI must
      be 14–16 digits, not the literal string `AT+CGSN`).

### Docs & release plumbing

- [ ] `RELEASE_NOTES.md` uses the **versioned `## vX.Y.Z` heading**, not
      `## Unreleased` — the release workflow extracts the section matching the
      tag, so `Unreleased` notes are silently dropped. (Flagged on #19 *and* #20.
      This is a repo rule in `.cursor/rules/release-notes.mdc`.)
- [ ] Test names still describe what the predicate now does
      (`only_child_sa_missing_skips_the_agent_restart` after the predicate grew a
      second condition).

---

## Part 2 — The recurring defect classes

Ranked by how often Greptile found them here. If you are touching the named
subsystem, assume the class applies until you have checked.

| # | Class | Where it bit | Count |
|---|---|---|---|
| 1 | Child-handle lifecycle (`wait()` vs `is_alive()`, unreaped detached children, PID identity, shutdown-vs-respawn races) | `supervise/runner.rs`, `orchestrate.rs`, `orchestrate_volte.rs`, `engines.rs`, `sim_recovery.rs` | ~10 |
| 2 | Undo/restore state not preserved across every intermediate path | `volte/mod.rs`, `volte/bridge.rs`, `entrypoint.sh` | 5 |
| 3 | Detector with no repair, or deadline with no escalation | `entrypoint.sh` readiness loops, `line_supervisor.rs`, `modules/mod.rs` (`GivenUp`) | 5 |
| 4 | Auth ordering / replay-state scoping | `sip/server/auth.rs` | 3 |
| 5 | Config accepted that runtime cannot honor (capacity, port/address collisions, overflow) | `config/mod.rs`, `config/build.rs`, `vowifi/discovery.rs`, `sip/mod.rs` | 6 |
| 6 | Latching flags & optimistic "already handled" marks | `modules/mod.rs`, `metrics/ingest.rs` | 4 |
| 7 | Blocking the only thread that must keep serving | `ims/agent.rs`, `ims/mod.rs`, `supervise/engines.rs` | 4 |
| 8 | Shared device/DB access without exclusion | `store/mod.rs`, `modules/pcsc_card.rs`, `entrypoint.sh` (serial) | 5 |
| 9 | Metric label/registry/queue correctness | `metrics/*`, `observability/reporter.rs` | 6 |
| 10 | Parser too strict, too anchored, or order-blind | `ims/rtp.rs`, `supervise/render.rs`, P-CSCF extraction, `at_commander.rs` | 5 |
| 11 | Security hygiene (PII in git, mutable CI tags, failed-query-as-empty) | docs, `.github/workflows/ci.yml`, `epdg_iface.rs` | 3 |
| 12 | Release/doc plumbing | `RELEASE_NOTES.md`, test names | 3 |

### The four-times bug — worth internalizing

`RealCommandRunner::wait()` removes a child's handle from the tracked table
*before* blocking (deliberately, to close a PID-reuse race). Four separate
supervision loops — the USIM holder, three VoLTE loops, then three more in
`orchestrate.rs` — each spawned a child, stored its handle for the shutdown plan
to signal later, then immediately blocked on `wait()`. The stored handle was
already gone, so:

- `TeardownStep::KillChild` silently no-opped;
- `TeardownStep::WaitForExit` polled `is_alive()` on an untracked handle, got
  `false` on the first poll ("not found", not "confirmed dead"), and proceeded
  straight into `volte-cleanup` while the real process still held the modem;
- SIM recovery's SIGSTOP/SIGCONT modem exclusion never happened at all.

`MockCommandRunner::wait()` does **not** remove its entry — which is precisely
why 650 green tests said nothing. **Rule: poll `is_alive()`, never block
`wait()`, for any process whose handle must stay externally signalable.**

### Fixing one bug can un-hide the next

Once the signals actually reached their targets, their *timing* began to matter
for the first time — and `docker compose stop` was measured at **10.15s**,
because `orchestrate::run` reused `runtime::wait_for_shutdown`, whose
unconditional 10s grace sleep was written for `main.rs`'s async leaf daemon
(which has in-flight tokio work to drain) and makes no sense for a supervisor
whose children are separate OS processes. `execute_shutdown_plan` didn't run a
single step until after that sleep — leaving ~0s of Docker's own 10s grace
period for real teardown. Split into a lean `wait_for_signal()`: **0.169s**.

**Rule: a shared helper's *assumptions* must be re-checked at each new call
site, not just its type signature.**

---

## Part 3 — Running the Greptile loop

### Mechanics

- Review **re-runs automatically on every push** to the PR branch. To force a
  re-run without a push, hit the retrigger URL in the footer of Greptile's
  summary comment (`https://app.greptile.com/api/retrigger?id=<id>`).
- **100-file limit.** Over it, Greptile posts a `greptile-status` comment and
  skips the review. Bypass by commenting `@greptile-apps please review` — and
  make that comment *useful*: PR #18 (118 files) named the five files most worth
  attention and asked two specific questions ("is `SharedCharon`'s
  check-and-spawn genuinely race-free across per-line supervisor threads? can the
  XFRM reclaim ever clear state that isn't ours?"). Both findings that came back
  were on exactly those files.
- **Greptile prunes obsolete inline comments.** Findings named in an older
  summary can be absent from the current inline set. If you need the full
  history, the PR-level summary comment is edited in place (only the latest
  version survives) — the durable record is a decisions log in the spec folder.
- Confidence score is on the summary comment. `5/5` + "no blocking failure
  remains" is the merge signal; `4/5` names the one blocker under **Files
  Needing Attention**.
- Fetch findings with:
  `gh api repos/<owner>/<repo>/pulls/<n>/comments --paginate --jq '.[] | select(.user.login=="greptile-apps[bot]")'`

### How to respond to a finding

1. **Verify it against the code first — every time.** Read both sides of the
   claim. Greptile has been right on the substance and slightly wrong in the
   phrasing: it described `WaitForExit` as "waiting through its exit timeout"
   when it actually returns instantly. Fix the *bug*, not the sentence.
2. **Fix, then add the regression test at the right level.** If the finding
   exists because a mock doesn't model reality, the test must use the real
   implementation — the PID-lifecycle findings needed genuinely spawned
   processes, since a mock has no concept of PID reuse to catch it with.
3. **Sweep for the same shape before pushing.** Greptile finds one call site per
   round. Grepping the module for the pattern yourself found 3 more instances of
   the `wait()` bug in one pass — and the shutdown-timing bug behind them.
4. **Push back with evidence when it's wrong, and don't iterate reflexively.**
   The round-4 PID-reuse finding was genuinely inapplicable: POSIX reserves a
   zombie's PID until its parent reaps it; the child-table mutex covers the only
   reaper; and a codebase grep confirmed no `SIGCHLD` handler and no
   `tokio::process`. Documenting that inline + replying beat pushing a "fix" for
   a race that doesn't exist. Greptile accepted the argument and offered to
   remember the rule.
5. **Reply on the thread** (`gh api .../pulls/<n>/comments/<id>/replies -f body=...`)
   naming the fixing commit — even for a false positive. It informs the next
   round and closes the thread.
6. **Know when to stop.** When a round returns the *same* comment IDs restating
   the same point in different words, with every actionable finding fixed and CI
   green, the loop is done. A documented, reasoned engineering disagreement is
   not an unaddressed bug. (Also: don't add a dependency — e.g. `rustix` for
   pidfd signaling — unilaterally to close a reviewer's theoretical race at the
   tail of an autonomous session. Flag it as the owner's call.)
7. **Feed the learning back into the artifacts**, not just the code. Findings on
   PR #16 were written back into `specs/022-*/research.md` (R8) and
   `data-model.md` as "**Revised post-review**" sections, and PR #2's P-CSCF
   finding became a named regression test carrying the original bug as its
   fixture. This repo's spec folders are the memory.

### Two levers not yet used here

- **`.greptile/rules.md` + `.greptile/config.json`** encode accepted invariants
  and house style repo-wide, so a settled question stops being re-flagged.
  Greptile explicitly offered to remember the PID-reuse rule; nothing captured
  it. Worth creating — start from Part 1 of this document.
- Greptile appends *"If this suggestion doesn't match your team's coding style,
  reply and let me know — I'll remember it for next time"* to style-adjacent
  findings. Replying is how that memory gets written.

---

## Part 4 — The meta-lessons

1. **Green mock-based tests are not evidence about the real runner.** Ten bugs,
   650+ passing tests, zero caught. Every one was a real-runner-vs-mock or
   real-output-vs-fixture blind spot. Wherever a mock stands in for something
   real, add at least one test against the real implementation — and make the
   mock's *divergences* from the real thing explicit (the whole `wait()` family
   of bugs traces to `MockCommandRunner::wait` not removing its entry).
2. **Verify ports against the real thing, not your own fixture.** The
   `sed`-ordering bug was caught by diffing the Rust renderer's output against
   the *actual* `sed` pipeline on the *actual* template — a check that a
   hand-written fixture could never have made.
3. **Some bug classes only appear on live hardware.** The charon facility-tag
   prefix and the warm-restart `tun` desync were both invisible to unit tests
   and to code review, and both presented as the *same* symptom (a fresh IKE_SA
   every 30s forever) with different root causes. Budget a real deploy.
4. **The same root cause recurs.** After fixing one, grep the module for the
   shape. Assume there are more.
5. **Run the full gate, not just `cargo test`.** `make lint` hard-fails on any
   `unsafe` in `gsm-sip-bridge/src` and on rustfmt violations in test files —
   both have caused broken commits. A green test run does not imply a green lint
   run in this repo. (See `CLAUDE.md`.)
6. **Live-testing has its own gotchas.** `docker compose up -d` does **not**
   pick up an edited bind-mounted config — compose only recreates on a
   *service-definition* change. Use `docker compose restart`. And the two
   subsystems' independent discovery can land on *different* serial ports for the
   same physical modem.
7. **Report scope honestly.** Several DECISIONS-LOG entries explicitly separate
   "all tests green" from "verified on hardware", and name what was *not*
   exercised (VoLTE while `[volte].enabled = false`; the STOP/CONT recovery path,
   deliberately not forced against a SIM with a documented drop history). That
   distinction is what makes the log worth reading — preserve it.

---

## Source material

- All 67 inline findings: `gh api repos/selvakn/gsm-sip-bridge/pulls/<1..21>/comments --paginate`
  (filter `user.login == "greptile-apps[bot]"`).
- Per-PR summaries + confidence scores: the same PRs' `issues/<n>/comments`.
- Round-by-round narrative, including the false positive and the stopping
  decision: `specs/021-entrypoint-supervise-rust/DECISIONS-LOG.md`.
- Findings written back into design docs: `specs/022-discord-critical-alerts/research.md`
  (R8), `specs/022-discord-critical-alerts/data-model.md`, `docs/todo.md`.
