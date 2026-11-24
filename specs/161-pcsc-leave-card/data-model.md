# Data Model: Stop the Bridge Resetting a Shared Card-Reader SIM

No persisted data. These are the in-process shapes that change.

## SharedCard (`modules::pcsc_card`, `pub(crate)`)

| Field | Type | Notes |
|---|---|---|
| card | `Option<pcsc::Card>` | `Some` from `connect` until `Drop` takes it |

- `connect(ctx: &Context, reader: &CStr) -> Result<SharedCard, pcsc::Error>`. Shared mode, any protocol, as today.
- `Deref` / `DerefMut` to `Card`.
- **Release (Drop)**: `card.take()` → `disconnect(Disposition::LeaveCard)`. On `Err((card, _))` → `std::mem::forget(card)`. Never reaches `Card::drop`, which resets.
- **Invariant**: no bridge code path drops a bare `pcsc::Card`.

## PcscTransport (`modules::pcsc_card`, `pub`)

| Field | Type | Notes |
|---|---|---|
| card | `SharedCard` | was `Card` |
| aid | `Vec<u8>` | **new**. USIM AID found by the identifying probe in `connect`, inside its transaction |

- `connect(target_imsi)`: the probe returns `(aid, imsi)`. Rejected candidates are dropped, which leaves their cards.
- `with_transaction(f)`: unchanged behaviour; the doc comment is corrected.
- `ApduTransport::authenticate_usim` (override): `with_transaction(|t| select_and_authenticate(t, &self.aid, rand, autn))`.

## ApduTransport (`modules::usim`, `pub trait`)

| Method | Kind | Default |
|---|---|---|
| `transmit_apdu(&mut self, &str) -> BridgeResult<String>` | required | — |
| `authenticate_usim(&mut self, &[u8;16], &[u8;16]) -> BridgeResult<AkaResult>` | **new, provided** | `authenticate(self, rand, autn)`, exactly today's call, against the already-selected ADF |

## select_and_authenticate (`modules::usim`, `pub(crate) fn`)

`(t: &mut dyn ApduTransport, aid: &[u8], rand, autn) -> BridgeResult<AkaResult>` = `select_usim(t, aid)?; authenticate(t, rand, autn)`.

- APDU order: SELECT MF, SELECT AID, AUTHENTICATE, then GET RESPONSE on `61xx`.
- A failed SELECT returns before AUTHENTICATE.

## OpenedCard (`ims`, `pub(crate)`)

The shape is unchanged. Its meaning changes:

- **Modem line**: the USIM ADF is selected at open, as today.
- **Reader line**: not selected at open. Each `run_aka` selects inside its own window.
