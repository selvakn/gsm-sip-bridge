//! Bridges strongSwan's `eap-sim-pcsc` plugin to the SIM inside the modem
//! (specs/012-strongswan-epdg, contracts/vpcd-bridge-protocol.md).
//!
//! `eap-sim-pcsc` speaks PC/SC; the SIM is only reachable via `AT+CSIM`.
//! This module implements the virtual-card side of vsmartcard's `vpcd`
//! wire protocol (a length-prefixed TCP framing carrying power/reset/ATR
//! control messages and raw command/response APDUs), forwarding APDUs to
//! the SIM via the existing `modules::usim`/`modules::at_commander`
//! machinery — which also absorbs the EC200U/SIM quirks documented in
//! `docker/patches/0001-ec200u-at-csim-fixes.patch` at this same boundary,
//! rather than patching strongSwan itself.
