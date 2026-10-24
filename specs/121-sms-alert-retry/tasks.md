# Tasks: SMS Alert Delivery Status and Retry

**Input**: Design documents from `/specs/121-sms-alert-retry/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md
**Tests**: Included — the project constitution (Principle I) requires integration tests, and the spec has measurable outcomes (SC-001…SC-006) that need them.

**Per-task rule (Constitution II/III, CLAUDE.md)**: finish each task or small group with `make format && make lint && make test`, then commit. All paths below are relative to `gsm-sip-bridge/` (the crate) unless they start with `docs/` or `specs/`. Test fixtures use placeholder numbers (`+919000000000`), never real ones.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: can run in parallel (different files, no dependency on an unfinished task)
- **[Story]**: US1–US4 map to the user stories in spec.md

---

## Phase 1: Setup

- [X] T001 Confirm baseline: on branch `121-sms-alert-retry`, run `make format && make lint && make test` and record that the tree is green before any change (use `CARGO_TARGET_DIR` per the shared-target-dir memory so another worktree's build does not clobber this one).

---

## Phase 2: Foundational (blocks all user stories)

**Purpose**: the persisted delivery state, typed failure reasons, and the config knobs. Nothing user-visible changes yet (no retry, behaviour identical to today).

- [X] T002 Add `SCHEMA_V5_SQL` and the `("4","5",…)` row in `MIGRATIONS`, bump `SCHEMA_VERSION` to `"5"` in `src/store/schema.rs`: rebuild `sms` as `sms_v5` with columns `attempt_count`, `last_attempt_at`, `last_error`, `first_seen_at`, `claimed_until`, `retry_eligible`, widen the `forwarding_status` CHECK to include `'abandoned'`, copy existing rows with `retry_eligible = 0`, recreate the indexes plus `idx_sms_retry`, and recreate the `recent_sms` view with the new columns (follow `SCHEMA_V2_SQL`'s rebuild pattern). Update the existing schema unit tests that assert version `"4"`.
- [X] T003 [P] Write `tests/test_sms_retry_store.rs` migration tests: a v4 database containing sent/failed/pending/skipped rows migrates to v5 with data intact and every migrated row `retry_eligible = 0`; a fresh database is v5; `abandoned` is accepted by the CHECK; re-running `init_schema` is a no-op. (Real SQLite files in a temp dir, no mocks.)
- [X] T004 Extend `src/store/sms.rs`: add `lease_until`/`retry_eligible`/`first_seen_at` to `SmsRecord` and `insert_sms` (rows inserted for a configured webhook are `pending`, `retry_eligible = 1`, `claimed_until = now + 120`, `attempt_count = 1`, `last_attempt_at = now`; no-webhook rows are `skipped`, `retry_eligible = 0`). Keep `StoreCommand::InsertSms` as the entry point; update the two call sites' construction of `SmsRecord`.
- [X] T005 Add the status-transition queries to `src/store/sms.rs` (plain functions on `&Connection`, each a single atomic statement): `claim_for_retry(conn, id, now, lease_secs) -> bool` (conditional UPDATE that also bumps `attempt_count`/`last_attempt_at`), `mark_sent`, `mark_failed(id, error, code)` (clears the lease), `release_claim(id)`, `abandon_expired(conn, now, max_age_secs) -> usize`, `candidates(conn, now, limit)` (predicate from data-model.md), and `summary(conn, now) -> SmsDeliverySummary` (counts per state, oldest undelivered age, last N undelivered/abandoned rows without bodies). Pre-v5 `failed` rows must be invisible to `candidates`/`abandon_expired`.
- [X] T006 [P] Extend `tests/test_sms_retry_store.rs` with store-API tests: two threads racing `claim_for_retry` on one row — exactly one wins; an expired lease can be re-claimed; a live lease cannot; `sent`/`skipped`/`abandoned` rows are never candidates; pre-v5 failed rows are never candidates; `abandon_expired` flips only rows older than the limit; candidates are ordered by `id`.
- [X] T007 Replace the string error in `src/alerts/discord.rs`'s `post_with_retry` with a typed `PostError` (`Unreachable`, `Timeout`, `Rejected(u16)`, `RateLimited { retry_after }`, `ServerError(u16)`) with `Display` reproducing today's exact messages and a `category()` giving the stable persisted strings from research R6 (never include the URL or response body). `send_alert` keeps returning `Result<u16, String>` via `Display`, so critical alerts are unchanged; `forward_sms` returns `Result<u16, PostError>`. Update `src/sms/mod.rs`'s match and the tests in `tests/test_alerts_discord.rs`.
- [X] T008 [P] Add the three `[sms]` keys (`retry_enabled`, `retry_interval_sec`, `retry_max_age_sec`) to `RawSms` + defaults in `src/config/raw.rs`, `SmsConfig` in `src/config/mod.rs`, with range validation and error messages naming `[sms].<key>` per `contracts/config.md`.
- [X] T009 [P] Extend `tests/test_config.rs` (defaults, overrides, out-of-range rejection, unknown-key still rejected) and add the keys to `config.toml.example` and `docs/configuration.md` so `tests/test_config_docs.rs` passes.

**Checkpoint**: tree green; behaviour unchanged except that rows now carry delivery metadata (`attempt_count`, `last_error` not yet populated on failure).

---

## Phase 3: User Story 1 — A failed text is eventually delivered (Priority: P1) 🎯 MVP

**Goal**: failed notifications are retried after the next alert and on an hourly timer, and are never re-sent once delivered.

**Independent Test**: fake Discord down → text recorded `failed` with `last_error`; Discord up → next text's sweep (or the timer) delivers it and marks `sent`.

- [X] T010 [US1] Add `forward_sms` late-delivery support in `src/alerts/discord.rs`: an `Option<LateInfo>`/`late: bool` argument producing the `[Late] ` title prefix and the `Originally received` field per `contracts/discord-payload.md`; the fresh-text payload must stay byte-identical (add a test asserting equality against a captured fresh payload).
- [X] T011 [US1] Create `src/sms/retry.rs` (declare in `src/sms/mod.rs`): `pub async fn sweep(db_path, client, cfg, handle) -> SweepOutcome { attempted, delivered, still_failing, abandoned, stopped_early }` that (1) opens a short-lived rusqlite connection with `busy_timeout`, (2) runs `abandon_expired`, (3) loops `candidates` → `claim_for_retry` → `forward_sms(late)` → `mark_sent`/`mark_failed`, oldest first, bounded batch (e.g. 50 per pass), and (4) logs one summary line per non-empty pass (FR-014; silent when nothing to do). DB work runs in `spawn_blocking`; never hold a connection across an `.await`.
- [X] T012 [US1] Change `record_and_forward` in `src/sms/mod.rs` so a failed first attempt records `last_error` (category) and `discord_status_code` and clears the lease via the store writer, and — after the first attempt finishes, never before — spawns `retry::sweep` (the "next alert" trigger; FR-003, FR-009). Skip when `retry_enabled = false`. Pass `SmsConfig` retry settings and `db_path` in through the existing arguments (no new globals).
- [X] T013 [US1] Add `retry::spawn_timer(handle, db_path, client, cfg)` using `tokio::time::interval(retry_interval_sec)` with `MissedTickBehavior::Delay` and the first tick one full interval after start (research R3). Start it in `src/commands/daemon.rs` where the SMS `DiscordClient` is created, and in `src/vowifi/mod.rs` on `sms_runtime` right after `build_discord_client` (agent B; covers VoLTE if it shares that path — verify `src/commands/volte.rs` and wire it too if it builds its own client).
- [X] T014 [US1] Write `tests/test_sms_retry_sweep.rs` with a scripted local HTTP listener standing in for Discord (comment at the mock site: real Discord can't be reached from CI; the real `reqwest` client is still exercised): (a) listener down → text recorded `failed`, `attempt_count = 1`, `last_error = unreachable`; (b) listener up → `sweep` delivers it, row `sent`, exactly one POST received; (c) a second `sweep` sends nothing more; (d) a retry that fails again keeps `failed`, increments `attempt_count`, updates `last_error`; (e) a new text triggers the sweep so the older failed text is delivered in the same pass; (f) `retry_enabled = false` never retries; (g) webhook unset → `skipped`, never retried even after a webhook is later configured.
- [X] T015 [US1] Add a timer test in the same file using `tokio::time::pause`/`advance`: with `retry_interval_sec = 3600` a failed row is retried at the first tick and not before; advancing the clock by several intervals at once produces one sweep, not a burst.

**Checkpoint**: US1 independently demonstrable and shippable (MVP).

---

## Phase 4: User Story 2 — The operator can see delivery status (Priority: P2)

**Goal**: delivery state is inspectable from the CLI and from Prometheus.

**Independent Test**: seed delivered/failed/abandoned rows; CLI and `/metrics` both show matching counts and oldest age.

- [X] T016 [P] [US2] Add the three gauges from `contracts/metrics.md` in `src/metrics/mod.rs` (register in the same registry as the other SMS metrics) and refresh them from `store::sms::summary` inside `metrics_handler` in `src/metrics/server.rs` using a short-lived read connection in `spawn_blocking`; on a read error log a warning and leave previous values.
- [X] T017 [P] [US2] Create `src/commands/sms_status.rs` and register `SmsStatus` (with `--json`) in `src/cli.rs` / `src/commands/mod.rs` per `contracts/cli.md`: read-only open of `[sms].db_path`, table output, never print bodies, exit non-zero only when the store can't be opened.
- [X] T018 [US2] Write `tests/test_sms_status_metrics.rs`: seed a store with a known mix, assert `summary` counts and oldest-age, assert the `/metrics` body contains the expected gauge lines (reuse the pattern in `tests/test_metrics_endpoint.rs`), and run the `sms-status` binary against the seeded file for both text and `--json` output (reuse `tests/test_cli.rs` helpers), including a check that no message body appears.

**Checkpoint**: US1 + US2 work together; an external monitor can alert on stuck texts (SC-004).

---

## Phase 5: User Story 3 — Restart-safe and duplicate-free (Priority: P2)

**Goal**: pending/failed state survives restarts; a delivered text is never re-sent; concurrent triggers and processes never double-post.

**Independent Test**: restart with failed rows → each delivered exactly once; two concurrent sweeps on one row → one POST.

- [X] T019 [US3] In `tests/test_sms_retry_sweep.rs` add: (a) drop all in-memory state, re-open the same DB file, run `sweep` → failed rows from before the "restart" are delivered once; (b) a `pending` row whose lease expired (simulated crash mid-send) is picked up and delivered; (c) a `pending` row with a *live* lease is skipped; (d) two `sweep`s started concurrently against one DB (separate connections, as separate processes would have) produce exactly one POST per row at the listener.
- [X] T020 [US3] Harden `retry::sweep` if T019 exposes gaps: ensure a lease is released on every exit path (error, rate-limit stop, task cancellation via a drop guard), and that `mark_sent` is a no-op if the row is already `sent`. Document the accepted at-least-once window (crash between Discord accepting and `mark_sent`) in a comment on `sweep`, citing the spec edge case.

**Checkpoint**: SC-002 verified.

---

## Phase 6: User Story 4 — Outages and backlogs don't flood anything (Priority: P3)

**Goal**: bounded, polite draining after a long outage; new texts are never queued behind the backlog.

**Independent Test**: 500 failed rows + listener returning 429 → pass stops early; listener healthy → backlog drains oldest-first while a new text is delivered immediately.

- [X] T021 [US4] In `retry::sweep`, stop the pass on the first `PostError::RateLimited`, release that row's lease, set `stopped_early`, and let the next trigger continue (FR-011). Cap rows per pass (the T011 batch) so a 500-row backlog drains across passes; keep the per-pass cap configurable only as a constant (YAGNI).
- [X] T022 [US4] Tests in `tests/test_sms_retry_sweep.rs`: (a) 429 on the 3rd request → exactly 3 POSTs, remaining rows untouched and un-leased; (b) 500-row backlog drains oldest-first over several passes, each embed carrying `[Late]` and the original `received_at`, in `id` order; (c) a fresh text arriving during a backlog is POSTed *before* any backlog row (first attempt is never delayed — FR-009); (d) rows older than `retry_max_age_sec` are marked `abandoned` with `last_error` preserved and are not POSTed (FR-010).

**Checkpoint**: SC-005 verified.

---

## Phase 7: Polish & cross-cutting

- [X] T023 [P] Update `docs/observability.md` (new gauges, suggested alert rule), `docs/operations.md` ("Discord forwarding failing" section: use `sms-status`, explain retry/abandon, replace the raw `sqlite` hint), and `docs/configuration.md` (already touched in T009 — add the behavioural paragraph on retry and the 7-day default).
- [X] T024 [P] Add a `CHANGELOG.md` entry: retry is **on by default**, pre-v5 failed rows are not retried, schema auto-migrates v4→v5 (and note the one-way migration for downgrade). Draft the RELEASE_NOTES line with only the feature name bolded, per the release-notes memory.
- [ ] T025 Run through `quickstart.md` manually in the local docker test dir (`test/`, pinned GHCR tag, own config/volume): dead webhook → `failed`; restore → `[Late]` message arrives; restart mid-failure → no duplicate. Record the result in `specs/121-sms-alert-retry/quickstart.md`.
- [X] T026 Final gate: `make format && make lint && make test`; confirm `git grep -n '+91[0-9]\{10\}'` shows only placeholder numbers in new files; confirm SC-006 by running the pre-existing SMS tests unmodified apart from the signature changes in T004/T007.

---

## Dependencies & Execution Order

- **Phase 1 → Phase 2 → stories → Polish.** Phase 2 blocks everything.
- Within Phase 2: T002 → T004 → T005 (T003 after T002, T006 after T005); T007, T008 are independent of the schema work and of each other (T009 after T008).
- **US1** (T010–T015) needs Phase 2 complete; T011 needs T005 + T007 + T010; T012–T013 need T011.
- **US2** (T016–T018) needs only T005 (summary) — can start in parallel with US1 once Phase 2 is done.
- **US3** (T019–T020) needs US1's sweep (T011).
- **US4** (T021–T022) needs US1's sweep; independent of US2/US3.
- Polish after the stories you intend to ship.

## Parallel opportunities

- After T002: T003, T007, T008 in parallel (different files).
- After Phase 2: US1 and US2 in parallel (T016, T017 touch metrics/CLI only).
- T023 and T024 are docs-only and parallel.

## Implementation strategy

- **MVP = Phase 1 + 2 + US1.** That alone delivers "a failed text is eventually delivered, restart-safely (state is in SQLite) and without duplicates between the first attempt and the sweep (lease-on-insert)".
- Then US2 (visibility), US3 (explicit multi-process/restart proof), US4 (backlog politeness), each its own PR-sized commit series.
- Never narrow `make lint`'s scope (CLAUDE.md); test targets are linted too.
