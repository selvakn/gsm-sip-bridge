//! Moved to `alerts::discord` (specs/022-discord-critical-alerts), which
//! generalizes `DiscordClient` to serve every alert category, not just SMS.
//! Re-exported here so existing `sms::discord::DiscordClient` call sites and
//! imports are unaffected (`forward_sms`'s behavior is unchanged — FR-001).

pub use crate::alerts::discord::DiscordClient;
