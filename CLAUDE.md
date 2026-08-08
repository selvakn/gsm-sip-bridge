<!-- SPECKIT START -->
For additional context about technologies to be used, project structure,
shell commands, and other important information, read the current plan at
`specs/030-bad-port-isolation/plan.md`.
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
