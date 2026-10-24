//! The per-line view behind `/api/overview` (specs/122-management-webui,
//! data-model.md `LineView`): what the daemon knows about each line, folded
//! from the agent reports (`metrics::ingest`) and the circuit-switched card
//! pool.
//!
//! [`assemble`] and [`derive_health`] are pure, so the rules are unit-tested
//! without a daemon; [`collect`] is the thin async layer that gathers the
//! inputs.

use crate::control::protocol::{
    ActiveCallInfoWire, AgentKind, ControlCmd, ControlResp, SignalReadingWire, SlotInfo,
    UplinkState,
};
use crate::metrics::ingest::AgentSnapshot;
use crate::modules::signal::SignalReading;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;

/// How a line is doing, worst first: `stale` outranks everything because
/// nothing else about a line nobody has heard from can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Ok,
    Degraded,
    Down,
    Stale,
}

impl Health {
    pub fn as_str(self) -> &'static str {
        match self {
            Health::Ok => "ok",
            Health::Degraded => "degraded",
            Health::Down => "down",
            Health::Stale => "stale",
        }
    }
}

/// The facts [`derive_health`] looks at. `None` means "not reported", which
/// is never read as down.
#[derive(Debug, Clone, Default)]
pub struct LineFacts {
    pub stale: bool,
    pub registered: Option<bool>,
    pub tunnel_up: Option<bool>,
    pub child_sa_up: Option<bool>,
    pub pdn_up: Option<bool>,
    pub gm_connection_up: Option<bool>,
    pub uplink_held: bool,
    pub flapping: bool,
}

/// `stale`, then `down` (unregistered, tunnel, child SA or PDN), then
/// `degraded` (Gm down, uplink held, flapping), then `ok`. The reasons name
/// every failing component in the winning tier, so a card can say what is
/// wrong, not merely that something is.
pub fn derive_health(facts: &LineFacts) -> (Health, Vec<&'static str>) {
    if facts.stale {
        return (Health::Stale, vec!["stale"]);
    }
    let mut down = Vec::new();
    if facts.registered == Some(false) {
        down.push("unregistered");
    }
    if facts.tunnel_up == Some(false) {
        down.push("tunnel_down");
    }
    if facts.child_sa_up == Some(false) {
        down.push("child_sa_down");
    }
    if facts.pdn_up == Some(false) {
        down.push("pdn_down");
    }
    if !down.is_empty() {
        return (Health::Down, down);
    }
    let mut degraded = Vec::new();
    if facts.gm_connection_up == Some(false) {
        degraded.push("gm_down");
    }
    if facts.uplink_held {
        degraded.push("uplink_held");
    }
    if facts.flapping {
        degraded.push("flapping");
    }
    if !degraded.is_empty() {
        return (Health::Degraded, degraded);
    }
    (Health::Ok, Vec::new())
}

#[derive(Debug, Clone, Serialize)]
pub struct LineView {
    pub id: String,
    pub index: u32,
    pub transport: &'static str,
    /// Operator name, or `MCC-MNC` when the name is unknown. Not set for
    /// circuit-switched lines (see `network`).
    pub carrier: Option<String>,
    /// Circuit-switched lines only: the radio access the card is on.
    pub network: Option<String>,
    pub msisdn: Option<String>,
    pub registration_state: Option<String>,
    pub registered_at: Option<String>,
    pub expires_at: Option<String>,
    pub tunnel_up: Option<bool>,
    pub child_sa_up: Option<bool>,
    pub gm_connection_up: Option<bool>,
    pub uplink: Option<&'static str>,
    pub pdn_up: Option<bool>,
    pub active_call_count: u32,
    pub active_calls: Vec<ActiveCallInfoWire>,
    pub signal: Option<SignalReadingWire>,
    pub signal_note: Option<&'static str>,
    pub last_report_age_seconds: Option<u64>,
    pub stale: bool,
    pub health: &'static str,
    pub health_reasons: Vec<&'static str>,
}

/// Everything [`assemble`] needs.
pub struct Inputs<'a> {
    pub agents: Vec<AgentSnapshot>,
    /// `module_id → phone number` from config.
    pub phones: &'a HashMap<String, String>,
    pub cs_slots: Vec<SlotInfo>,
    /// Latest CS reading per module id.
    pub cs_signal: HashMap<String, SignalReading>,
    pub signal_poll_enabled: bool,
}

