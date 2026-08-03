<!-- SPECKIT START -->
For additional context about technologies to be used, project structure,
shell commands, and other important information, read the current plan at
`specs/025-outbound-calling/plan.md`.
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
