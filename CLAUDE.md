<!-- SPECKIT START -->
For additional context about technologies to be used, project structure,
shell commands, and other important information, read the current plan at
`specs/123-ims-registration-refresh/plan.md`.
<!-- SPECKIT END -->

## Pre-commit Checklist

**MANDATORY — run before EVERY commit, no exceptions:**

```bash
make format              # fix formatting in place
make lint                # clippy --workspace --all-targets -D warnings, + deny/shellcheck/unsafe
make test                # all tests must pass
```

Do NOT commit if any of these fail. `make lint` failing has caused broken
commits in the past (e.g. rustfmt line-length violations in test files).

`make lint` covers the **whole workspace including all test targets** — a
warning in an integration test or a `#[cfg(test)]` module fails the build
exactly like one in production code. Do not narrow its scope back down.

## No real personal data in version control

This is a public, open-source repository. **Never commit real phone numbers,
MSISDNs, IMSIs, ICCIDs, or any other personal/subscriber identifiers** — not in
source, tests, fixtures, spec docs, logs, or commit messages.

Use obviously-synthetic placeholders instead:

- Phone numbers / MSISDNs: `+919000000000` (line 1), `+919000000001` (line 2),
  and so on. The classic `+919876543210` example is also fine.
- These are clearly fake and won't collide with any real subscriber.

If you find a real number already in the tree, scrub it from the working tree
with a placeholder (rewriting git history is a separate, deliberate decision).

## Before planning and before every PR

Read `docs/greptile-review-learnings.md` Part 1 before `/speckit-plan` and before every push, and note which classes apply. A feature that changes existing runtime state needs the plan's **Blast Radius** section filled in with real `grep` output, not from memory.

**Fix the class, not the instance.** After any review finding, grep for every site of the same shape and fix or justify each, and re-read every sentence of the finding before replying "Addressed". Never close a finding by documenting it as a known limitation without the maintainer's agreement. A requirement narrowed during implementation is recorded as a deviation in `tasks.md`, never ticked as done.

**Self-review over the whole diff, not one lens** (all of these have been found in review here):
- For every field changed on `RegisteredSession`, `LoopState` or similar: `grep -rn '<field>' src/` and check each reader and each struct that **copies** it at construction (dialogs, pending origination, status). Is it updated at the transition, or deliberately left? Does an invariant comment about it still hold?
- For every new error string: `grep -rn 'contains(\|starts_with(' src/ims`. Does an existing classifier depend on the wording?
- For every limit or constant: is there an existing one (`grep -rn 'MAX_'`), and is it checked at the same step?
- For every blocking wait: does it call the loop's own entry points, and do predicates such as `busy()` see it?
- For every guard, `take()` or `into_parts()`: can anything fail after it is disarmed, and who releases the resources then?
- For every thread or socket clone: who closes it on error, cancel and rebuild?
- After a state transition (refresh, rebuild, switch, rollback): what does the **next request of every in-flight kind** (BYE, UPDATE, CANCEL, PRACK, SUBSCRIBE) carry: Via sent-by, Security-Verify, connection, route?
- Docs: does every claim in `live-verification.md`, the learnings doc and release notes match the evidence, and the repo's release-note rule?

`greploop` and `/code-review` are run only when the maintainer asks for them.

**Scratch files** go in `./tmp` in the repo, never `/tmp`.
