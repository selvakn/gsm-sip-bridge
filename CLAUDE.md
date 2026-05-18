<!-- SPECKIT START -->
For additional context about technologies to be used, project structure,
shell commands, and other important information, read the current plan at
`specs/009-gsm-resiliency-cli/plan.md`.
<!-- SPECKIT END -->

## Pre-commit Checklist

Before every commit, run the following and fix any issues:

```bash
cargo fmt --check        # formatting
cargo clippy -p gsm-sip-bridge -p pjsua-safe -- -D warnings  # lint
cargo test --workspace   # tests
```

Or equivalently: `make format && make lint && make test`

Do NOT commit if any of these fail.
