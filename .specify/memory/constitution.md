<!--
  Sync Impact Report
  ==================
  Version change: N/A (initial) -> 1.0.0
  Bump rationale: MAJOR - Initial ratification of project constitution.

  Modified principles:
    - (new) I. Integration-First Testing
    - (new) II. Green-on-Commit
    - (new) III. Frequent Atomic Commits
    - (new) IV. Makefile-Driven Build
    - (new) V. Simplicity & Refactorability

  Added sections:
    - Core Principles (5 principles)
    - Build & Tooling Standards
    - Development Workflow
    - Governance

  Removed sections: None

  Templates requiring updates:
    - .specify/templates/plan-template.md           ✅ compatible (no changes needed)
    - .specify/templates/spec-template.md            ✅ compatible (no changes needed)
    - .specify/templates/tasks-template.md           ✅ compatible (no changes needed)
    - .specify/templates/checklist-template.md       ✅ compatible (no changes needed)
    - .specify/templates/agent-file-template.md      ✅ compatible (no changes needed)

  Follow-up TODOs: None
-->

# Audio Echo Constitution

## Core Principles

### I. Integration-First Testing (NON-NEGOTIABLE)

- All tests MUST exercise real components through actual interaction,
  not through mocked boundaries.
- Mocks are permitted ONLY for external services that are impractical
  to run locally (e.g., third-party paid APIs, hardware not available
  in CI).
- When a mock is introduced, it MUST include a written justification
  in a comment at the mock site explaining why a real component cannot
  be used.
- Test coverage MUST aim for >90% through integration tests.
- Refactoring internal implementation MUST NOT break tests as long as
  external behavior is preserved.

**Rationale**: Mock-heavy test suites become brittle and resist
refactoring. Integration tests validate real behavior and provide
confidence during structural changes. The ability to refactor freely
is a first-class project value.

### II. Green-on-Commit (NON-NEGOTIABLE)

- Every commit MUST have the full test suite passing.
- Run `make test` and confirm zero failures before every commit.
- A commit with known failing tests is prohibited regardless of
  circumstance (including "will fix later" or "unrelated failure").
- CI pipelines MUST enforce this gate; commits that break the build
  MUST be reverted or fixed immediately.

**Rationale**: A broken commit poisons the project history. When every
commit is green, any commit can serve as a safe rollback point, and
`git bisect` remains a reliable debugging tool.

### III. Frequent Atomic Commits

- Commits MUST be small, focused, and represent a single logical
  change.
- Each commit message MUST clearly describe the "why" of the change,
  not just the "what."
- Commit after completing each task or logical unit of work; do not
  accumulate unrelated changes.
- Large commits that mix multiple concerns (e.g., feature + refactor
  + formatting) are prohibited.

**Rationale**: Small commits simplify code review, bisection, and
rollback. They create a readable, navigable project history that
serves as living documentation of decisions.

### IV. Makefile-Driven Build

- All build, test, run, and common development operations MUST be
  exposed as Makefile targets.
- Required minimum targets: `build`, `test`, `run`, `clean`, `lint`.
- The Makefile is the single entry point for all project operations.
  Developers MUST NOT need to memorize tool-specific commands.
- Each target MUST be documented with a brief description (accessible
  via `make help` or equivalent).

**Rationale**: A Makefile provides a language-agnostic, discoverable
interface for project operations. Any contributor can read the
Makefile to understand available operations without prior knowledge of
the specific toolchain.

### V. Simplicity & Refactorability

- Favor straightforward implementations over clever abstractions.
- YAGNI: Do not build features, layers, or infrastructure until they
  are demonstrably needed.
- Code structure MUST support easy refactoring without cascading test
  failures (enabled by Principle I).
- When choosing between two approaches of equal correctness, choose
  the one with fewer moving parts.

**Rationale**: Premature abstraction creates accidental complexity.
Keeping things simple and validated through integration tests allows
the codebase to evolve freely as real requirements emerge.

## Build & Tooling Standards

- The project Makefile MUST reside at the repository root.
- All Makefile targets MUST be idempotent where possible (running them
  twice produces the same result).
- The `test` target MUST run the full integration test suite and
  return a nonzero exit code on any failure.
- The `build` target MUST produce a runnable artifact or confirm the
  project compiles without errors.
- The `clean` target MUST remove all generated artifacts and return
  the workspace to a pristine state.
- Dependencies MUST be declared in a version-pinned manifest
  appropriate to the language (e.g., `requirements.txt`,
  `package.json`, `go.mod`).
- Add additional Makefile targets as the project grows (e.g.,
  `format`, `docker-build`, `docs`), but never remove the required
  minimum set.

## Development Workflow

- The commit workflow follows this strict sequence:
  1. Write or modify code.
  2. Run `make test` and confirm all tests pass.
  3. Run `make lint` and confirm no violations.
  4. Stage changes and commit with a descriptive message.
- Test-Driven Development (TDD) is the default practice:
  1. Write a failing test that defines the desired behavior.
  2. Implement the minimum code to make the test pass.
  3. Refactor while keeping all tests green.
- Branch naming, PR process, and review requirements are deferred
  until the team workflow is established but MUST be documented in
  this section when adopted.
- Every developer MUST be able to go from clone to running tests in
  under 5 minutes using only `make` targets.

## Governance

- This constitution is the highest-authority document for development
  practices in this project. It supersedes ad-hoc decisions, verbal
  agreements, and informal conventions.
- Amendments require:
  1. A written proposal describing the change and its rationale.
  2. Update to this document with a version bump following semantic
     versioning (MAJOR for principle removal/redefinition, MINOR for
     additions/expansions, PATCH for clarifications/typos).
  3. All dependent templates and documentation MUST be reviewed for
     consistency after any amendment.
- Compliance review: Every code review and PR MUST verify adherence
  to these principles. Violations MUST be flagged and resolved before
  merge.
- Complexity justification: Any deviation from Principle V (adding
  layers, abstractions, or indirection) MUST include a written
  justification in the relevant plan or PR description.

**Version**: 1.0.0 | **Ratified**: 2026-05-02 | **Last Amended**: 2026-05-02
