# Quickstart: verifying `tel:` caller identity

## Automated

```bash
make format && make lint && make test
```

Every row of [contracts/identity-parsing.md](./contracts/identity-parsing.md)
is a unit test in `ims/identity.rs`, `ims/session.rs` or
`ims/agent/mod.rs`. Run them on their own with:

```bash
cargo test -p gsm-sip-bridge identity
cargo test -p gsm-sip-bridge extract_caller
cargo test -p gsm-sip-bridge header_uri
```

## Live

Use only synthetic numbers in anything committed. The real caller numbers
from these runs stay out of the repo.

1. **Vodafone (local rig, pjsip-linked build on the host)**: call the line
   from a phone.
   - The `inbound VoWiFi call` log shows the real `caller=`, not `unknown`.
   - On the PBX leg, `P-Asserted-Identity`, `X-GSM-Caller-ID` and
     `X-GSM-Caller-Name` are unchanged from a pre-change build. This is SC-005.
2. **Jio (Pi)**: do an arm64 build via the `arm-build-200` skill, deploy the
   next `jio-gmN` tag, and recreate the container (not restart). Then send an
   SMS to the line.
   - The log `sent the SMS delivery report` names the same `ipsmgw=` URI as
     before the change.
   - The SMS reaches the PBX/Discord.

   This is SC-006.
3. **T2**: not reachable from our rigs. After release, ask the reporter on
   #104 to confirm `caller=` shows the number. This is SC-001.
