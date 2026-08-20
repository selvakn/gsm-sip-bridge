//! specs/041-shutdown-resource-cleanup T020: pins the relationship between
//! `supervise::shutdown::STOP_ALLOWANCE` (the budget the teardown plan is
//! written against) and `stop_grace_period` in `docker/docker-compose.yml`
//! (what the container runtime actually enforces). The two must never drift
//! apart by hand — if `stop_grace_period` is ever lowered below the code's
//! own budget, a graceful stop can be force-killed mid-teardown, which is
//! exactly the "worse than today" case FR-019/research.md R8 exist to avoid.

use gsm_sip_bridge::supervise::shutdown::STOP_ALLOWANCE;
use std::path::Path;

fn compose_yaml() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docker/docker-compose.yml");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()))
}

/// Parses a bare `stop_grace_period: 60s` line's value into seconds. Minimal
/// on purpose — this test exists to catch numeric drift, not to be a YAML
/// parser; a `duration` fixture without a plain trailing `s` unit fails the
/// test loudly rather than silently mis-parsing.
fn parse_seconds(raw: &str) -> u64 {
    let raw = raw.trim();
    let digits: String = raw.chars().take_while(|c| c.is_ascii_digit()).collect();
    let unit = &raw[digits.len()..];
    let n: u64 = digits
        .parse()
        .unwrap_or_else(|_| panic!("could not parse a leading number out of {raw:?}"));
    match unit {
        "s" => n,
        "m" => n * 60,
        other => panic!("unrecognised stop_grace_period unit {other:?} in {raw:?}"),
    }
}

#[test]
fn compose_declares_a_stop_grace_period_at_least_the_teardown_budget() {
    let yaml = compose_yaml();
    let line = yaml
        .lines()
        .find(|l| l.trim_start().starts_with("stop_grace_period:"))
        .expect(
            "docker-compose.yml must declare stop_grace_period on the gsm-sip-bridge \
             service — without it Docker's 10s default applies, which the teardown \
             (waits, IKE terminate, XFRM flush, per-line device deletes) cannot fit \
             inside",
        );
    let value = line
        .split_once(':')
        .map(|(_, v)| v)
        .expect("malformed stop_grace_period line");
    let declared_secs = parse_seconds(value);

    assert!(
        declared_secs >= STOP_ALLOWANCE.as_secs(),
        "docker-compose.yml's stop_grace_period ({declared_secs}s) must be at least \
         supervise::shutdown::STOP_ALLOWANCE ({}s) — the container runtime must not be \
         able to force-kill a teardown that is still within its own budget",
        STOP_ALLOWANCE.as_secs()
    );
}
