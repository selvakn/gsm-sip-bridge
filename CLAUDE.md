<!-- SPECKIT START -->
For additional context about technologies to be used, project structure,
shell commands, and other important information, read the current plan at
`specs/015-volte-host-ims/plan.md`.
<!-- SPECKIT END -->

## Pre-commit Checklist

**MANDATORY — run before EVERY commit, no exceptions:**

```bash
cargo fmt --all          # fix formatting in place
make lint                # rustfmt check + clippy -D warnings + unsafe ratio
cargo test --workspace   # all tests must pass
```

Or equivalently: `make format && make lint && make test`

Do NOT commit if any of these fail. `make lint` failing has caused broken
commits in the past (e.g. rustfmt line-length violations in test files).
