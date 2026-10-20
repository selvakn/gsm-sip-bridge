# Contract: on-demand DPD in the strongSwan fork

Delivered as `docker/patches/0002-strongswan-vici-dpd.patch`. It is applied in the `strongswan-builder` stage of `docker/Dockerfile`, after checking out `022f3e515` and before `autoreconf`. It touches only:
- `src/libcharon/plugins/vici/vici_control.c`
- `src/swanctl/commands/dpd.c` (new)
- `src/swanctl/Makefile.am`

## vici command `dpd`

**Request:**

| Key | Type | Required | Meaning |
|---|---|---|---|
| `ike` | string | yes | Connection (IKE_SA) name, e.g. `ims0`. |

**Behaviour:**
- Check out the first IKE_SA with that name (`ike_sa_manager->checkout_by_name(…, FALSE)`).
- If none, reply `success = no`, `errmsg = "no IKE_SA named <ike>"`.
- If its state is not `ESTABLISHED`, check it back in and reply `success = no`, `errmsg = "IKE_SA <ike> is <state>"`.
- Otherwise queue `ike_dpd_create(TRUE)`, call `ike_sa->initiate(ike_sa, NULL, 0, NULL, NULL)`, and check it back in. If that returns `DESTROY_ME`, use `checkin_and_destroy` and reply `success = no`. Otherwise reply `success = yes`, `ike-id = <unique id>`.
- It never waits for the answer.
- The exchange is an ordinary IKEv2 DPD: an empty INFORMATIONAL request on charon's normal retransmit schedule (`retransmit_*` settings). charon handles an unanswered one exactly as it would its own DPD.
- Logged at `DBG1 CFG`: `vici dpd IKE_SA '<ike>'`. The exchange itself logs as usual: `generating INFORMATIONAL request <mid> [ ]`, then `parsed INFORMATIONAL response <mid> [ ]` if answered.

## swanctl command

```
swanctl --dpd --ike <name> [--raw|--pretty]     # short: -D -i <name>
```

- Exit 0 on `success = yes`.
- Exit non-zero, printing `dpd failed: <errmsg>`, otherwise.
- It uses the 26th and last `MAX_COMMANDS` slot. Short option `D` was unused.

## How the supervisor uses it

1. Take a log mark (`engine.log_mark`).
2. `swanctl --dpd --ike <conn>`, under the same bounded-timeout wrapper as the other swanctl calls.
3. A non-zero exit counts as "no evidence yet". The pending check stands, and the next tick decides from the log and counters alone.
4. Next tick: any `<conn|…> parsed …` line after the mark, a younger inbound ESP age, or a grown tunnel RX counter counts as evidence of life (research R4).

## Tests

- **Patch:** the image build compiles it. The quickstart's step 1 runs `swanctl --dpd --ike ims0` on the live rig and expects a `parsed INFORMATIONAL response` within 2s.
- **Supervisor side:** the trigger and the reading of the result are tested through `MockCommandRunner`, with real charon log text captured from the rig. The mock is justified at its definition: charon, a netns and root aren't available in `cargo test`.