fn unix_to_rfc3339(secs: u64) -> Option<String> {
    DateTime::<Utc>::from_timestamp(i64::try_from(secs).ok()?, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// Operator names for the PLMNs this bridge is run against; anything else is
/// shown as the raw `MCC-MNC` rather than guessed at.
fn carrier_name(plmn: &str) -> String {
    let (mcc, mnc) = plmn.split_once('-').unwrap_or((plmn, ""));
    let name = match (mcc, mnc) {
        ("405", m) if m.starts_with('8') && m.len() == 3 => Some("Jio"),
        ("404", "10" | "45" | "49" | "40" | "70") => Some("Airtel"),
        ("404", "43" | "86" | "11" | "13") => Some("Vodafone Idea"),
        _ => None,
    };
    match name {
        Some(n) => format!("{n} ({plmn})"),
        None => plmn.to_string(),
    }
}

fn cs_health(lifecycle: &str) -> (Health, Vec<&'static str>) {
    match lifecycle {
        "Ready" => (Health::Ok, Vec::new()),
        "Initializing" => (Health::Degraded, vec!["initializing"]),
        "Recovering" => (Health::Down, vec!["recovering"]),
        _ => (Health::Down, vec!["given_up"]),
    }
}

/// Folds agent snapshots and CS slots into one view per line, CS first and
/// then IMS lines by id.
pub fn assemble(inputs: Inputs<'_>) -> Vec<LineView> {
    let mut lines: Vec<LineView> = Vec::new();

    let mut cs_slots = inputs.cs_slots;
    cs_slots.sort_by_key(|s| s.slot);
    for slot in cs_slots {
        let id = if slot.module_id.is_empty() {
            format!("slot-{}", slot.slot)
        } else {
            slot.module_id.clone()
        };
        let reading = inputs.cs_signal.get(&id);
        let (health, reasons) = cs_health(&slot.state);
        let msisdn = match slot.phone.trim() {
            "" | "Unknown" => inputs.phones.get(&id).cloned(),
            p => Some(p.to_string()),
        };
        lines.push(LineView {
            id: id.clone(),
            index: slot.slot,
            transport: "cs",
            carrier: None,
            network: (!slot.network.is_empty()).then(|| slot.network.clone()),
            msisdn,
            registration_state: Some(slot.state.to_lowercase()),
            registered_at: None,
            expires_at: None,
            tunnel_up: None,
            child_sa_up: None,
            gm_connection_up: None,
            uplink: None,
            pdn_up: None,
            active_call_count: u32::from(slot.has_active_call),
            // The pool knows a call is up, not who is on it.
            active_calls: Vec::new(),
            signal: reading.map(SignalReadingWire::from),
            signal_note: if reading.is_some() {
                None
            } else if inputs.signal_poll_enabled {
                Some("not_polled_yet")
            } else {
                Some("polling_disabled")
            },
            last_report_age_seconds: None,
            stale: false,
            health: health.as_str(),
            health_reasons: reasons,
        });
    }

    // Group agent reports by module id. A group is a line only when a carrier
    // side (`Ims`/`Volte`) reports it: the SIP-side agents also report
    // pseudo-modules such as the embedded registrar's `sip-server`.
    let mut groups: HashMap<String, Vec<&AgentSnapshot>> = HashMap::new();
    for snap in &inputs.agents {
        groups.entry(snap.module_id.clone()).or_default().push(snap);
    }
    let mut ids: Vec<&String> = groups
        .iter()
        .filter(|(_, g)| {
            g.iter()
                .any(|a| matches!(a.agent, AgentKind::Ims | AgentKind::Volte))
        })
        .map(|(id, _)| id)
        .collect();
    ids.sort();

    let first_ims_index = u32::try_from(lines.len()).unwrap_or(0);
    for (n, id) in ids.into_iter().enumerate() {
        let group = &groups[id];
        let carrier_side = group
            .iter()
            .find(|a| matches!(a.agent, AgentKind::Ims | AgentKind::Volte))
            .copied()
            .expect("filtered to groups with a carrier side");
        let supervisor = group.iter().find(|a| a.agent == AgentKind::Supervisor);
        let is_volte = carrier_side.agent == AgentKind::Volte;
        let stale = group.iter().any(|a| !a.up);

        let mut active_calls: Vec<ActiveCallInfoWire> = group
            .iter()
            .flat_map(|a| a.details.active_call_details.iter().flatten().cloned())
            .collect();
        active_calls.sort_by_key(|c| c.started_at_unix);
        let reported_count = group
            .iter()
            .filter_map(|a| a.active_calls)
            .max()
            .unwrap_or(0);
        let active_call_count =
            reported_count.max(u32::try_from(active_calls.len()).unwrap_or(u32::MAX));

        let uplink = supervisor.and_then(|s| s.uplink);
        let facts = LineFacts {
            stale,
            registered: carrier_side.registered,
            tunnel_up: if is_volte {
                None
            } else {
                carrier_side.tunnel_up
            },
            child_sa_up: supervisor.and_then(|s| s.child_sa_up),
            pdn_up: if is_volte {
                carrier_side.tunnel_up
            } else {
                None
            },
            gm_connection_up: carrier_side.gm_connection_up,
            uplink_held: matches!(uplink, Some(u) if u != UplinkState::Up),
            flapping: supervisor.is_some_and(|s| s.flapping),
        };
        let (health, reasons) = derive_health(&facts);

        let signal = carrier_side.details.signal.clone();
        let signal_note = if signal.is_some() {
            None
        } else if id.starts_with("pcsc") {
            Some("not_available_pcsc")
        } else if inputs.signal_poll_enabled {
            Some("not_polled_yet")
        } else {
            Some("polling_disabled")
        };

        lines.push(LineView {
            id: id.clone(),
            index: first_ims_index + u32::try_from(n).unwrap_or(0) + 1,
            transport: if is_volte { "volte" } else { "vowifi" },
            carrier: carrier_side.details.plmn.as_deref().map(carrier_name),
            network: None,
            msisdn: inputs.phones.get(id).cloned(),
            registration_state: carrier_side
                .details
                .registration_state
                .map(|s| s.to_string()),
            registered_at: carrier_side.details.registered_at.and_then(unix_to_rfc3339),
            expires_at: carrier_side
                .registration_expires_at
                .and_then(unix_to_rfc3339),
            tunnel_up: facts.tunnel_up,
            child_sa_up: facts.child_sa_up,
            gm_connection_up: facts.gm_connection_up,
            uplink: uplink.map(UplinkState::as_str),
            pdn_up: facts.pdn_up,
            active_call_count,
            active_calls,
            signal,
            signal_note,
            last_report_age_seconds: Some(carrier_side.age_seconds as u64),
            stale,
            health: health.as_str(),
            health_reasons: reasons,
        });
    }

    lines
}

/// `module_id → number` for every line config names a number for, including
/// PC/SC lines (whose ids are `pcsc0`, `pcsc1`, ... in config order).
pub fn phone_map(config: &crate::config::AppConfig) -> HashMap<String, String> {
    let mut map = crate::alerts::line_phone_map(config);
    for (i, over) in config
        .vowifi
        .line_overrides
        .iter()
        .filter(|o| o.pcsc_reader)
        .enumerate()
    {
        if let Some(msisdn) = over.msisdn.as_deref().filter(|m| !m.is_empty()) {
            map.insert(format!("pcsc{i}"), msisdn.to_string());
        }
    }
    map
}

/// Asks the card pool for its slots. `None` when the pool is not running or
/// does not answer within a second.
pub async fn list_cs_slots(cs: &crate::modules::ControlCmdSender) -> Option<Vec<SlotInfo>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    cs.send_timeout((ControlCmd::ListSlots, tx), Duration::from_secs(1))
        .await
        .ok()?;
    match tokio::time::timeout(Duration::from_secs(1), rx).await {
        Ok(Ok(ControlResp::OkSlots { slots })) => Some(slots),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::protocol::{
        ActiveCallInfoWire, AgentDetails, CallDirectionWire, RegistrationStateWire,
    };

    fn facts() -> LineFacts {
        LineFacts::default()
    }

    #[test]
    fn nothing_wrong_is_ok() {
        assert_eq!(derive_health(&facts()), (Health::Ok, vec![]));
    }

    #[test]
    fn unreported_facts_are_never_read_as_down() {
        // Every Option is None: an older agent that reports nothing.
        let (health, _) = derive_health(&LineFacts::default());
        assert_eq!(health, Health::Ok);
    }

    #[test]
    fn stale_outranks_everything() {
        let f = LineFacts {
            stale: true,
            registered: Some(false),
            tunnel_up: Some(false),
            ..facts()
        };
        assert_eq!(derive_health(&f), (Health::Stale, vec!["stale"]));
    }

    #[test]
    fn each_down_condition_names_itself() {
        for (f, reason) in [
            (
                LineFacts {
                    registered: Some(false),
                    ..facts()
                },
                "unregistered",
            ),
            (
                LineFacts {
                    tunnel_up: Some(false),
                    ..facts()
                },
                "tunnel_down",
            ),
            (
                LineFacts {
                    child_sa_up: Some(false),
                    ..facts()
                },
                "child_sa_down",
            ),
            (
                LineFacts {
                    pdn_up: Some(false),
                    ..facts()
                },
                "pdn_down",
            ),
        ] {
            assert_eq!(derive_health(&f), (Health::Down, vec![reason]));
        }
    }

    #[test]
    fn down_lists_every_failing_component() {
        let f = LineFacts {
            registered: Some(false),
            child_sa_up: Some(false),
            ..facts()
        };
        let (health, reasons) = derive_health(&f);
        assert_eq!(health, Health::Down);
        assert_eq!(reasons, vec!["unregistered", "child_sa_down"]);
    }

    #[test]
    fn down_outranks_degraded() {
        let f = LineFacts {
            child_sa_up: Some(false),
            gm_connection_up: Some(false),
            uplink_held: true,
            ..facts()
        };
        assert_eq!(derive_health(&f), (Health::Down, vec!["child_sa_down"]));
    }

    #[test]
    fn degraded_conditions() {
        let f = LineFacts {
            gm_connection_up: Some(false),
            uplink_held: true,
            flapping: true,
            ..facts()
        };
        assert_eq!(
            derive_health(&f),
            (Health::Degraded, vec!["gm_down", "uplink_held", "flapping"])
        );
    }

    fn snap(agent: AgentKind, module: &str) -> AgentSnapshot {
        AgentSnapshot {
            agent,
            module_id: module.to_string(),
            up: true,
            age_seconds: 3.0,
            registered: None,
            tunnel_up: None,
            gm_connection_up: None,
            child_sa_up: None,
            pbx_registered: None,
            active_calls: None,
            uplink: None,
            flapping: false,
            registration_expires_at: None,
            details: AgentDetails::default(),
        }
    }

    fn inputs<'a>(agents: Vec<AgentSnapshot>, phones: &'a HashMap<String, String>) -> Inputs<'a> {
        Inputs {
            agents,
            phones,
            cs_slots: vec![],
            cs_signal: HashMap::new(),
            signal_poll_enabled: true,
        }
    }

    #[test]
    fn agents_of_one_module_fold_into_one_line() {
        let phones = HashMap::from([("card-1".to_string(), "+919000000000".to_string())]);
        let mut ims = snap(AgentKind::Ims, "card-1");
        ims.registered = Some(true);
        ims.tunnel_up = Some(true);
        ims.gm_connection_up = Some(true);
        ims.registration_expires_at = Some(1_700_003_600);
        ims.details.registration_state = Some(RegistrationStateWire::Registered);
        ims.details.plmn = Some("405-857".into());
        let mut sup = snap(AgentKind::Supervisor, "card-1");
        sup.child_sa_up = Some(true);
        sup.uplink = Some(UplinkState::Up);
        let mut sip = snap(AgentKind::Sip, "card-1");
        sip.details.active_call_details = Some(vec![ActiveCallInfoWire {
            remote: "+919000000001".into(),
            direction: CallDirectionWire::Outbound,
            started_at_unix: 5,
            answered: true,
            loss_percent: None,
            jitter_ms: None,
            rtt_ms: None,
        }]);

        let lines = assemble(inputs(vec![ims, sup, sip], &phones));
        assert_eq!(lines.len(), 1);
        let l = &lines[0];
        assert_eq!(l.transport, "vowifi");
        assert_eq!(l.msisdn.as_deref(), Some("+919000000000"));
        assert_eq!(l.registration_state.as_deref(), Some("registered"));
        assert_eq!(l.child_sa_up, Some(true));
        assert_eq!(l.uplink, Some("up"));
        assert_eq!(l.pdn_up, None);
        assert_eq!(l.expires_at.as_deref(), Some("2023-11-14T23:13:20Z"));
        assert_eq!(l.active_calls.len(), 1);
        assert_eq!(l.active_call_count, 1);
        assert_eq!(l.health, "ok");
        assert!(l.carrier.as_deref().unwrap().contains("405-857"));
    }

    #[test]
    fn a_volte_line_reports_pdn_not_tunnel() {
        let phones = HashMap::new();
        let mut volte = snap(AgentKind::Volte, "volte");
        volte.registered = Some(true);
        volte.tunnel_up = Some(false);
        let lines = assemble(inputs(
            vec![volte, snap(AgentKind::VolteSip, "volte")],
            &phones,
        ));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].transport, "volte");
        assert_eq!(lines[0].tunnel_up, None);
        assert_eq!(lines[0].pdn_up, Some(false));
        assert_eq!(lines[0].health, "down");
        assert_eq!(lines[0].health_reasons, vec!["pdn_down"]);
    }

    #[test]
    fn sip_side_pseudo_modules_are_not_lines() {
        let phones = HashMap::new();
        let lines = assemble(inputs(vec![snap(AgentKind::Sip, "sip-server")], &phones));
        assert!(lines.is_empty());
    }

    #[test]
    fn a_silent_agent_makes_the_whole_line_stale() {
        let phones = HashMap::new();
        let ims = snap(AgentKind::Ims, "card-2");
        let mut sup = snap(AgentKind::Supervisor, "card-2");
        sup.up = false;
        let lines = assemble(inputs(vec![ims, sup], &phones));
        assert!(lines[0].stale);
        assert_eq!(lines[0].health, "stale");
    }

    #[test]
    fn signal_notes_distinguish_the_reasons_for_no_reading() {
        let phones = HashMap::new();
        let lines = assemble(inputs(
            vec![
                snap(AgentKind::Ims, "pcsc0"),
                snap(AgentKind::Ims, "card-3"),
            ],
            &phones,
        ));
        let by_id = |id: &str| lines.iter().find(|l| l.id == id).unwrap();
        assert!(by_id("pcsc0").signal.is_none());
        assert_eq!(by_id("pcsc0").signal_note, Some("not_available_pcsc"));
        assert_eq!(by_id("card-3").signal_note, Some("not_polled_yet"));
    }

    #[test]
    fn cs_slots_become_cs_lines_with_their_signal() {
        let phones = HashMap::new();
        let slot = SlotInfo {
            slot: 1,
            state: "Ready".into(),
            phone: "+919000000000".into(),
            network: "LTE".into(),
            module_id: "ec20-AAAAAA".into(),
            has_active_call: true,
        };
        let mut cs_signal = HashMap::new();
        cs_signal.insert(
            "ec20-AAAAAA".to_string(),
            SignalReading {
                at_unix: 9,
                rssi_dbm: Some(-71),
                ..Default::default()
            },
        );
        let lines = assemble(Inputs {
            agents: vec![],
            phones: &phones,
            cs_slots: vec![slot],
            cs_signal,
            signal_poll_enabled: true,
        });
        let l = &lines[0];
        assert_eq!(l.transport, "cs");
        assert_eq!(l.id, "ec20-AAAAAA");
        assert_eq!(l.index, 1);
        assert_eq!(l.signal.as_ref().unwrap().rssi_dbm, Some(-71));
        assert_eq!(l.active_call_count, 1);
        assert_eq!(l.health, "ok");
        assert_eq!(l.network.as_deref(), Some("LTE"));
    }

    #[test]
    fn cs_lifecycle_maps_to_health() {
        assert_eq!(cs_health("Ready").0, Health::Ok);
        assert_eq!(cs_health("Initializing").0, Health::Degraded);
        assert_eq!(cs_health("Recovering").0, Health::Down);
        assert_eq!(cs_health("GivenUp").0, Health::Down);
    }

    #[test]
    fn unknown_plmns_are_shown_raw() {
        assert_eq!(carrier_name("999-99"), "999-99");
        assert!(carrier_name("405-857").starts_with("Jio"));
    }
}
