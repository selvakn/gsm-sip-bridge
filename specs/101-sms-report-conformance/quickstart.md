# Quickstart: verifying SMS delivery report conformance

## Local (no hardware)

```bash
make format && make lint && make test
```

New tests to see pass: the builder tests in `ims/sip_client.rs` (In-Reply-To present/absent; P-CSCF-then-Service-Route ordering; IPv6 bracketing; setting off) and the dispatch test that feeds a `500` to a sent report and expects one warning.

## Live: Jio (remote Pi) and Vodafone (local rig)

Follow the existing deployment notes for each rig (Pi: `jio-gmN` tag, native arm64 build). Then, for each:

1. Start the line and wait for registration.
2. Send an SMS to the line from a test handset.
3. In the log find `sent the SMS delivery report` and the answer to it: expect a `2xx`, no new warning, and the SMS forwarded as before. Repeat to 5 consecutive messages (SC-004).
4. Capture the outgoing report (tcpdump inside the privileged container, or the agent's message log) and confirm `In-Reply-To` and the P-CSCF `Route` against contracts/delivery-report-message.md.
5. Rollback check: set `sms_report_pcscf_route = false`, recreate the container (do not restart it), send one SMS, confirm the report has `In-Reply-To` but no P-CSCF `Route`.

If any carrier rejects, apply FR-010: only add the P-CSCF `Route` when the registration returned no `Service-Route`, and repeat.

## Reporter (T2 Russia, MEO)

Ask the #140 reporter to run the built image and post the log lines for the report and its answer. Not a merge gate (SC-006).
