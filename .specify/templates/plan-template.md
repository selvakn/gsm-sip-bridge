# Implementation Plan: [FEATURE]

**Branch**: `[###-feature-name]` | **Date**: [DATE] | **Spec**: [link]
**Input**: Feature specification from `/specs/[###-feature-name]/spec.md`

**Note**: This template is filled in by the `/speckit.plan` command. See `.specify/templates/plan-template.md` for the execution workflow.

## Summary

[Extract from feature spec: primary requirement + technical approach from research]

## Technical Context

<!--
  ACTION REQUIRED: Replace the content in this section with the technical details
  for the project. The structure here is presented in advisory capacity to guide
  the iteration process.
-->

**Language/Version**: [e.g., Python 3.11, Swift 5.9, Rust 1.75 or NEEDS CLARIFICATION]  
**Primary Dependencies**: [e.g., FastAPI, UIKit, LLVM or NEEDS CLARIFICATION]  
**Storage**: [if applicable, e.g., PostgreSQL, CoreData, files or N/A]  
**Testing**: [e.g., pytest, XCTest, cargo test or NEEDS CLARIFICATION]  
**Target Platform**: [e.g., Linux server, iOS 15+, WASM or NEEDS CLARIFICATION]
**Project Type**: [e.g., library/cli/web-service/mobile-app/compiler/desktop-app or NEEDS CLARIFICATION]  
**Performance Goals**: [domain-specific, e.g., 1000 req/s, 10k lines/sec, 60 fps or NEEDS CLARIFICATION]  
**Constraints**: [domain-specific, e.g., <200ms p95, <100MB memory, offline-capable or NEEDS CLARIFICATION]  
**Scale/Scope**: [domain-specific, e.g., 10k users, 1M LOC, 50 screens or NEEDS CLARIFICATION]

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

[Gates determined based on constitution file]

## Blast Radius (required when the feature changes existing runtime state)

*GATE: fill in before Phase 0 research ends; re-check after Phase 1 design. Skip with "N/A: new code only" when nothing existing is changed. Born from #163, where 10 of 15 review findings were things an existing part of the system still assumed.*

### State and invariants inventory

For every field, connection, SA, timer, flag or predicate this feature **changes**, list each row. Use `grep -rn '<name>' <src>`; do not rely on memory.

| Changed item | Readers | Copies made at construction (dialogs, pending transactions, status snapshots, retired lists) | Invariant comments about it (`grep -n -i 'cannot change\|deferred while\|never changes\|always'`) | Decision: update at the transition / must not change / reuse existing function |
|---|---|---|---|---|

Also list **implicit contracts** with existing code: error text matched by a classifier, limits and retry counts in the existing path (reuse the constant and its check point, or match the semantics exactly), ordering rules, lock order.

### Concurrency and ownership map

Every thread, socket (including clones held by readers), kernel object and guard this feature creates or touches: who owns it, who holds a clone, and how it is released on success, error, cancel, rollback and shutdown. A guard that is disarmed early (`take`, `into_parts`) needs the error path after it spelled out.

### Where else does this logic run?

Blocking waits (ring wait, origination wait), shutdown, other transports (UDP and TCP), other lines. If the feature's logic must also run there, the plan says it **calls the same entry points** and extends the shared predicates (for example `busy()`); it never copies the logic.

### Situations checklist

Answer each for every new or changed behaviour: idle / in a call / ringing / origination pending / UDP and TCP / error and rollback / cancel / shutdown / limits and off-by-one against the existing path / blocks the dispatch loop (how long, and does the existing code already do the same) / docs, release notes and repo rules.

## Project Structure

### Documentation (this feature)

```text
specs/[###-feature]/
├── plan.md              # This file (/speckit.plan command output)
├── research.md          # Phase 0 output (/speckit.plan command)
├── data-model.md        # Phase 1 output (/speckit.plan command)
├── quickstart.md        # Phase 1 output (/speckit.plan command)
├── contracts/           # Phase 1 output (/speckit.plan command)
└── tasks.md             # Phase 2 output (/speckit.tasks command - NOT created by /speckit.plan)
```

### Source Code (repository root)
<!--
  ACTION REQUIRED: Replace the placeholder tree below with the concrete layout
  for this feature. Delete unused options and expand the chosen structure with
  real paths (e.g., apps/admin, packages/something). The delivered plan must
  not include Option labels.
-->

```text
# [REMOVE IF UNUSED] Option 1: Single project (DEFAULT)
src/
├── models/
├── services/
├── cli/
└── lib/

tests/
├── contract/
├── integration/
└── unit/

# [REMOVE IF UNUSED] Option 2: Web application (when "frontend" + "backend" detected)
backend/
├── src/
│   ├── models/
│   ├── services/
│   └── api/
└── tests/

frontend/
├── src/
│   ├── components/
│   ├── pages/
│   └── services/
└── tests/

# [REMOVE IF UNUSED] Option 3: Mobile + API (when "iOS/Android" detected)
api/
└── [same as backend above]

ios/ or android/
└── [platform-specific structure: feature modules, UI flows, platform tests]
```

**Structure Decision**: [Document the selected structure and reference the real
directories captured above]

## Complexity Tracking

> **Fill ONLY if Constitution Check has violations that must be justified**

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|-------------------------------------|
| [e.g., 4th project] | [current need] | [why 3 projects insufficient] |
| [e.g., Repository pattern] | [specific problem] | [why direct DB access insufficient] |
