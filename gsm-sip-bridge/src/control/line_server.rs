//! The per-line listener a process hosting an idle line runs so the process
//! that owns the SIP side can command it to place an outbound call.
//!
//! Distinct from `control::server` (the CLI/agent-report socket): that
//! channel is one-directional (agent -> daemon) and too slow for call setup
//! (`agent_report_interval_seconds`, default 10s). This one is synchronous,
//! request/response, and exists only when `[outbound].enabled`.
//!
//! See `specs/025-outbound-calling/contracts/line-command.md` and
//! `research.md` R-003.
