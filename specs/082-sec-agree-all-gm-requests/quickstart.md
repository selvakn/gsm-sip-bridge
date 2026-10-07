# Quickstart: verifying security-agreement headers

## Automated

```bash
make format && make lint && make test
```

## Live (FR-009)

Use only synthetic numbers in anything committed; the test destination
is kept out of the repo.

1. **Vodafone (local rig)**: prime the rig, start the bridge with sec-agree on,
   place an outbound call, let it ring and answer, hang up from each side.
2. **Jio (Pi)**: arm64 build via the `arm-build-200` skill, deploy the next
   `jio-gmN` tag, repeat.
3. For each run capture the Gm traffic (decrypted, as the earlier IPsec work
   did; xfrm counters are not enough) and confirm BYE, PRACK, ACK, OPTIONS and
   SUBSCRIBE carry all three headers, and that no request drew a new 4xx.
