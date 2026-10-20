# Quickstart: verifying Q.850 reject detection

1. `make format && make lint && make test` (unit tests cover parsing and
   the classification table without PJSIP).
2. Live: start the privileged test container (`test/`), run `siptest` as
   the PBX leg and answer the bridge's INVITE with:
   - `403` + `Reason: Q.850;cause=21`  -> carrier leg gets `486`
   - `503` + `Reason: Q.850;cause=21`  -> `486`
   - `503`, no Reason                  -> `480`
   - `403`, no Reason                  -> `480`; with `reject_statuses = [403]` -> `486`
3. Check the call record: `declined:pbx_rejected:403:q850=21`
   (`vowifi-status`).
4. Confirm no second INVITE arrives within 60 s.
