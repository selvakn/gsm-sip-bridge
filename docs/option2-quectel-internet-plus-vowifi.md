# Option 2: Quectel as Internet Gateway + VoWiFi — SUPERSEDED

> **This design-only note has been superseded by
> [`ec20-internet-plus-vowifi.md`](ec20-internet-plus-vowifi.md).**
>
> That runbook is the maintained, validated version of "one EC20 card serving
> both internet and VoWiFi calls." It corrects this note's main inaccuracy —
> this draft brought internet up over the modem's **AT port**
> (`AT+CGDCONT`/`AT+CGACT`), which would contend with the bridge's `AT+CSIM`.
> The supported approach drives internet over **QMI** (`/dev/cdc-wdm0`) via a
> small opt-in sidecar container (specs/032-cellular-internet-sidecar), keeping
> the AT port free, and **gates** the bridge on a real internet-reachability
> probe.

Please read [`ec20-internet-plus-vowifi.md`](ec20-internet-plus-vowifi.md)
instead. The core insight this note got right — that VoWiFi's ePDG tunnel is
**completely decoupled** from the internet APN, so one card can do both — is
carried forward there.
