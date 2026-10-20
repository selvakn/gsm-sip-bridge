//! Concrete [`TunnelEngine`] implementations (specs/021-entrypoint-supervise-rust
//! Phase 3) — `StrongswanEngine` (1:1 port of `start_line_strongswan`'s charon/
//! swanctl choreography) and `SwuEngine` (1:1 port of `start_line_swu`'s dialer
//! process). Every log-parsing check is a pure function, ported from the
//! current script's `grep`/`sed` pattern with the same fixture the original
//! bash comment used, tested independently of any [`CommandRunner`].

use super::line_supervisor::{
    ChildSaLifetime, IkeActivity, IkeEvidence, IkeHeaderState, InboundEsp, SteadyStateHealth,
    TunnelEngine,
};
use super::runner::{ChildHandle, ChildSpec, CommandRunner};
use crate::config::TunnelAddressFamily;
use crate::control::protocol::IkeSaLossCause;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Keeps only the lines of the **shared** charon log that belong to
/// `conn_name`.
///
/// Every line's charon used to own its whole log file, so no filtering was
/// needed. Now one daemon serves all lines into one file, and the only thing
/// attributing an event to a line is the `<name|uniqueid>` prefix charon emits
/// because `strongswan.conf` sets `ike_name = yes`. That directive is
/// therefore load-bearing: drop it and every line would read every other
/// line's establishment and P-CSCF events as its own, so line 1 would report
/// itself up the moment line 0 connected.
fn lines_for_conn<'a>(
    charon_log: &'a str,
    conn_name: &str,
    // `DoubleEndedIterator`, not plain `Iterator`: `extract_latest_pcscf`
    // scans backwards for the most recent address.
) -> impl DoubleEndedIterator<Item = &'a str> + 'a {
    let needle = format!("<{conn_name}|");
    charon_log.lines().filter(move |l| l.contains(&needle))
}

/// 1:1 port of `grep -q "CHILD_SA.*established"`, scoped to one connection.
pub fn charon_log_shows_established(charon_log: &str, conn_name: &str) -> bool {
    lines_for_conn(charon_log, conn_name).any(|l| {
        l.contains("CHILD_SA")
            && l.find("CHILD_SA")
                .is_some_and(|pos| l[pos..].contains("established"))
    })
}

/// 1:1 port of the establish-time loop's P-CSCF extraction — same function
/// as `render`'s log-scraping sibling, `extract_latest_pcscf` (bash side,
/// pending its own port); duplicated here as a pure function rather than
/// imported, since this crate has no shared "bash-parity log parsers" module
/// yet — both read the identical `received P-CSCF server IP ...` marker.
pub fn extract_latest_pcscf(charon_log: &str, conn_name: &str) -> Option<String> {
    pick_stable_pcscf(&latest_pcscf_batch(charon_log, conn_name).0)
}

/// The P-CSCF of a negotiation that started **after** byte offset `mark` of the
/// charon log, or `None` while there is none yet (specs/087, research R4).
///
/// `mark` is the log's length taken before a recovery was issued, so only the
/// new negotiation can answer. Only a *closed* batch (ended by a `CHILD_SA ...
/// established` line) counts, so a half-logged negotiation is never accepted.
/// If the log is shorter than `mark` (truncated or rotated) the whole log is
/// read; a `mark` landing mid-line or off a char boundary is snapped forward
/// to the next line start.
pub fn extract_pcscf_after(charon_log: &str, conn_name: &str, mark: usize) -> Option<String> {
    closed_batch_after(charon_log, conn_name, mark).and_then(|b| pick_stable_pcscf(&b))
}

/// The whole P-CSCF pool the most recent negotiation advertised, deduplicated,
/// in the order charon logged it (specs/091 R1). Empty while there is none.
///
/// The single selection ([`extract_latest_pcscf`]) is still the only thing the
/// steady-state "P-CSCF changed" check compares; this is published beside it
/// so the agent can fail over to another member (FR-001, FR-004).
pub fn extract_latest_pcscf_pool(charon_log: &str, conn_name: &str) -> Vec<String> {
    dedup_pool(latest_pcscf_batch(charon_log, conn_name).0)
}

/// [`extract_latest_pcscf_pool`] for a negotiation logged after `mark`, with
/// the same rules as [`extract_pcscf_after`]: only a closed batch counts.
pub fn extract_pcscf_pool_after(
    charon_log: &str,
    conn_name: &str,
    mark: usize,
) -> Option<Vec<String>> {
    closed_batch_after(charon_log, conn_name, mark)
        .map(dedup_pool)
        .filter(|p| !p.is_empty())
}

fn dedup_pool(batch: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(batch.len());
    for a in batch {
        if !out.contains(&a) {
            out.push(a);
        }
    }
    out
}

/// The batch of a negotiation that started after byte offset `mark`, if it has
/// been closed by its establishment (see [`extract_pcscf_after`]).
fn closed_batch_after(charon_log: &str, conn_name: &str, mark: usize) -> Option<Vec<String>> {
    let slice = log_after(charon_log, mark);
    let (batch, closed) = latest_pcscf_batch(slice, conn_name);
    closed.then_some(batch)
}

fn is_valid_pcscf_addr(s: &str) -> bool {
    let is_v4 = s.split('.').count() == 4 && s.chars().all(|c| c.is_ascii_digit() || c == '.');
    let is_v6 = s.contains(':') && s.chars().all(|c| c.is_ascii_hexdigit() || c == ':');
    is_v4 || is_v6
}

/// The P-CSCF addresses advertised by the **most recent** negotiation on this
/// connection.
///
/// The charon log accumulates, and one negotiation emits several of these
/// lines in a row (Jio sends four: two IPv6, two IPv4) followed by the
/// `CHILD_SA ... established` line. So a batch is the run of addresses since
/// the last establishment: seeing an establishment closes the current batch,
/// and the next address starts a fresh one. Also reports whether the returned
/// batch has been closed by its establishment.
fn latest_pcscf_batch(charon_log: &str, conn_name: &str) -> (Vec<String>, bool) {
    const MARKER: &str = "received P-CSCF server IP ";
    let mut batch: Vec<String> = Vec::new();
    let mut closed = false;
    for line in lines_for_conn(charon_log, conn_name) {
        // `grep -oE 'received P-CSCF server IP .*'` matches the marker
        // ANYWHERE in the line, not just at its start — real charon output
        // prefixes every line with a facility tag (`[CFG] received P-CSCF
        // server IP ...`), which `strip_prefix` (requiring an exact line
        // start) never matches. Caught live: this bug made the establish
        // loop treat every successful connection as "stuck without P-CSCF"
        // forever, so it kept terminating and re-initiating a perfectly
        // good tunnel every ~30s instead of ever reporting success.
        if let Some(pos) = line.find(MARKER) {
            let addr = line[pos + MARKER.len()..].trim();
            if is_valid_pcscf_addr(addr) {
                if closed {
                    batch.clear();
                    closed = false;
                }
                batch.push(addr.to_string());
            }
        } else if line.contains("CHILD_SA")
            && line
                .find("CHILD_SA")
                .is_some_and(|pos| line[pos..].contains("established"))
        {
            closed = true;
        }
    }
    (batch, closed)
}

/// Choose one P-CSCF from a batch, as a **deterministic function of the set**
/// rather than of log order.
///
/// This used to be "whichever address charon logged last", which made the
/// steady-state check compare a value that moved on its own. The carrier
/// returns the same pool in varying order across rekeys, so a pure reorder
/// read as `P-CSCF changed` and refreshed the line — tearing down a perfectly
/// healthy registration. Observed on Jio 2026-08-14 with the trailing address
/// rotating between `56.2.134.134`, `56.2.134.146`, `10.56.156.21` and
/// `10.56.156.24` for one unchanging pool.
///
/// IPv4 is preferred because that is the family the Gm path uses in practice;
/// within a family the minimum is taken purely because it is stable. A
/// genuinely different pool still changes the answer, which is the only time
/// a refresh is warranted.
fn pick_stable_pcscf(batch: &[String]) -> Option<String> {
    let v4 = batch.iter().filter(|a| !a.contains(':')).min();
    v4.or_else(|| batch.iter().min()).cloned()
}

/// How long, after a recovery that restarts the agent, to wait for the new
/// negotiation's P-CSCF before restarting the agent with the old one (R4).
/// Measured live: a rebuild logs its P-CSCF batch within a few seconds.
pub const POST_RECOVERY_PCSCF_WAIT: Duration = Duration::from_secs(20);
/// Poll interval for that wait.
pub const POST_RECOVERY_PCSCF_POLL: Duration = Duration::from_secs(1);

/// Outer bound on a steady-state IKE_SA teardown. #107's teardown waited 52s on
/// an exchange the ePDG would never answer; `shutdown.rs`'s
/// `TERMINATE_TIMEOUT_SECS` is the precedent for bounding it.
const STEADY_TERMINATE_TIMEOUT_SECS: u32 = 10;

/// How long a graceful `swanctl --terminate` waits for the ePDG's DELETE
/// response (swanctl's own `--timeout`), and the outer `timeout` around it.
const GRACEFUL_TERMINATE_WAIT_SECS: u32 = 5;
const GRACEFUL_TERMINATE_TIMEOUT_SECS: u32 = 8;

/// Outer bound on the steady-state `swanctl --list-sas` query.
const STEADY_LIST_SAS_TIMEOUT_SECS: u32 = 10;

/// CHILD_SA states that still carry (or are about to carry) traffic. A rekey
/// briefly shows the old SA as `REKEYED` beside the new `INSTALLED` one, and
/// `REKEYING`/`UPDATING` are the same SA mid-exchange, so none of them may read
/// as "lost" or a healthy rekey would trigger recovery.
const USABLE_CHILD_SA_STATES: &[&str] = &["INSTALLED", "REKEYING", "REKEYED", "UPDATING"];

/// Whether `swanctl --list-sas` shows a usable CHILD_SA for `conn_name`.
///
/// This used to be a 1:1 port of `grep -q '^ims:'`, which matched the IKE_SA
/// *header* line (`ims0: #1, ESTABLISHED, ...`). A CHILD_SA lost under a live
/// IKE_SA therefore went undetected (#107). A CHILD_SA line is the two-space
/// indented `  ims0: #N, reqid R, STATE, ...` line, so that is what is matched,
/// scoped to one connection so another line's healthy SA never masks this
/// one's missing one.
pub fn list_sas_has_child_sa(sas_output: &str, conn_name: &str) -> bool {
    sas_output
        .lines()
        .filter_map(|l| child_sa_header(l, conn_name))
        .any(|(_, state)| USABLE_CHILD_SA_STATES.contains(&state))
}

/// `(unique_id, state)` from a CHILD_SA header line of `conn_name`
/// (`  ims0: #12, reqid 1, INSTALLED, ...`), or `None` for any other line.
///
/// Shared by [`list_sas_has_child_sa`] and [`newest_child_sa_lifetime`] so
/// "is there a usable CHILD_SA" and "which one is it" can never disagree about
/// what a CHILD_SA line looks like.
fn child_sa_header<'a>(line: &'a str, conn_name: &str) -> Option<(u32, &'a str)> {
    let rest = line.strip_prefix("  ")?;
    if rest.starts_with(' ') {
        return None;
    }
    let rest = rest.strip_prefix(conn_name)?.strip_prefix(": #")?;
    let mut fields = rest.split(", ");
    let id = fields.next()?.parse().ok()?;
    let state = fields.nth(1)?;
    Some((id, state))
}

/// The newest usable CHILD_SA of `conn_name` and its time to hard expiry,
/// from `swanctl --list-sas` (specs/093 research R1).
///
/// "Newest" is the highest `#N`, so during a natural rekey's overlap (old
/// `REKEYED` beside new `INSTALLED`) the replacement, with its full lifetime,
/// is what gets judged. The lifetime is the `expires in Ns` field on the
/// details line right under the header, as the pinned 5.9.3 prints it:
///
/// ```text
///   ims0: #12, reqid 1, INSTALLED, TUNNEL-in-UDP, ESP:AES_CBC-128/HMAC_SHA1_96
///     installed 1109s ago, rekeying in 2167s, expires in 2852s
/// ```
///
/// `expires_in` is `None` when that field is missing; the CHILD_SA is still
/// reported, so the caller can tell "no lifetime" from "no CHILD_SA".
pub fn newest_child_sa_lifetime(sas_output: &str, conn_name: &str) -> Option<ChildSaLifetime> {
    let mut newest: Option<ChildSaLifetime> = None;
    let mut lines = sas_output.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((unique_id, state)) = child_sa_header(line, conn_name) else {
            continue;
        };
        if !USABLE_CHILD_SA_STATES.contains(&state) {
            continue;
        }
        let expires_in = lines
            .peek()
            .filter(|d| d.starts_with("    ") && !d.starts_with("     "))
            .and_then(|d| {
                d.split(", ")
                    .find_map(|f| f.trim().strip_prefix("expires in "))
            })
            .and_then(|v| v.strip_suffix('s'))
            .and_then(|v| v.parse().ok());
        if newest.is_none_or(|n| unique_id > n.unique_id) {
            newest = Some(ChildSaLifetime {
                unique_id,
                expires_in,
            });
        }
    }
    newest
}

/// Inbound ESP across every CHILD_SA of `conn_name` that `swanctl --list-sas`
/// lists, whatever its state, so a rekey's old and new SA are both looked at
/// (specs/098 R4). Each prints an inbound line under its header:
///
/// ```text
///   ims0: #12, reqid 1, INSTALLED, TUNNEL-in-UDP, ESP:AES_CBC-128/HMAC_SHA1_96
///     installed 1109s ago, rekeying in 2167s, expires in 2852s
///     in  cea6576a (-|0x00000017),  21600 bytes,   242 packets,     4s ago
/// ```
///
/// The trailing `Ns ago` is the kernel's last use of that inbound SA, and is
/// absent while it has received nothing. `None` when no CHILD_SA of the
/// connection is listed.
pub fn inbound_esp(sas_output: &str, conn_name: &str) -> Option<InboundEsp> {
    let mut seen = false;
    let mut in_conn = false;
    let mut youngest: Option<u64> = None;
    let mut packets = 0u64;
    for line in sas_output.lines() {
        if child_sa_header(line, conn_name).is_some() {
            seen = true;
            in_conn = true;
            continue;
        }
        // Any other line at the IKE_SA or CHILD_SA header's depth ends the
        // CHILD_SA whose details were being read.
        if !line.starts_with("    ") {
            in_conn = false;
            continue;
        }
        let Some(rest) = line.strip_prefix("    in  ").filter(|_| in_conn) else {
            continue;
        };
        let fields: Vec<&str> = rest.split(", ").map(str::trim).collect();
        packets += fields
            .iter()
            .find_map(|f| f.strip_suffix(" packets"))
            .and_then(|n| n.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let age = fields
            .iter()
            .find_map(|f| f.strip_suffix("s ago"))
            .and_then(|n| n.trim().parse::<u64>().ok());
        if let Some(age) = age {
            youngest = Some(youngest.map_or(age, |y| y.min(age)));
        }
    }
    seen.then(|| InboundEsp {
        last_in_age: youngest.map(Duration::from_secs),
        packets,
        tun_rx_packets: None,
    })
}

/// The received-packet counter from `ip -s link show <iface>`:
///
/// ```text
/// 344: tun23-0@NONE: <NOARP,UP,LOWER_UP> mtu 1500 qdisc noqueue state UNKNOWN …
///     link/none  link-netnsid 0
///     RX:  bytes packets errors dropped  missed   mcast
///          63043     379      3       3       0       0
/// ```
///
/// The column is found by its header, which iproute2 has renamed and
/// re-spaced across versions. `None` when there is no RX block.
pub fn link_rx_packets(ip_s_link_output: &str) -> Option<u64> {
    let mut lines = ip_s_link_output.lines();
    while let Some(line) = lines.next() {
        if let Some(header) = line.trim_start().strip_prefix("RX:") {
            let column = header.split_whitespace().position(|h| h == "packets")?;
            return lines.next()?.split_whitespace().nth(column)?.parse().ok();
        }
    }
    None
}

/// Whether `swanctl --list-sas --ike <conn>` lists an IKE_SA for `conn_name`
/// in **any** state, `CONNECTING` included.
///
/// The IKE_SA is the column-0 `ims0: #1, ESTABLISHED, ...` header; the CHILD_SA
/// beneath it is indented. vici enumerates half-open IKE_SAs too, so an
/// initiate still in flight from the previous tick reads as present (and so
/// takes the two-strike CHILD_SA path) rather than as lost. No header at all
/// means the carrier deleted it or charon gave up (#106; research R1).
pub fn list_sas_has_ike_sa(sas_output: &str, conn_name: &str) -> bool {
    let prefix = format!("{conn_name}: #");
    sas_output.lines().any(|l| l.starts_with(&prefix))
}

/// The `#N` and state of `conn_name`'s IKE_SA header in `swanctl --list-sas`
/// output (specs/100): `ims0: #2, CONNECTING, IKEv2, …`.
///
/// Matched on `"<conn>: #"` at column 0, so `ims10` is not `ims1`, and the
/// indented CHILD_SA lines (which print `  ims0: #4, reqid …`) are not
/// headers. The first matching header wins.
pub fn ike_sa_header(sas_output: &str, conn_name: &str) -> Option<(u32, IkeHeaderState)> {
    let prefix = format!("{conn_name}: #");
    let rest = sas_output
        .lines()
        .find_map(|l| l.strip_prefix(prefix.as_str()))?;
    let mut fields = rest.split(',').map(str::trim);
    let id = fields.next()?.parse().ok()?;
    let state = match fields.next()? {
        "ESTABLISHED" => IkeHeaderState::Established,
        "CONNECTING" => IkeHeaderState::Connecting,
        other => IkeHeaderState::Other(other.to_string()),
    };
    Some((id, state))
}

/// The text charon logged after `<conn|N>` on one of its lines, without the
/// timestamp / thread / facility prefix.
fn ike_line_body<'a>(line: &'a str, conn_name: &str) -> Option<&'a str> {
    let needle = format!("<{conn_name}|");
    let at = line.find(&needle)?;
    let rest = &line[at + needle.len()..];
    Some(&rest[rest.find("> ")? + 2..])
}

/// Is this log body the connection's IKE traffic (specs/100 R4/R6): a message
/// generated, a message parsed, or a retransmit of an outstanding request.
fn is_ike_traffic(body: &str) -> bool {
    body.starts_with("generating ")
        || body.starts_with("parsed ")
        || (body.starts_with("retransmit ") && body.contains(" of request"))
}

/// A fingerprint of the IKE traffic charon has logged for `conn_name`
/// (specs/100 R6): how many generated, parsed or retransmitted lines, and the
/// last of them. `None` when there are none.
///
/// A fingerprint rather than a time because charon's log is stamped in the
/// container's local time (the Jio Pi's is IST), so a time parsed from it
/// would be wrong by the zone's offset. Two fingerprints that differ say that
/// charon was busy between two looks, which is all the half-open check needs.
pub fn ike_activity(charon_log: &str, conn_name: &str) -> Option<IkeActivity> {
    let mut lines = 0usize;
    let mut last: Option<&str> = None;
    for line in lines_for_conn(charon_log, conn_name) {
        if ike_line_body(line, conn_name).is_some_and(is_ike_traffic) {
            lines += 1;
            last = Some(line);
        }
    }
    last.map(|last| IkeActivity {
        lines,
        last: last.to_string(),
    })
}

/// What `log` (the charon log **after** a mark) shows of the ePDG for
/// `conn_name` (specs/100 R4): whether it said anything, and how many
/// retransmits of an outstanding request have gone unanswered since it last
/// did. A parsed message clears the count: it is the peer answering, or
/// speaking first with a DPD of its own.
pub fn ike_evidence(log: &str, conn_name: &str) -> IkeEvidence {
    let mut evidence = IkeEvidence::default();
    for body in lines_for_conn(log, conn_name).filter_map(|l| ike_line_body(l, conn_name)) {
        if body.starts_with("parsed ")
            && (body.contains(" request ") || body.contains(" response "))
        {
            evidence.parsed_from_peer = true;
            evidence.unanswered_retransmits = 0;
        } else if body.starts_with("retransmit ") && body.contains(" of request") {
            evidence.unanswered_retransmits += 1;
        }
    }
    evidence
}

/// Whether `ip route get <addr>` finds a usable route (specs/100 R3): exit 0
/// **and** a `src` address. A blackhole, unreachable or prohibit route and a
/// missing default route all fail with a non-zero exit; a route without a
/// source address could not carry the packet either.
pub fn route_to(runner: &dyn CommandRunner, addr: &str) -> bool {
    runner
        .run(&["ip", "route", "get", addr])
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .any(|t| t == "src")
        })
}

/// The part of `charon_log` after byte offset `mark`, snapped forward to a
/// line start (see [`extract_pcscf_after`] for the rules).
fn log_after(charon_log: &str, mark: usize) -> &str {
    if mark > charon_log.len() {
        return charon_log;
    }
    let mut at = mark;
    while !charon_log.is_char_boundary(at) {
        at += 1;
    }
    if at > 0 && charon_log.as_bytes()[at - 1] != b'\n' {
        match charon_log[at..].find('\n') {
            Some(nl) => at += nl + 1,
            None => at = charon_log.len(),
        }
    }
    &charon_log[at..]
}

/// Best-effort reason this connection's IKE_SA was lost, from the charon log.
///
/// Only lines after the most recent `IKE_SA <conn>[N] established` count, so a
/// previous session's DELETE is never blamed. Both markers are DBG1 in
/// strongSwan 5.9, hence present at our `filelog default = 1` (research R2).
/// A loss we caused ourselves logs neither, and reads `Unknown`.
pub fn ike_sa_loss_cause(charon_log: &str, conn_name: &str) -> IkeSaLossCause {
    let established = format!("IKE_SA {conn_name}[");
    let mut cause = IkeSaLossCause::Unknown;
    for line in lines_for_conn(charon_log, conn_name) {
        if line
            .find(&established)
            .is_some_and(|pos| line[pos..].contains("] established"))
        {
            cause = IkeSaLossCause::Unknown;
        } else if line.contains("received DELETE for IKE_SA") {
            cause = IkeSaLossCause::CarrierDelete;
        } else if line.contains("giving up after") {
            cause = IkeSaLossCause::DeadPeer;
        }
    }
    cause
}

/// The current IKE_SA session's tag (`"ims0[3]"`) if, within that session,
/// the ePDG answered a CREATE_CHILD_SA (a CHILD_SA rekey) with
/// `N(INVAL_SEL)`, i.e. `INVALID_SELECTORS` (specs/092 FR-009a, #107).
///
/// Scoped like [`ike_sa_loss_cause`]: the most recent `IKE_SA <conn>[N]
/// established` starts a session and clears any earlier rejection, so a
/// rebuilt session is judged only on its own rekeys and returns its own tag.
/// The match is on the parsed-message summary charon logs at level 1
/// (`parsed CREATE_CHILD_SA response 12 [ N(INVAL_SEL) ]`), which is what
/// #107's capture from the pinned fork shows.
pub fn child_rekey_invalid_selectors_session(charon_log: &str, conn_name: &str) -> Option<String> {
    let established = format!("IKE_SA {conn_name}[");
    let mut session: Option<String> = None;
    let mut rejected = false;
    for line in lines_for_conn(charon_log, conn_name) {
        if let Some(pos) = line.find(&established) {
            let rest = &line[pos + established.len()..];
            if let Some(end) = rest.find("] established") {
                session = Some(format!("{conn_name}[{}]", &rest[..end]));
                rejected = false;
                continue;
            }
        }
        if line.contains("CREATE_CHILD_SA response") && line.contains("N(INVAL_SEL)") {
            rejected = true;
        }
    }
    session.filter(|_| rejected)
}

/// The notify the ePDG most recently refused a CREATE_CHILD_SA with in the
/// current IKE_SA session (`"INVAL_SEL"`, `"TS_UNACCEPT"`, ...), or `None` if
/// the latest one succeeded or none was refused (specs/093 research R4).
///
/// Only ever used to explain a stuck-rekey rebuild in its log line, never to
/// trigger one: the trigger is the CHILD_SA's own lifetime, which works the
/// same whatever (if anything) the carrier answered. Scoped per session like
/// [`child_rekey_invalid_selectors_session`]. A refusal is a response whose
/// payloads are all notifies (`[ N(INVAL_SEL) ]`); a success carries an `SA`
/// (`[ SA No TSi TSr ]`) and clears it.
pub fn child_sa_rejection_notify(charon_log: &str, conn_name: &str) -> Option<String> {
    let established = format!("IKE_SA {conn_name}[");
    let mut notify: Option<String> = None;
    for line in lines_for_conn(charon_log, conn_name) {
        if line
            .find(&established)
            .is_some_and(|pos| line[pos..].contains("] established"))
        {
            notify = None;
            continue;
        }
        let Some(pos) = line.find("CREATE_CHILD_SA response") else {
            continue;
        };
        let rest = &line[pos..];
        let Some(payloads) = rest.find("[ ").and_then(|start| {
            rest[start + 2..]
                .find(" ]")
                .map(|end| &rest[start + 2..start + 2 + end])
        }) else {
            continue;
        };
        let Some(first) = payloads.split_whitespace().next() else {
            continue;
        };
        notify = if payloads.split_whitespace().all(|t| t.starts_with("N(")) {
            first
                .strip_prefix("N(")
                .and_then(|t| t.strip_suffix(')'))
                .map(str::to_string)
        } else {
            None
        };
    }
    notify
}

/// The single charon daemon shared by every strongswan-engine line.
///
/// One charon *per line* was the original design, and it was wrong for a
/// reason that only appears once a second line exists: charon's socket-default
/// plugin sets `SO_REUSEADDR` but never `SO_REUSEPORT`, so N charon processes
/// in one network namespace all wildcard-bind `0.0.0.0:500`/`0.0.0.0:4500` and
/// exactly **one** of them is delivered every inbound IKE packet. The losers
/// retransmit into the void and give up, which the carrier surfaces to callers
/// as the line being "switched off".
///
/// Observed live 2026-07-29: on one boot line 0 established and line 1 timed
/// out; on the very next restart of the same image the two swapped. That
/// coin-flip is what identified it as a local port collision rather than
/// anything carrier-side.
///
/// Sharing the daemon costs nothing in isolation. A CHILD_SA is bound to its
/// line by the `if_id` in its own connection block and by the pre-created
/// `tunN` XFRM interface sitting in that line's netns; the kernel keys its
/// SA/policy lookup on `if_id` regardless of which daemon installed the state.
/// This is exactly the arrangement strongSwan expects of a gateway terminating
/// many independent tunnels.
///
/// What sharing *does* cost is recovery blast radius, so the daemon is only
/// ever restarted when it is genuinely dead ([`SharedCharon::restart_if_dead`]).
/// Every other fault — a stale SA, a vanished interface, a missing CHILD_SA —
/// is recovered per connection through `--terminate --ike <conn>` /
/// `--initiate --child <conn>`, leaving other lines' tunnels untouched.
pub struct SharedCharon {
    pub strongswan_conf: String,
    pub swanctl_top_conf: String,
    pub charon_log: PathBuf,
    /// `Mutex`, not the `RefCell` the per-line engines use: this is reached
    /// concurrently from every line's supervisor thread, and holding the lock
    /// across the check-and-spawn is what makes "restart it only if nobody
    /// else already has" atomic.
    handle: Mutex<Option<Arc<ChildHandle>>>,
    /// Bumped every time the log is truncated for a fresh charon, so a
    /// [`LogMark`] taken before that can tell its byte offset no longer refers
    /// to the same file (a length comparison alone cannot: the new log may
    /// have grown past the old mark by the next read).
    log_generation: AtomicU64,
}

/// A position in the shared charon log: its truncation generation and length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogMark {
    generation: u64,
    len: usize,
}

impl SharedCharon {
    pub fn new(strongswan_conf: String, swanctl_top_conf: String, charon_log: PathBuf) -> Self {
        Self {
            strongswan_conf,
            swanctl_top_conf,
            charon_log,
            handle: Mutex::new(None),
            log_generation: AtomicU64::new(0),
        }
    }

    fn env_prefix(&self) -> String {
        format!("STRONGSWAN_CONF={}", self.strongswan_conf)
    }

    /// Spawns the daemon unless it is already running. Idempotent and safe to
    /// call concurrently from every line's startup thread — lines start on
    /// their own threads, and whichever arrives first wins.
    ///
    /// Returns the handle **only to the caller that actually spawned it**, so
    /// the daemon gets registered into `StartedState` exactly once instead of
    /// once per line.
    pub fn ensure_started(&self, runner: &dyn CommandRunner) -> Option<Arc<ChildHandle>> {
        let mut guard = self.handle.lock().unwrap();
        if guard.as_ref().is_some_and(|h| runner.is_alive(h)) {
            return None;
        }
        self.spawn_locked(runner, &mut guard)
    }

    /// Restarts the daemon **only if it is actually dead**, reporting whether
    /// it did.
    ///
    /// Both halves of that condition matter. A line whose vici calls fail
    /// while the daemon is alive must not take every other line's tunnel down
    /// with it; and a second line noticing the same death a moment later must
    /// not kill the replacement the first line just spawned.
    pub fn restart_if_dead(&self, runner: &dyn CommandRunner) -> bool {
        let mut guard = self.handle.lock().unwrap();
        if guard.as_ref().is_some_and(|h| runner.is_alive(h)) {
            return false;
        }
        self.spawn_locked(runner, &mut guard).is_some()
    }

    fn spawn_locked(
        &self,
        runner: &dyn CommandRunner,
        guard: &mut Option<Arc<ChildHandle>>,
    ) -> Option<Arc<ChildHandle>> {
        if let Some(old) = guard.take() {
            // Signalling alone was never enough (Greptile P1 on the per-line
            // version this replaces): RealCommandRunner's tracked-children
            // table only drops an entry once something reaps it, so a bare
            // signal leaked an entry on every restart — and let a
            // slow-to-terminate charon keep contending for the vici socket
            // and pidfile with the replacement about to be spawned.
            runner.reap(&old);
        }
        let _ = runner.write_file(&self.charon_log, "");
        self.log_generation.fetch_add(1, Ordering::SeqCst);
        let _ = runner.run(&["rm", "-f", "/var/run/charon.pid"]);

        let env = self.env_prefix();
        // `env` must be argv[0], not the `STRONGSWAN_CONF=...` string itself:
        // RealCommandRunner passes argv[0] straight to `Command::new`, so
        // without it the spawn tries to exec the environment assignment as a
        // program, fails with ENOENT, and leaves no daemon running while the
        // rest of the sequence proceeds as though one were.
        let spawned = runner
            .spawn(
                ChildSpec::new(["env", env.as_str(), "/usr/libexec/ipsec/charon"])
                    .capture_stdout_to(self.charon_log.clone()),
            )
            .ok()
            .map(Arc::new);
        if spawned.is_some() {
            // Let the vici socket come up before any swanctl call. A
            // `--load-all` issued too early fails silently (its result is
            // deliberately ignored), and the `--initiate` that follows then
            // fails with "CHILD_SA config not found" — leaving the line
            // re-initiating against an empty config forever, since
            // steady-state's ChildSaMissing branch only re-initiates and
            // never reloads.
            runner.sleep(Duration::from_secs(2));
        }
        guard.clone_from(&spawned);
        spawned
    }

    pub fn is_alive(&self, runner: &dyn CommandRunner) -> bool {
        self.handle
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|h| runner.is_alive(h))
    }

    /// The daemon's current handle, so a caller that has just driven a
    /// recovery can make sure the (possibly replaced) process is registered
    /// for shutdown.
    pub fn current_handle(&self) -> Option<Arc<ChildHandle>> {
        self.handle.lock().unwrap().clone()
    }

    /// `swanctl --load-all` over the shared `conf.d`, loading the union of
    /// every line's connection file.
    ///
    /// It must stay a whole-directory load: `--load-all` *unloads* any
    /// connection missing from what it just read, so loading one line's file
    /// alone would evict the other lines' connections. Loading the union is
    /// idempotent and order-independent, which is what makes it safe for
    /// concurrently-starting lines to each call it.
    pub fn load_all(&self, runner: &dyn CommandRunner) {
        let env = self.env_prefix();
        let _ = runner.run(&[
            "env",
            env.as_str(),
            "swanctl",
            "--load-all",
            "--file",
            &self.swanctl_top_conf,
        ]);
    }
}

/// One line's strongSwan engine state — this line's own identifiers, plus a
/// handle on the daemon every line shares. Anything not per line (the daemon,
/// its log, its vici socket, its config paths) lives in [`SharedCharon`].
pub struct StrongswanEngine {
    pub idx: u32,
    /// This line's swanctl connection *and* child name (`ims0`, `ims1`, ...).
    /// Unique per line so that `--initiate`/`--terminate` address exactly one
    /// line, and so the shared charon log's `<name|N>` prefixes can be
    /// attributed back to one line.
    pub conn_name: String,
    pub netns: String,
    pub tun_iface: String,
    /// This line's XFRM `if_id`, needed to recreate the interface after
    /// `SteadyStateHealth::TunVanished` (see `recreate_interface`).
    pub if_id: String,
    pub shared: Arc<SharedCharon>,
    /// Bounds the establish-time loop, unlike a real persistent line, which
    /// retries indefinitely (`None`, the value every persistent-line call
    /// site passes). specs/080-volte-pcscf-auto-prime's transient priming
    /// capture is the one caller that sets this: a one-shot attempt inside
    /// an operator-watched startup sequence needs an actual deadline rather
    /// than retrying forever.
    pub max_establish_attempts: Option<u32>,
    /// This line's effective `tunnel_address_family` (specs/092). Filters the
    /// P-CSCFs this engine selects and publishes (FR-013), and gates the
    /// `INVALID_SELECTORS` rekey hint to `Dual` lines (FR-009a).
    pub address_family: TunnelAddressFamily,
    /// What the last [`TunnelEngine::steady_state_health`] read of inbound
    /// ESP, for [`TunnelEngine::inbound_esp`] (specs/098). Cleared whenever a
    /// health check could not read the SAs, so it is never older than the
    /// listing it came from.
    pub last_inbound: std::cell::Cell<Option<InboundEsp>>,
    /// The tunnel interface's received-packet counter as the last
    /// [`TunnelEngine::steady_state_health`] read it, on every tick that
    /// found the interface, for [`TunnelEngine::tun_rx_packets`] (specs/098).
    pub last_tun_rx: std::cell::Cell<Option<u64>>,
    /// The ePDG address this line's tunnel runs to, for the underlay guard's
    /// route check (specs/100); `None` skips the check.
    pub epdg_addr: Option<String>,
    /// The IKE_SA header the last [`TunnelEngine::steady_state_health`] read,
    /// for [`TunnelEngine::ike_header`] (specs/100). Cleared whenever a health
    /// check could not read the SAs.
    pub last_ike_header: RefCell<Option<(u32, IkeHeaderState)>>,
}

impl StrongswanEngine {
    fn env_prefix(&self) -> String {
        self.shared.env_prefix()
    }

    /// `swanctl --list-sas` scoped to this line's connection; `None` if the
    /// call failed or timed out.
    ///
    /// `--ike <conn>` keeps the shared daemon from reporting every line's SAs,
    /// where another line's healthy CHILD_SA would mask this line's missing
    /// one. Bounded: a vici call that never returns would otherwise freeze this
    /// line's supervision loop while the reporter thread keeps heartbeating the
    /// last (healthy) state. A timeout reads as a broken vici.
    fn list_sas(&self, runner: &dyn CommandRunner) -> Option<String> {
        let env = self.env_prefix();
        let secs = STEADY_LIST_SAS_TIMEOUT_SECS.to_string();
        match runner.run(&[
            "timeout",
            &secs,
            "env",
            &env,
            "swanctl",
            "--list-sas",
            "--ike",
            &self.conn_name,
        ]) {
            Ok(out) if out.status.success() => {
                Some(String::from_utf8_lossy(&out.stdout).into_owned())
            }
            _ => None,
        }
    }

    /// The charon log's current length, taken before a recovery is issued so
    /// [`Self::fresh_pcscf_since`] can tell the new negotiation from the old.
    /// `0` if the log cannot be read, which makes any closed batch count.
    pub fn log_mark(&self, runner: &dyn CommandRunner) -> LogMark {
        LogMark {
            generation: self.shared.log_generation.load(Ordering::SeqCst),
            len: runner
                .read_file(&self.shared.charon_log)
                .map(|c| c.len())
                .unwrap_or(0),
        }
    }

    /// The P-CSCF of a negotiation logged after `mark`, if one has completed.
    /// A log truncated since the mark (charon restarted) is read from its
    /// start: everything in it is newer than the mark.
    ///
    /// Only addresses this line's `address_family` admits count (FR-013).
    pub fn fresh_pcscf_since(&self, runner: &dyn CommandRunner, mark: LogMark) -> Option<String> {
        self.fresh_pcscf_pool_since(runner, mark)
            .and_then(|pool| pick_stable_pcscf(&pool))
    }

    /// The whole pool of a negotiation logged after `mark`, filtered to this
    /// line's address family; `None` while there is none, or when the family
    /// excludes every address. See [`Self::fresh_pcscf_since`].
    pub fn fresh_pcscf_pool_since(
        &self,
        runner: &dyn CommandRunner,
        mark: LogMark,
    ) -> Option<Vec<String>> {
        let offset = if self.shared.log_generation.load(Ordering::SeqCst) == mark.generation {
            mark.len
        } else {
            0
        };
        runner
            .read_file(&self.shared.charon_log)
            .ok()
            .and_then(|c| extract_pcscf_pool_after(&c, &self.conn_name, offset))
            .map(|pool| self.filtered(pool))
            .filter(|pool| !pool.is_empty())
    }

    /// Keeps the addresses this line's `address_family` admits (specs/092
    /// FR-013). A no-op for `Dual`.
    fn filtered(&self, batch: Vec<String>) -> Vec<String> {
        batch
            .into_iter()
            .filter(|a| self.address_family.admits(a))
            .collect()
    }

    /// The latest negotiation's P-CSCFs that this line's address family
    /// excludes, for the "ignoring P-CSCF" log at each publication point.
    /// Always empty for `Dual`.
    pub fn ignored_pcscfs(&self, runner: &dyn CommandRunner) -> Vec<String> {
        runner
            .read_file(&self.shared.charon_log)
            .map(|c| extract_latest_pcscf_pool(&c, &self.conn_name))
            .unwrap_or_default()
            .into_iter()
            .filter(|a| !self.address_family.admits(a))
            .collect()
    }

    /// [`child_rekey_invalid_selectors_session`] over the shared charon log.
    pub fn rekey_invalid_selectors_session(&self, runner: &dyn CommandRunner) -> Option<String> {
        runner
            .read_file(&self.shared.charon_log)
            .ok()
            .and_then(|c| child_rekey_invalid_selectors_session(&c, &self.conn_name))
    }

    /// Backgrounded `swanctl` invocation — matches the bash calls that end
    /// in `&` (every `--initiate`). Fire-and-forget: like the bash version,
    /// the spawned process's handle is never tracked or waited on (the
    /// script only ever redirects its output to a log file, `>>"...
    /// -initiate-$idx.log" 2>&1 &`, and never references its pid again).
    /// Not backgrounding this was a real bug caught by review: running
    /// `--initiate` synchronously via `run()` blocks the calling supervisor
    /// tick for as long as IKE negotiation takes, exactly the polling stall
    /// specs/012-strongswan-epdg's own establish/steady-state loops are
    /// designed to avoid by backgrounding it in the first place.
    fn swanctl_background(&self, runner: &dyn CommandRunner, args: &[&str]) {
        let env = self.env_prefix();
        let mut argv = vec!["env".to_string(), env, "swanctl".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        // spawn_detached, not spawn: a real review finding caught that
        // `spawn` + discarding the returned handle still leaves an
        // un-reapable entry in RealCommandRunner's table forever (nothing
        // holds the handle needed to remove it) — every repeated tunnel
        // recovery over a long-running container's lifetime would leak one.
        // `spawn_detached` never inserts a tracked entry at all, matching
        // this call's true fire-and-forget nature.
        let _ = runner.spawn_detached(ChildSpec::new(argv));
    }
}

impl TunnelEngine for StrongswanEngine {
    fn pcscf_reachable(&self, runner: &dyn CommandRunner, pcscf: &str) -> bool {
        if pcscf.is_empty() {
            // Nothing captured yet: not a failure, just nothing to test.
            return true;
        }
        runner.tcp_connect_ok_in_netns(&self.netns, pcscf, super::line_supervisor::PCSCF_SIP_PORT)
    }

    fn is_tunnel_established(&self, runner: &dyn CommandRunner) -> bool {
        runner
            .read_file(&self.shared.charon_log)
            .map(|c| charon_log_shows_established(&c, &self.conn_name))
            .unwrap_or(false)
    }

    /// `pick_stable_pcscf` over the family-filtered pool. For `Dual` that is
    /// exactly [`extract_latest_pcscf`]: the pick is a minimum, so
    /// deduplicating the batch first cannot change it.
    fn latest_pcscf(&self, runner: &dyn CommandRunner) -> Option<String> {
        pick_stable_pcscf(&self.latest_pcscf_pool(runner))
    }

    fn latest_pcscf_pool(&self, runner: &dyn CommandRunner) -> Vec<String> {
        self.filtered(
            runner
                .read_file(&self.shared.charon_log)
                .map(|c| extract_latest_pcscf_pool(&c, &self.conn_name))
                .unwrap_or_default(),
        )
    }

    fn is_process_alive(&self, runner: &dyn CommandRunner) -> bool {
        self.shared.is_alive(runner)
    }

    fn ike_sa_loss_cause(&self, runner: &dyn CommandRunner) -> IkeSaLossCause {
        runner
            .read_file(&self.shared.charon_log)
            .map(|c| ike_sa_loss_cause(&c, &self.conn_name))
            .unwrap_or(IkeSaLossCause::Unknown)
    }

    fn log_prefix(&self) -> String {
        format!("[supervise] line {}: {}", self.idx, self.conn_name)
    }

    fn child_sa_rejection(&self, runner: &dyn CommandRunner) -> Option<String> {
        runner
            .read_file(&self.shared.charon_log)
            .ok()
            .and_then(|c| child_sa_rejection_notify(&c, &self.conn_name))
    }

    /// Graceful delete first, forced only if the ePDG does not acknowledge it
    /// (specs/087, research R3).
    ///
    /// A non-forced terminate sends the DELETE and waits up to
    /// `GRACEFUL_TERMINATE_WAIT_SECS` for the ePDG's response, which is the
    /// ePDG's own statement that the old session is gone. That is what a fixed
    /// post-terminate sleep used to guess at (#106). Whether it was
    /// acknowledged is decided by presence (a follow-up `--list-sas`), not by
    /// swanctl's exit status; a failed presence query counts as "still there",
    /// because forcing is the only safe step then. `--force` is never combined
    /// with swanctl's `--timeout`, which disables its no-wait behaviour (the
    /// outer `timeout` bounds it instead; #107 spent 52s on a stuck exchange).
    ///
    /// No sleep on either path: on the forced path the ePDG is not answering,
    /// so waiting buys nothing. Worst case is 8 + 10 + 10 = 28s, under the
    /// shutdown read guard; `shutdown.rs`'s own teardown is already non-forced
    /// with a 5s bound and is intentionally unchanged.
    fn terminate(&self, runner: &dyn CommandRunner) {
        let env = self.env_prefix();
        let graceful_outer = GRACEFUL_TERMINATE_TIMEOUT_SECS.to_string();
        let graceful_wait = GRACEFUL_TERMINATE_WAIT_SECS.to_string();
        let _ = runner.run(&[
            "timeout",
            &graceful_outer,
            "env",
            &env,
            "swanctl",
            "--terminate",
            "--ike",
            &self.conn_name,
            "--timeout",
            &graceful_wait,
        ]);
        if self
            .list_sas(runner)
            .is_some_and(|out| !list_sas_has_ike_sa(&out, &self.conn_name))
        {
            println!(
                "[supervise] line {}: IKE_SA deleted (acknowledged)",
                self.idx
            );
            return;
        }
        println!(
            "[supervise] line {}: ePDG did not acknowledge DELETE within {GRACEFUL_TERMINATE_WAIT_SECS}s; forced",
            self.idx
        );
        let secs = STEADY_TERMINATE_TIMEOUT_SECS.to_string();
        let _ = runner.run(&[
            "timeout",
            &secs,
            "env",
            &env,
            "swanctl",
            "--terminate",
            "--ike",
            &self.conn_name,
            "--force",
        ]);
    }

    fn reinitiate(&self, runner: &dyn CommandRunner) {
        self.swanctl_background(runner, &["--initiate", "--child", &self.conn_name]);
    }

    fn steady_state_health(&self, runner: &dyn CommandRunner) -> SteadyStateHealth {
        self.last_inbound.set(None);
        self.last_tun_rx.set(None);
        *self.last_ike_header.borrow_mut() = None;
        // `-s` for the counter a dead-peer verdict compares across ticks; the
        // existence check is the same.
        let tun_check =
            runner.run_in_netns(&self.netns, &["ip", "-s", "link", "show", &self.tun_iface]);
        let Some(tun_check) = tun_check.ok().filter(|o| o.status.success()) else {
            println!(
                "[supervise] line {}: {} missing from netns {}; recreating and forcing reinitiate",
                self.idx, self.tun_iface, self.netns
            );
            return SteadyStateHealth::TunVanished;
        };
        let tun_rx = link_rx_packets(&String::from_utf8_lossy(&tun_check.stdout));
        self.last_tun_rx.set(tun_rx);

        // Bounded and scoped to this line: see `list_sas`.
        let Some(sas_output) = self.list_sas(runner) else {
            println!(
                "[supervise] line {}: swanctl --list-sas failed (vici connection broken)",
                self.idx
            );
            return SteadyStateHealth::ViciBroken;
        };
        if !list_sas_has_ike_sa(&sas_output, &self.conn_name) {
            println!(
                "[supervise] line {}: {} IKE_SA missing; re-initiating",
                self.idx, self.conn_name
            );
            return SteadyStateHealth::IkeSaMissing;
        }
        let header = ike_sa_header(&sas_output, &self.conn_name);
        *self.last_ike_header.borrow_mut() = header.clone();
        // A half-open IKE_SA is charon's own negotiation in flight, not a lost
        // CHILD_SA: the tick leaves it to charon while it is still working
        // (specs/100 US3).
        if let Some((ike_id, IkeHeaderState::Connecting)) = header {
            return SteadyStateHealth::IkeSaConnecting { ike_id };
        }
        if !list_sas_has_child_sa(&sas_output, &self.conn_name) {
            println!(
                "[supervise] line {}: {} CHILD_SA missing; re-initiating",
                self.idx, self.conn_name
            );
            return SteadyStateHealth::ChildSaMissing;
        }
        self.last_inbound.set(
            inbound_esp(&sas_output, &self.conn_name).map(|esp| InboundEsp {
                tun_rx_packets: tun_rx,
                ..esp
            }),
        );
        SteadyStateHealth::Ok(newest_child_sa_lifetime(&sas_output, &self.conn_name))
    }

    fn inbound_esp(&self) -> Option<InboundEsp> {
        self.last_inbound.get()
    }

    fn epdg_addr(&self) -> Option<&str> {
        self.epdg_addr.as_deref()
    }

    fn route_ok(&self, runner: &dyn CommandRunner) -> bool {
        self.epdg_addr
            .as_deref()
            .is_none_or(|addr| route_to(runner, addr))
    }

    fn ike_header(&self) -> Option<(u32, IkeHeaderState)> {
        self.last_ike_header.borrow().clone()
    }

    fn ike_activity(&self, runner: &dyn CommandRunner) -> Option<IkeActivity> {
        runner
            .read_file(&self.shared.charon_log)
            .ok()
            .and_then(|c| ike_activity(&c, &self.conn_name))
    }

    /// A log truncated since the mark (charon restarted) is read from its
    /// start: everything in it is newer than the mark. An unreadable log reads
    /// as life, so a failed read never holds a line.
    fn ike_evidence_since(&self, runner: &dyn CommandRunner, mark: LogMark) -> IkeEvidence {
        let offset = if self.shared.log_generation.load(Ordering::SeqCst) == mark.generation {
            mark.len
        } else {
            0
        };
        runner
            .read_file(&self.shared.charon_log)
            .map(|c| ike_evidence(log_after(&c, offset), &self.conn_name))
            .unwrap_or(IkeEvidence::LIFE)
    }

    fn log_mark(&self, runner: &dyn CommandRunner) -> LogMark {
        StrongswanEngine::log_mark(self, runner)
    }

    /// `swanctl --dpd --ike <conn>` under the bounded wrapper `list_sas` uses
    /// (specs/100 contracts/dpd-command.md). The command only queues the
    /// exchange; the answer is read from the log on a later tick.
    fn send_dpd(&self, runner: &dyn CommandRunner) -> bool {
        let env = self.env_prefix();
        let secs = STEADY_LIST_SAS_TIMEOUT_SECS.to_string();
        runner
            .run(&[
                "timeout",
                &secs,
                "env",
                &env,
                "swanctl",
                "--dpd",
                "--ike",
                &self.conn_name,
            ])
            .is_ok_and(|o| o.status.success())
    }

    fn tun_rx_packets(&self) -> Option<u64> {
        self.last_tun_rx.get()
    }

    /// Recovery for "the process died" and "vici is unreachable".
    ///
    /// Restarting the daemon is the one genuinely *global* action here, so it
    /// happens only when the daemon is actually dead — and `restart_if_dead`
    /// makes that check-and-respawn atomic, so two lines noticing the same
    /// death don't restart it twice or kill each other's replacement. While
    /// the daemon is alive, this line's trouble is its own, and recovery stays
    /// scoped to its connection so other lines' tunnels keep running.
    fn restart_process(&self, runner: &dyn CommandRunner) {
        let respawned = self.shared.restart_if_dead(runner);
        // Safe either way: loading the union of conf.d cannot evict anything.
        // Required after a respawn (a fresh daemon has nothing loaded), and a
        // harmless no-op otherwise.
        self.shared.load_all(runner);
        if !respawned {
            // Only meaningful against a daemon still holding a stale SA for
            // this connection; a freshly spawned one has nothing to terminate.
            self.terminate(runner);
        }
        self.reinitiate(runner);
    }

    fn max_establish_attempts(&self) -> Option<u32> {
        self.max_establish_attempts
    }

    fn reinitiate_cadence(&self) -> Option<u32> {
        Some(super::line_supervisor::STRONGSWAN_REINITIATE_EVERY)
    }

    fn repair_default_route(&self, runner: &dyn CommandRunner, pcscf: &str) -> bool {
        use super::epdg_iface::{ensure_default_route, has_default_route, RouteFamily};
        // The P-CSCF's family is the route that matters. One that does not
        // parse is never expected (it comes from charon's log as an address),
        // so fall back to requiring both rather than guessing one.
        let families = match RouteFamily::of_addr(pcscf) {
            Some(f) => vec![f],
            None => RouteFamily::ALL.to_vec(),
        };
        let missing = |f: RouteFamily| !has_default_route(runner, &self.netns, &self.tun_iface, f);
        let Some(gone) = families.iter().copied().find(|&f| missing(f)) else {
            return false;
        };
        println!(
            "[supervise] line {}: netns {} has no {} default route via {} — the P-CSCF is \
             unreachable for that reason alone, not because the tunnel is broken; reinstating it",
            self.idx,
            self.netns,
            gone.label(),
            self.tun_iface
        );
        ensure_default_route(runner, &self.netns, &self.tun_iface);
        !families.into_iter().any(missing)
    }

    fn recreate_interface(&self, runner: &dyn CommandRunner) -> bool {
        if super::epdg_iface::ensure_epdg_interface(
            runner,
            &self.netns,
            &self.tun_iface,
            &self.if_id,
        ) {
            return true;
        }
        // Without this the loop is silent and endless: steady-state sees a
        // missing interface every 30s, calls this, and finds it missing again.
        //
        // specs/041-shutdown-resource-cleanup: this used to assume the usual
        // cause was a previous container run's namespace not yet reaped, and
        // that waiting alone was the remedy. That stopped being the common
        // case once a graceful stop started deleting the device explicitly
        // and a fresh start reclaims one an ungraceful exit left behind (see
        // epdg_iface::ensure_epdg_interface's doc comment for the full
        // account) — reaching this mid-run now more likely means something
        // else outside this deployment is holding the if_id.
        println!(
            "[supervise] line {}: {} could not be recreated in netns {} (if_id {} not \
             available yet); leaving this line's SA alone and retrying next tick. \
             See docs/operations.md",
            self.idx, self.tun_iface, self.netns, self.if_id
        );
        false
    }
}

/// 1:1 port of `grep -q "STATE CONNECTED"`.
pub fn swu_log_shows_connected(log: &str) -> bool {
    log.contains("STATE CONNECTED")
}

/// 1:1 port of the swu dialer's P-CSCF extraction (`grep 'P-CSCF IPV4
/// ADDRESS'`, falling back to the IPV6 line only when no IPv4 line matched).
pub fn extract_swu_pcscf(log: &str) -> Option<String> {
    let v4 = log
        .lines()
        .find(|l| l.contains("P-CSCF IPV4 ADDRESS"))
        .and_then(|l| l.split_whitespace().find(|tok| tok.contains('.')));
    if let Some(addr) = v4 {
        return Some(addr.to_string());
    }
    log.lines()
        .find(|l| l.contains("P-CSCF IPV6 ADDRESS"))
        .and_then(|l| {
            l.split_whitespace().find(|tok| {
                // Must actually look like an IPv6 address (only hex digits
                // and colons) — a naive `tok.contains(':')` also matches the
                // literal word "ADDRESS:" earlier in the same line.
                tok.contains(':') && tok.chars().all(|c| c.is_ascii_hexdigit() || c == ':')
            })
        })
        .map(str::to_string)
}

/// Every P-CSCF address the swu dialer logged, IPv4 and IPv6 lines alike, in
/// log order and deduplicated (specs/091 R1). [`extract_swu_pcscf`] stays the
/// single selection.
pub fn extract_swu_pcscf_pool(log: &str) -> Vec<String> {
    let addrs = log
        .lines()
        .filter(|l| l.contains("P-CSCF IPV4 ADDRESS") || l.contains("P-CSCF IPV6 ADDRESS"))
        .filter_map(|l| {
            l.split_whitespace()
                .find(|tok| tok.parse::<std::net::IpAddr>().is_ok())
                .map(str::to_string)
        })
        .collect();
    dedup_pool(addrs)
}

/// One line's swu engine state.
pub struct SwuEngine {
    pub idx: u32,
    pub modem: String,
    pub apn: String,
    pub mcc: String,
    pub mnc: String,
    pub netns: String,
    pub src_addr: Option<String>,
    pub log_file: PathBuf,
    pub dialer_handle: RefCell<Option<std::sync::Arc<ChildHandle>>>,
}

impl SwuEngine {
    fn spawn_dialer(&self, runner: &dyn CommandRunner) -> Option<ChildHandle> {
        let mut argv = vec![
            "python3".to_string(),
            "-u".to_string(),
            "swu_emulator.py".to_string(),
            "-m".to_string(),
            self.modem.clone(),
            "-a".to_string(),
            self.apn.clone(),
            "-M".to_string(),
            self.mcc.clone(),
            "-N".to_string(),
            self.mnc.clone(),
            "-n".to_string(),
            self.netns.clone(),
        ];
        if let Some(src) = &self.src_addr {
            argv.push("-s".to_string());
            argv.push(src.clone());
        }
        runner
            .spawn(ChildSpec::new(argv).capture_stdout_to(self.log_file.clone()))
            .ok()
    }
}

impl TunnelEngine for SwuEngine {
    fn pcscf_reachable(&self, runner: &dyn CommandRunner, pcscf: &str) -> bool {
        if pcscf.is_empty() {
            // Nothing captured yet: not a failure, just nothing to test.
            return true;
        }
        runner.tcp_connect_ok_in_netns(&self.netns, pcscf, super::line_supervisor::PCSCF_SIP_PORT)
    }

    fn is_tunnel_established(&self, runner: &dyn CommandRunner) -> bool {
        runner
            .read_file(&self.log_file)
            .map(|c| swu_log_shows_connected(&c))
            .unwrap_or(false)
    }

    fn latest_pcscf(&self, runner: &dyn CommandRunner) -> Option<String> {
        runner
            .read_file(&self.log_file)
            .ok()
            .and_then(|c| extract_swu_pcscf(&c))
    }

    fn latest_pcscf_pool(&self, runner: &dyn CommandRunner) -> Vec<String> {
        runner
            .read_file(&self.log_file)
            .map(|c| extract_swu_pcscf_pool(&c))
            .unwrap_or_default()
    }

    fn is_process_alive(&self, runner: &dyn CommandRunner) -> bool {
        self.dialer_handle
            .borrow()
            .as_ref()
            .is_some_and(|h| runner.is_alive(h))
    }

    // No in-place terminate/reinitiate concept for this engine (the current
    // script's own comment: "no re-initiate-in-place concept for this
    // engine — recovery is restarting the dialer for this line only").
    fn terminate(&self, _runner: &dyn CommandRunner) {}
    fn reinitiate(&self, _runner: &dyn CommandRunner) {}

    fn steady_state_health(&self, _runner: &dyn CommandRunner) -> SteadyStateHealth {
        // The swu steady-state loop has exactly one check (is the dialer
        // alive), already covered by `is_process_alive` before this is ever
        // consulted — so there is nothing left for this engine to report.
        SteadyStateHealth::Ok(None)
    }

    fn restart_process(&self, runner: &dyn CommandRunner) {
        if let Some(old) = self.dialer_handle.borrow_mut().take() {
            // Same fix as StrongswanEngine::restart_charon (Greptile P1):
            // reap the old dialer before spawning its replacement, so its
            // table entry doesn't leak and it can't overlap/contend with
            // the new one.
            runner.reap(&old);
        }
        let _ = runner.write_file(&self.log_file, "");
        *self.dialer_handle.borrow_mut() = self.spawn_dialer(runner).map(std::sync::Arc::new);
    }

    fn max_establish_attempts(&self) -> Option<u32> {
        Some(super::line_supervisor::SWU_MAX_ESTABLISH_ATTEMPTS)
    }

    fn reinitiate_cadence(&self) -> Option<u32> {
        None
    }

    fn recreate_interface(&self, _runner: &dyn CommandRunner) -> bool {
        // No pre-created interface concept for this engine — the dialer
        // manages its own tun device, and `restart_process` (a full dialer
        // respawn) is this engine's only recovery path, matching the
        // current script's own comment on `start_line_swu`. `true` so this
        // engine's recovery is never gated on an interface it does not have.
        true
    }

    fn repair_default_route(&self, _runner: &dyn CommandRunner, _pcscf: &str) -> bool {
        // Same reason: the dialer owns this engine's device and its routing, so
        // there is no externally-managed route here to have gone missing.
        // `false` means "nothing repaired", which leaves the caller's escalation
        // exactly as it was for this engine.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charon_log_established_matches_the_grep_pattern() {
        assert!(charon_log_shows_established(
            "12[IKE] <ims0|1> CHILD_SA ims0{1} established with SPIs...",
            "ims0"
        ));
        assert!(!charon_log_shows_established("nothing relevant", "ims0"));
    }

    #[test]
    fn one_lines_established_tunnel_is_not_read_as_another_lines() {
        // The invariant that makes a shared charon log safe. Both lines write
        // into one file now, so without `<conn|` scoping line 1 would report
        // itself up the instant line 0 connected — and then sit waiting for a
        // P-CSCF that belongs to someone else.
        let log = "12[IKE] <ims0|1> CHILD_SA ims0{1} established with SPIs...\n";
        assert!(charon_log_shows_established(log, "ims0"));
        assert!(
            !charon_log_shows_established(log, "ims1"),
            "line 1 must not see line 0's CHILD_SA as its own"
        );
    }

    #[test]
    fn extract_latest_pcscf_prefers_ipv4_and_ignores_the_order_they_arrived_in() {
        // Deliberate change from "chronologically last": that made the value
        // move on its own, because the carrier returns one pool in varying
        // order and the steady-state check read a reorder as a real change.
        let log = "<ims0|1> received P-CSCF server IP 10.0.0.1\n<ims0|1> received P-CSCF server IP 2001:db8::1\n<ims0|1> received P-CSCF server IP 10.0.0.9\n";
        assert_eq!(
            extract_latest_pcscf(log, "ims0"),
            Some("10.0.0.1".to_string())
        );
    }

    #[test]
    fn the_same_pcscf_pool_in_any_order_yields_the_same_choice() {
        // The bug this guards: a pure reorder used to read as
        // `P-CSCF changed` and refresh the line, tearing down a healthy
        // registration. Observed repeatedly on Jio 2026-08-14.
        let pool = ["56.2.134.146", "10.56.156.21", "2405:200:666::15"];
        let one = format!(
            "<ims0|1> received P-CSCF server IP {}\n<ims0|1> received P-CSCF server IP {}\n<ims0|1> received P-CSCF server IP {}\n",
            pool[0], pool[1], pool[2]
        );
        let other = format!(
            "<ims0|1> received P-CSCF server IP {}\n<ims0|1> received P-CSCF server IP {}\n<ims0|1> received P-CSCF server IP {}\n",
            pool[2], pool[0], pool[1]
        );
        assert_eq!(
            extract_latest_pcscf(&one, "ims0"),
            extract_latest_pcscf(&other, "ims0")
        );
    }

    #[test]
    fn a_new_negotiation_replaces_the_previous_pcscf_batch() {
        // A genuinely different pool must still be picked up — the stickiness
        // above must not make the line blind to a real move.
        let log = "<ims0|1> received P-CSCF server IP 10.0.0.1\n\
                   <ims0|1> CHILD_SA ims0{1} established with SPIs\n\
                   <ims0|1> received P-CSCF server IP 10.7.7.7\n";
        assert_eq!(
            extract_latest_pcscf(log, "ims0"),
            Some("10.7.7.7".to_string())
        );
    }

    #[test]
    fn extract_latest_pcscf_pool_returns_the_whole_latest_batch_in_log_order() {
        let log = "[CFG] <ims0|1> received P-CSCF server IP 2402:8100::10\n\
                   [CFG] <ims0|1> received P-CSCF server IP 10.0.1.10\n\
                   [CFG] <ims1|2> received P-CSCF server IP 10.9.9.9\n\
                   [CFG] <ims0|1> received P-CSCF server IP 2402:8100::11\n\
                   [CFG] <ims0|1> received P-CSCF server IP 10.0.0.10\n\
                   [CFG] <ims0|1> received P-CSCF server IP 10.0.1.10\n";
        assert_eq!(
            extract_latest_pcscf_pool(log, "ims0"),
            vec!["2402:8100::10", "10.0.1.10", "2402:8100::11", "10.0.0.10"]
        );
        // The single selection is unchanged by publishing the pool (FR-004).
        assert_eq!(
            extract_latest_pcscf(log, "ims0"),
            Some("10.0.0.10".to_string())
        );
    }

    #[test]
    fn a_new_negotiation_replaces_the_previous_pcscf_pool() {
        let log = "<ims0|1> received P-CSCF server IP 10.0.0.10\n\
                   <ims0|1> received P-CSCF server IP 10.0.1.10\n\
                   <ims0|1> CHILD_SA ims0{1} established with SPIs\n\
                   <ims0|1> received P-CSCF server IP 10.0.2.10\n";
        assert_eq!(extract_latest_pcscf_pool(log, "ims0"), vec!["10.0.2.10"]);
        assert!(extract_latest_pcscf_pool(log, "ims1").is_empty());
    }

    #[test]
    fn extract_pcscf_pool_after_wants_a_closed_batch_after_the_mark() {
        let old = "<ims0|1> received P-CSCF server IP 10.0.0.10\n\
                   <ims0|1> CHILD_SA ims0{1} established with SPIs\n";
        let mark = old.len();
        assert_eq!(extract_pcscf_pool_after(old, "ims0", mark), None);
        let open = format!("{old}<ims0|2> received P-CSCF server IP 10.0.2.10\n");
        assert_eq!(extract_pcscf_pool_after(&open, "ims0", mark), None);
        let closed = format!(
            "{open}<ims0|2> received P-CSCF server IP 10.0.3.10\n\
             <ims0|2> CHILD_SA ims0{{2}} established with SPIs\n"
        );
        assert_eq!(
            extract_pcscf_pool_after(&closed, "ims0", mark),
            Some(vec!["10.0.2.10".to_string(), "10.0.3.10".to_string()])
        );
        assert_eq!(
            extract_pcscf_after(&closed, "ims0", mark),
            Some("10.0.2.10".to_string())
        );
    }

    #[test]
    fn each_line_extracts_only_its_own_pcscf_from_the_shared_log() {
        // Two lines interleaved in one file, line 1's address written last.
        // Each must still read its own — handing line 0 line 1's P-CSCF would
        // point its IMS agent at the wrong carrier's proxy.
        let log = "<ims0|1> received P-CSCF server IP 10.0.0.1\n\
                   <ims1|2> received P-CSCF server IP 10.9.9.9\n";
        assert_eq!(
            extract_latest_pcscf(log, "ims0"),
            Some("10.0.0.1".to_string())
        );
        assert_eq!(
            extract_latest_pcscf(log, "ims1"),
            Some("10.9.9.9".to_string())
        );
    }

    #[test]
    fn extract_latest_pcscf_matches_real_charons_facility_tagged_log_lines() {
        // Regression test for a real bug found live: real charon output
        // prefixes every line with a facility tag (`[CFG] `, `[IKE] `, ...),
        // e.g. `[CFG] received P-CSCF server IP 2401:4900:c4:4035::8` — an
        // earlier version used `strip_prefix`, which requires the marker at
        // the exact start of the line and so never matched real output,
        // silently keeping every line "stuck without P-CSCF" forever (caught
        // by live-testing against the real EC20 + Airtel SIM: the tunnel
        // established successfully but the establish loop never noticed).
        // IPv6-only pool, so the IPv4 preference does not apply and the
        // stable (minimum) member is chosen.
        let log = "[CFG] <ims0|1> received P-CSCF server IP 2401:4900:c4:4035::8\n[CFG] <ims0|1> received P-CSCF server IP 2401:4900:c4:4035::b\n";
        assert_eq!(
            extract_latest_pcscf(log, "ims0"),
            Some("2401:4900:c4:4035::8".to_string())
        );
    }

    fn ike_header(conn: &str) -> String {
        format!("{conn}: #1, ESTABLISHED, IKEv2, 0123456789abcdef_i* fedcba9876543210_r\n  local  '001010000000001@nai.epc.mnc001.mcc001.3gppnetwork.org' @ 198.51.100.10[4500]\n")
    }

    fn child(conn: &str, n: u32, state: &str) -> String {
        format!("  {conn}: #{n}, reqid 1, {state}, TUNNEL-in-UDP, ESP:AES_CBC-128/HMAC_SHA1_96\n    installed 10s ago\n")
    }

    /// A CHILD_SA in the exact shape the pinned 5.9.3 prints, details line
    /// included (specs/093 research R1, sampled live; identities synthetic).
    fn child_with_life(conn: &str, n: u32, state: &str, details: &str) -> String {
        format!(
            "  {conn}: #{n}, reqid 1, {state}, TUNNEL-in-UDP, ESP:AES_CBC-128/HMAC_SHA1_96\n    {details}\n    in  cea6576a (-|0x00000017),  21600 bytes,   242 packets,     4s ago\n    local  192.0.2.10/32\n    remote 0.0.0.0/0\n"
        )
    }

    fn life(n: u32, expires_in: Option<i64>) -> Option<ChildSaLifetime> {
        Some(ChildSaLifetime {
            unique_id: n,
            expires_in,
        })
    }

    /// A CHILD_SA with an explicit inbound line (`in_tail` is everything after
    /// the SPI, as the pinned 5.9.3 prints it).
    fn child_with_in(conn: &str, n: u32, state: &str, in_tail: &str) -> String {
        format!(
            "  {conn}: #{n}, reqid 1, {state}, TUNNEL-in-UDP, ESP:AES_CBC-128/HMAC_SHA1_96\n    installed 10s ago, rekeying in 3000s, expires in 3600s\n    in  cea6576a (-|0x00000017), {in_tail}\n    out 0badf00d (-|0x00000017),  1200 bytes,    20 packets,     2s ago\n    local  192.0.2.10/32\n    remote 0.0.0.0/0\n"
        )
    }

    fn esp(age: Option<u64>, packets: u64) -> Option<InboundEsp> {
        Some(InboundEsp {
            last_in_age: age.map(Duration::from_secs),
            packets,
            tun_rx_packets: None,
        })
    }

    #[test]
    fn link_rx_packets_reads_the_packets_column_by_its_header() {
        // iproute2 6.x, as the live rig prints it.
        let new = "344: tun23-0@NONE: <NOARP,UP,LOWER_UP> mtu 1500 qdisc noqueue state UNKNOWN mode DEFAULT group default qlen 1000\n    link/none  link-netnsid 0\n    RX:  bytes packets errors dropped  missed   mcast           \n         63043     379      3       3       0       0 \n    TX:  bytes packets errors dropped carrier collsns           \n         79828     688     11      11      11       0 \n";
        assert_eq!(link_rx_packets(new), Some(379));
        // Older iproute2 names the columns differently.
        let old = "5: tun23: <NOARP,UP,LOWER_UP> mtu 1500\n    link/none \n    RX: bytes  packets  errors  dropped overrun mcast   \n    1200       12       0       0       0       0       \n    TX: bytes  packets  errors  dropped carrier collsns \n    900        9        0       0       0       0       \n";
        assert_eq!(link_rx_packets(old), Some(12));
        assert_eq!(link_rx_packets("5: tun23\n"), None);
    }

    #[test]
    fn inbound_esp_reads_the_last_use_age_and_packets() {
        let sas = format!(
            "{}{}",
            ike_header("ims0"),
            child_with_in(
                "ims0",
                3,
                "INSTALLED",
                " 21600 bytes,   242 packets,     4s ago"
            )
        );
        assert_eq!(inbound_esp(&sas, "ims0"), esp(Some(4), 242));
    }

    #[test]
    fn inbound_esp_takes_the_youngest_age_across_a_rekeys_two_sas() {
        let sas = format!(
            "{}{}{}",
            ike_header("ims0"),
            child_with_in(
                "ims0",
                3,
                "REKEYED",
                " 21600 bytes,   242 packets,    95s ago"
            ),
            child_with_in(
                "ims0",
                4,
                "INSTALLED",
                "   300 bytes,     3 packets,    12s ago"
            )
        );
        assert_eq!(inbound_esp(&sas, "ims0"), esp(Some(12), 245));
    }

    #[test]
    fn inbound_esp_without_any_inbound_use_has_no_age() {
        let sas = format!(
            "{}{}",
            ike_header("ims0"),
            child_with_in("ims0", 4, "INSTALLED", "     0 bytes,     0 packets")
        );
        assert_eq!(inbound_esp(&sas, "ims0"), esp(None, 0));
    }

    #[test]
    fn inbound_esp_ignores_another_connections_sa() {
        let sas = format!(
            "{}{}{}{}",
            ike_header("ims1"),
            child_with_in(
                "ims1",
                7,
                "INSTALLED",
                " 9000 bytes,   90 packets,     1s ago"
            ),
            ike_header("ims0"),
            child_with_in(
                "ims0",
                3,
                "INSTALLED",
                " 21600 bytes,   242 packets,    40s ago"
            )
        );
        assert_eq!(inbound_esp(&sas, "ims0"), esp(Some(40), 242));
        assert_eq!(inbound_esp(&sas, "ims10"), None);
    }

    #[test]
    fn inbound_esp_with_no_child_sa_is_none() {
        assert_eq!(inbound_esp(&ike_header("ims0"), "ims0"), None);
        assert_eq!(inbound_esp("", "ims0"), None);
    }

    #[test]
    fn child_sa_lifetime_table() {
        let h = ike_header("ims0");
        let full = "installed 1109s ago, rekeying in 2167s, expires in 2852s";
        let cases: Vec<(String, Option<ChildSaLifetime>)> = vec![
            (
                format!("{h}{}", child_with_life("ims0", 12, "INSTALLED", full)),
                life(12, Some(2852)),
            ),
            // A natural rekey's overlap: the replacement is the one judged.
            (
                format!(
                    "{h}{}{}",
                    child_with_life(
                        "ims0",
                        12,
                        "REKEYED",
                        "installed 3660s ago, expires in 300s"
                    ),
                    child_with_life(
                        "ims0",
                        13,
                        "INSTALLED",
                        "installed 0s ago, rekeying in 3300s, expires in 3960s"
                    )
                ),
                life(13, Some(3960)),
            ),
            (
                format!(
                    "{h}{}",
                    child_with_life(
                        "ims0",
                        12,
                        "REKEYING",
                        "installed 3670s ago, rekeying in -40s, expires in 290s"
                    )
                ),
                life(12, Some(290)),
            ),
            // A replacement still being installed is not usable yet.
            (
                format!(
                    "{h}{}{}",
                    child_with_life("ims0", 12, "INSTALLED", full),
                    child_with_life("ims0", 13, "INSTALLING", "installed 0s ago")
                ),
                life(12, Some(2852)),
            ),
            (
                format!(
                    "{h}{}",
                    child_with_life("ims0", 12, "INSTALLED", "installed 1109s ago")
                ),
                life(12, None),
            ),
            (
                format!("{h}{}", child("ims0", 4, "INSTALLED")),
                life(4, None),
            ),
            (
                format!(
                    "{}{}",
                    ike_header("ims1"),
                    child_with_life("ims1", 2, "INSTALLED", full)
                ),
                None,
            ),
            (
                format!(
                    "{}{}",
                    ike_header("ims10"),
                    child_with_life("ims10", 2, "INSTALLED", full)
                ),
                None,
            ),
            (h.clone(), None),
            (String::new(), None),
        ];
        for (i, (out, want)) in cases.iter().enumerate() {
            assert_eq!(
                newest_child_sa_lifetime(out, "ims0"),
                *want,
                "case {i}: {out}"
            );
        }
        assert_eq!(
            newest_child_sa_lifetime(
                &format!(
                    "{}{}",
                    ike_header("ims10"),
                    child_with_life("ims10", 2, "INSTALLED", full)
                ),
                "ims1"
            ),
            None
        );
    }

    #[test]
    fn list_sas_child_detection_table() {
        let h = ike_header("ims0");
        let cases: Vec<(String, bool)> = vec![
            (h.clone(), false),
            (h.replace("ESTABLISHED", "CONNECTING"), false),
            (format!("{h}{}", child("ims0", 4, "INSTALLED")), true),
            (
                format!(
                    "{h}{}{}",
                    child("ims0", 3, "REKEYED"),
                    child("ims0", 4, "INSTALLED")
                ),
                true,
            ),
            (format!("{h}{}", child("ims0", 4, "REKEYING")), true),
            (format!("{h}{}", child("ims0", 4, "UPDATING")), true),
            (format!("{h}{}", child("ims0", 4, "INSTALLING")), false),
            (format!("{h}{}", child("ims0", 4, "DELETING")), false),
            (format!("{h}{}", child("ims1", 4, "INSTALLED")), false),
            (
                format!("{h}    ims0: #4, reqid 1, INSTALLED, TUNNEL\n"),
                false,
            ),
            (
                format!("{}{}{h}", ike_header("ims1"), child("ims1", 2, "INSTALLED")),
                false,
            ),
            (String::new(), false),
        ];
        for (i, (out, want)) in cases.iter().enumerate() {
            assert_eq!(list_sas_has_child_sa(out, "ims0"), *want, "case {i}: {out}");
        }
    }

    #[test]
    fn list_sas_ike_detection_table() {
        let h = ike_header("ims0");
        let cases: Vec<(String, bool)> = vec![
            (format!("{h}{}", child("ims0", 4, "INSTALLED")), true),
            (h.clone(), true),
            (h.replace("ESTABLISHED", "CONNECTING"), true),
            (String::new(), false),
            (ike_header("ims1"), false),
            (child("ims0", 4, "INSTALLED"), false),
            (ike_header("ims10"), false),
        ];
        for (i, (out, want)) in cases.iter().enumerate() {
            assert_eq!(list_sas_has_ike_sa(out, "ims0"), *want, "case {i}: {out}");
        }
    }

    #[test]
    fn ike_sa_loss_cause_table() {
        let est = "09[IKE] <ims0|3> IKE_SA ims0[3] established between 198.51.100.10[a]...203.0.113.1[b]\n";
        let del = "11[IKE] <ims0|3> received DELETE for IKE_SA ims0[3]\n";
        let dpd = "11[IKE] <ims0|3> giving up after 5 retransmits\n";
        let other = "11[IKE] <ims1|2> received DELETE for IKE_SA ims1[2]\n";
        let cases: Vec<(String, IkeSaLossCause)> = vec![
            (format!("{est}{del}"), IkeSaLossCause::CarrierDelete),
            (format!("{est}{dpd}"), IkeSaLossCause::DeadPeer),
            (est.to_string(), IkeSaLossCause::Unknown),
            (format!("{del}{est}"), IkeSaLossCause::Unknown),
            (format!("{est}{dpd}{del}"), IkeSaLossCause::CarrierDelete),
            (format!("{est}{del}{dpd}"), IkeSaLossCause::DeadPeer),
            (format!("{est}{other}"), IkeSaLossCause::Unknown),
            (String::new(), IkeSaLossCause::Unknown),
        ];
        for (i, (log, want)) in cases.iter().enumerate() {
            assert_eq!(ike_sa_loss_cause(log, "ims0"), *want, "case {i}: {log}");
        }
    }

    #[test]
    fn extract_pcscf_after_only_accepts_a_closed_batch_logged_after_the_mark() {
        let old = "[CFG] <ims0|1> received P-CSCF server IP 192.0.2.10\n[IKE] <ims0|1> CHILD_SA ims0{3} established with SPIs a_i b_o\n";
        let new_open = "[CFG] <ims0|2> received P-CSCF server IP 192.0.2.11\n";
        let new_close = "[IKE] <ims0|2> CHILD_SA ims0{4} established with SPIs c_i d_o\n";
        let mark = old.len();
        // Nothing new yet.
        assert_eq!(extract_pcscf_after(old, "ims0", mark), None);
        // New batch, not yet closed.
        let open = format!("{old}{new_open}");
        assert_eq!(extract_pcscf_after(&open, "ims0", mark), None);
        // New batch, closed.
        let closed = format!("{old}{new_open}{new_close}");
        assert_eq!(
            extract_pcscf_after(&closed, "ims0", mark),
            Some("192.0.2.11".to_string())
        );
        // Truncated log: the whole log is read.
        assert_eq!(
            extract_pcscf_after(&closed, "ims0", closed.len() + 100),
            Some("192.0.2.11".to_string())
        );
        // Another connection's batch does not count.
        let other = format!("{old}{}", closed[old.len()..].replace("ims0", "ims1"));
        assert_eq!(extract_pcscf_after(&other, "ims0", mark), None);
    }

    #[test]
    fn extract_pcscf_after_never_panics_on_an_awkward_mark() {
        let log = "[CFG] <ims0|1> received P-CSCF server IP 192.0.2.10\n[IKE] <ims0|1> CHILD_SA ims0{3} established é\n";
        for mark in 0..=log.len() + 2 {
            let _ = extract_pcscf_after(log, "ims0", mark);
        }
        // A mark mid-line skips the rest of that line.
        assert_eq!(extract_pcscf_after(log, "ims0", 10), None);
    }

    #[test]
    fn swu_connected_marker_matches_the_grep_pattern() {
        assert!(swu_log_shows_connected("... STATE CONNECTED ..."));
        assert!(!swu_log_shows_connected("STATE CONNECTING"));
    }

    #[test]
    fn extract_swu_pcscf_prefers_ipv4_over_ipv6() {
        let log = "P-CSCF IPV4 ADDRESS: 10.1.2.3\nP-CSCF IPV6 ADDRESS: 2001:db8::1\n";
        assert_eq!(extract_swu_pcscf(log), Some("10.1.2.3".to_string()));
    }

    #[test]
    fn extract_swu_pcscf_falls_back_to_ipv6_when_no_ipv4_present() {
        let log = "P-CSCF IPV6 ADDRESS: 2001:db8::1\n";
        assert_eq!(extract_swu_pcscf(log), Some("2001:db8::1".to_string()));
    }

    #[test]
    fn extract_swu_pcscf_pool_collects_both_families_in_order() {
        let log = "P-CSCF IPV6 ADDRESS: 2402:8100::10\n\
                   P-CSCF IPV4 ADDRESS: 10.0.0.10\n\
                   P-CSCF IPV4 ADDRESS: 10.0.1.10\n\
                   P-CSCF IPV4 ADDRESS: 10.0.0.10\n";
        assert_eq!(
            extract_swu_pcscf_pool(log),
            vec!["2402:8100::10", "10.0.0.10", "10.0.1.10"]
        );
        assert!(extract_swu_pcscf_pool("STATE CONNECTED\n").is_empty());
    }

    #[test]
    fn extract_swu_pcscf_none_when_neither_present() {
        assert_eq!(extract_swu_pcscf("STATE CONNECTED\n"), None);
    }

    #[test]
    fn child_rekey_invalid_selectors_session_table() {
        const EST3: &str =
            "<ims0|3> IKE_SA ims0[3] established between 10.0.0.2[x]...192.0.2.1[ims]\n";
        const EST4: &str =
            "<ims0|4> IKE_SA ims0[4] established between 10.0.0.2[x]...192.0.2.1[ims]\n";
        const REJ3: &str = "<ims0|3> parsed CREATE_CHILD_SA response 12 [ N(INVAL_SEL) ]\n";
        const REJ4: &str = "<ims0|4> parsed CREATE_CHILD_SA response 7 [ N(INVAL_SEL) ]\n";
        const OK3: &str = "<ims0|3> parsed CREATE_CHILD_SA response 12 [ SA No TSi TSr ]\n";
        let cases: [(&str, String, Option<&str>); 6] = [
            ("no rejection", format!("{EST3}{OK3}"), None),
            ("rejected in the current session", format!("{EST3}{REJ3}"), Some("ims0[3]")),
            ("only an earlier session rejected", format!("{EST3}{REJ3}{EST4}"), None),
            (
                "another connection's rejection",
                format!("{EST3}<ims1|7> IKE_SA ims1[7] established\n<ims1|7> parsed CREATE_CHILD_SA response 3 [ N(INVAL_SEL) ]\n"),
                None,
            ),
            ("two sessions rejected", format!("{EST3}{REJ3}{EST4}{REJ4}"), Some("ims0[4]")),
            ("a response without INVAL_SEL", format!("{EST3}{OK3}{OK3}"), None),
        ];
        for (name, log, want) in cases {
            assert_eq!(
                child_rekey_invalid_selectors_session(&log, "ims0").as_deref(),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn child_sa_rejection_notify_table() {
        const EST3: &str =
            "<ims0|3> IKE_SA ims0[3] established between 10.0.0.2[x]...192.0.2.1[ims]\n";
        const EST4: &str =
            "<ims0|4> IKE_SA ims0[4] established between 10.0.0.2[x]...192.0.2.1[ims]\n";
        const INVAL: &str = "<ims0|3> parsed CREATE_CHILD_SA response 12 [ N(INVAL_SEL) ]\n";
        const TS: &str = "<ims0|3> parsed CREATE_CHILD_SA response 13 [ N(TS_UNACCEPT) ]\n";
        const OK: &str = "<ims0|3> parsed CREATE_CHILD_SA response 14 [ SA No TSi TSr ]\n";
        const OTHER: &str = "<ims1|7> parsed CREATE_CHILD_SA response 3 [ N(NO_PROP) ]\n";
        let cases: [(&str, String, Option<&str>); 7] = [
            ("INVALID_SELECTORS", format!("{EST3}{INVAL}"), Some("INVAL_SEL")),
            ("the latest refusal wins", format!("{EST3}{INVAL}{TS}"), Some("TS_UNACCEPT")),
            ("a later success clears it", format!("{EST3}{INVAL}{OK}"), None),
            ("only an earlier session refused", format!("{EST3}{INVAL}{EST4}"), None),
            ("another connection's refusal", format!("{EST3}{OTHER}"), None),
            ("no CREATE_CHILD_SA at all", EST3.to_string(), None),
            ("our own request is not a response", format!("{EST3}<ims0|3> generating CREATE_CHILD_SA request 12 [ N(REKEY_SA) SA No TSi TSr ]\n"), None),
        ];
        for (name, log, want) in cases {
            assert_eq!(
                child_sa_rejection_notify(&log, "ims0").as_deref(),
                want,
                "{name}"
            );
        }
    }

    mod engine_wiring_tests {
        use super::*;
        use crate::supervise::runner::MockCommandRunner;

        // MOCK JUSTIFICATION (constitution Principle I): stands in for real
        // charon/swanctl/the swu dialer/a live modem — none available in
        // CI. The wiring under test (which commands get issued, in what
        // order, on what engine method) is real production code.

        fn shared_charon() -> Arc<SharedCharon> {
            Arc::new(SharedCharon::new(
                "/etc/strongswan-shared.conf".to_string(),
                "/etc/swanctl/swanctl.conf".to_string(),
                PathBuf::from("/tmp/charon.log"),
            ))
        }

        fn strongswan_engine() -> StrongswanEngine {
            StrongswanEngine {
                idx: 0,
                conn_name: "ims0".to_string(),
                netns: "ims".to_string(),
                tun_iface: "tun23".to_string(),
                if_id: "23".to_string(),
                shared: shared_charon(),
                max_establish_attempts: None,
                address_family: TunnelAddressFamily::Dual,
                last_inbound: Default::default(),
                last_tun_rx: Default::default(),
                epdg_addr: None,
                last_ike_header: Default::default(),
            }
        }

        const V4_ROUTE_SHOW: &[&str] = &["ip", "route", "show", "default", "dev", "tun23"];
        const V6_ROUTE_SHOW: &[&str] = &["ip", "-6", "route", "show", "default", "dev", "tun23"];

        /// A runner whose `ims` netns has the default route in `present` only.
        fn routes_present(present: &[&[&str]]) -> MockCommandRunner {
            let runner = MockCommandRunner::new();
            for argv in present {
                runner.set_netns_output("ims", argv, "default scope link \n");
            }
            runner
        }

        /// Makes each family's `ip route replace` actually install that
        /// family's route, so a repair's re-check sees the result.
        fn replaces_take_effect(runner: &MockCommandRunner) {
            for (replace, show) in [
                (
                    &["ip", "route", "replace", "default", "dev", "tun23"][..],
                    V4_ROUTE_SHOW,
                ),
                (
                    &["ip", "-6", "route", "replace", "default", "dev", "tun23"][..],
                    V6_ROUTE_SHOW,
                ),
            ] {
                runner.set_netns_output_after("ims", replace, show, "default scope link \n");
            }
        }

        fn route_replaces(runner: &MockCommandRunner) -> Vec<Vec<String>> {
            runner
                .run_in_netns_calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(ns, c)| ns == "ims" && c.contains(&"replace".to_string()))
                .map(|(_, c)| c.clone())
                .collect()
        }

        #[test]
        fn an_ipv6_pcscf_line_repairs_a_lost_ipv6_route_though_ipv4_is_present() {
            // #133, as observed on the Vodafone rig: only the IPv6 route was
            // deleted, the old IPv4-only check called it present, and the line
            // waited out 82s of unreachable strikes and rebuilt the tunnel.
            let runner = routes_present(&[V4_ROUTE_SHOW]);
            replaces_take_effect(&runner);
            assert!(
                strongswan_engine().repair_default_route(&runner, "2001:db8::1"),
                "a repaired route must be reported, or the supervisor counts a strike \
                 and eventually rebuilds the tunnel anyway"
            );
            assert!(route_replaces(&runner)
                .iter()
                .any(|c| c == &["ip", "-6", "route", "replace", "default", "dev", "tun23"]));
        }

        #[test]
        fn a_repair_that_does_not_bring_the_route_back_is_not_reported() {
            // `replace` runs but the route never appears (the mock leaves the
            // listing empty): the supervisor must fall through to counting a
            // strike rather than claim DefaultRouteMissing was fixed.
            let runner = routes_present(&[V4_ROUTE_SHOW]);
            assert!(!strongswan_engine().repair_default_route(&runner, "2001:db8::1"));
            assert!(!route_replaces(&runner).is_empty());
        }

        #[test]
        fn an_ipv6_pcscf_line_ignores_a_missing_ipv4_route() {
            let runner = routes_present(&[V6_ROUTE_SHOW]);
            assert!(!strongswan_engine().repair_default_route(&runner, "2001:db8::1"));
            assert!(route_replaces(&runner).is_empty());
        }

        #[test]
        fn an_ipv4_pcscf_line_ignores_a_missing_ipv6_route() {
            let runner = routes_present(&[V4_ROUTE_SHOW]);
            assert!(!strongswan_engine().repair_default_route(&runner, "10.0.0.1"));
            assert!(route_replaces(&runner).is_empty());
        }

        #[test]
        fn an_ipv4_pcscf_line_repairs_a_lost_ipv4_route() {
            let runner = routes_present(&[V6_ROUTE_SHOW]);
            replaces_take_effect(&runner);
            assert!(strongswan_engine().repair_default_route(&runner, "10.0.0.1"));
            assert!(route_replaces(&runner)
                .iter()
                .any(|c| c == &["ip", "route", "replace", "default", "dev", "tun23"]));
        }

        #[test]
        fn an_unparseable_pcscf_requires_both_routes() {
            let runner = routes_present(&[V4_ROUTE_SHOW]);
            replaces_take_effect(&runner);
            assert!(strongswan_engine().repair_default_route(&runner, "pcscf.example"));
            assert!(!route_replaces(&runner).is_empty());

            let runner = routes_present(&[V4_ROUTE_SHOW, V6_ROUTE_SHOW]);
            assert!(!strongswan_engine().repair_default_route(&runner, "pcscf.example"));
            assert!(route_replaces(&runner).is_empty());
        }

        #[test]
        fn the_daemon_is_spawned_once_no_matter_how_many_lines_share_it() {
            // This is the fix itself. N lines used to mean N charon
            // processes, all wildcard-bound to UDP 500/4500 in one netns,
            // where only one of them ever received a reply.
            let runner = MockCommandRunner::new();
            let shared = shared_charon();

            let first = shared.ensure_started(&runner);
            let second = shared.ensure_started(&runner);

            assert!(
                first.is_some(),
                "the first caller spawns it and is handed the handle to register"
            );
            assert!(
                second.is_none(),
                "a later line must find it already running, not spawn a second"
            );
            let charon_spawns = runner
                .spawn_specs
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.argv.iter().any(|a| a == "/usr/libexec/ipsec/charon"))
                .count();
            assert_eq!(charon_spawns, 1);
        }

        #[test]
        fn restart_process_leaves_a_live_shared_daemon_alone() {
            // The property that makes sharing safe: one line's vici trouble
            // must not restart the daemon out from under every other line.
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            engine.shared.ensure_started(&runner);
            let id = engine.shared.current_handle().unwrap().id();

            engine.restart_process(&runner);

            assert_eq!(
                engine.shared.current_handle().unwrap().id(),
                id,
                "a live shared daemon must survive one line's recovery"
            );
            let specs = runner.spawn_specs.lock().unwrap();
            assert!(
                specs
                    .iter()
                    .any(|s| s.argv.contains(&"--initiate".to_string())
                        && s.argv.contains(&"ims0".to_string())),
                "recovery must still re-initiate, scoped to this line's connection"
            );
        }

        /// specs/093 FR-008/T011: a stuck rekey on line 0, through the real
        /// engine and the real tick, rebuilds line 0's connection and nothing
        /// else on the shared daemon.
        #[test]
        fn a_stuck_rekey_rebuilds_only_its_own_connection() {
            use crate::supervise::line_supervisor::{
                tick_steady_state, DegradeReason, SteadyOutcome, SteadyTracking,
                STEADY_STATE_POLL_INTERVAL,
            };
            use std::os::unix::process::ExitStatusExt;
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            assert!(engine.shared.ensure_started(&runner).is_some());
            runner.set_netns_output("ims", &["ip", "-s", "link", "show", "tun23"], "5: tun23\n");
            runner.set_run_output(
                "timeout 10 env STRONGSWAN_CONF=/etc/strongswan-shared.conf swanctl --list-sas --ike ims0",
                std::process::Output {
                    status: std::process::ExitStatus::from_raw(0),
                    stdout: b"ims0: #3, ESTABLISHED, IKEv2, aa_i* bb_r\n  ims0: #12, reqid 1, INSTALLED, TUNNEL-in-UDP, ESP:AES_CBC-128/HMAC_SHA1_96\n    installed 3670s ago, rekeying in -70s, expires in 290s\n".to_vec(),
                    stderr: Vec::new(),
                },
            );
            runner.set_file(
                "/tmp/charon.log",
                "<ims0|3> IKE_SA ims0[3] established between 192.0.2.10[x]...203.0.113.1[ims]\n<ims0|3> parsed CREATE_CHILD_SA response 9 [ N(INVAL_SEL) ]\n",
            );
            let mut tracking = SteadyTracking::default();
            assert_eq!(
                tick_steady_state(
                    &engine,
                    &runner,
                    "",
                    &mut tracking,
                    STEADY_STATE_POLL_INTERVAL
                ),
                SteadyOutcome::Recovered {
                    reason: DegradeReason::ChildSaRekeyStuck,
                    ike_sa_loss_cause: None,
                }
            );
            let ran = runner.run_calls.lock().unwrap().clone();
            let terminates: Vec<_> = ran
                .iter()
                .filter(|a| a.iter().any(|x| x == "--terminate"))
                .collect();
            assert!(!terminates.is_empty(), "{ran:?}");
            for t in &terminates {
                assert!(
                    t.windows(2).any(|w| w[0] == "--ike" && w[1] == "ims0"),
                    "{t:?}"
                );
            }
            let spawned: Vec<Vec<String>> = runner
                .spawn_specs
                .lock()
                .unwrap()
                .iter()
                .map(|s| s.argv.clone())
                .collect();
            assert!(
                spawned
                    .iter()
                    .any(|a| a.windows(2).any(|w| w[0] == "--child" && w[1] == "ims0")),
                "{spawned:?}"
            );
            assert!(
                !ran.iter()
                    .chain(spawned.iter())
                    .flatten()
                    .any(|a| a == "ims1"),
                "another line's connection was touched"
            );
        }

        #[test]
        fn steady_state_health_distinguishes_a_bare_ike_sa_from_one_with_a_child() {
            use std::os::unix::process::ExitStatusExt;
            let list_key =
                "timeout 10 env STRONGSWAN_CONF=/etc/strongswan-shared.conf swanctl --list-sas --ike ims0";
            let seed = |stdout: &str| {
                let runner = MockCommandRunner::new();
                runner.set_run_output(
                    list_key,
                    std::process::Output {
                        status: std::process::ExitStatus::from_raw(0),
                        stdout: stdout.as_bytes().to_vec(),
                        stderr: Vec::new(),
                    },
                );
                runner
            };
            let engine = strongswan_engine();
            let bare = seed(
                "ims0: #1, ESTABLISHED, IKEv2, aa_i* bb_r\n  local  'x' @ 198.51.100.10[4500]\n",
            );
            assert_eq!(
                engine.steady_state_health(&bare),
                SteadyStateHealth::ChildSaMissing
            );
            let healthy = seed("ims0: #1, ESTABLISHED, IKEv2, aa_i* bb_r\n  ims0: #4, reqid 1, INSTALLED, TUNNEL-in-UDP, ESP:AES_CBC-128/HMAC_SHA1_96\n    installed 1109s ago, rekeying in 2167s, expires in 2852s\n");
            assert_eq!(
                engine.steady_state_health(&healthy),
                SteadyStateHealth::Ok(Some(ChildSaLifetime {
                    unique_id: 4,
                    expires_in: Some(2852),
                }))
            );
        }

        #[test]
        fn a_healthy_check_snapshots_the_tunnel_counter_with_the_esp() {
            use std::os::unix::process::ExitStatusExt;
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            assert!(engine.shared.ensure_started(&runner).is_some());
            runner.set_netns_output(
                "ims",
                &["ip", "-s", "link", "show", "tun23"],
                "5: tun23\n    RX:  bytes packets errors dropped  missed   mcast\n         900      77      0       0       0       0\n",
            );
            runner.set_run_output(
                "timeout 10 env STRONGSWAN_CONF=/etc/strongswan-shared.conf swanctl --list-sas --ike ims0",
                std::process::Output {
                    status: std::process::ExitStatus::from_raw(0),
                    stdout: format!("{}{}", super::ike_header("ims0"), super::child_with_in("ims0", 1, "INSTALLED", " 900 bytes,     9 packets,     4s ago")).into_bytes(),
                    stderr: Vec::new(),
                },
            );
            assert!(matches!(
                engine.steady_state_health(&runner),
                SteadyStateHealth::Ok(_)
            ));
            assert_eq!(engine.tun_rx_packets(), Some(77));
            assert_eq!(
                engine.inbound_esp().and_then(|e| e.tun_rx_packets),
                Some(77)
            );
        }
        #[test]
        fn steady_state_health_reports_a_missing_ike_sa_distinctly() {
            use std::os::unix::process::ExitStatusExt;
            let list_key =
                "timeout 10 env STRONGSWAN_CONF=/etc/strongswan-shared.conf swanctl --list-sas --ike ims0";
            let seed = |status: i32, stdout: &str| {
                let runner = MockCommandRunner::new();
                runner.set_run_output(
                    list_key,
                    std::process::Output {
                        status: std::process::ExitStatus::from_raw(status),
                        stdout: stdout.as_bytes().to_vec(),
                        stderr: Vec::new(),
                    },
                );
                runner
            };
            let engine = strongswan_engine();
            assert_eq!(
                engine.steady_state_health(&seed(0, "")),
                SteadyStateHealth::IkeSaMissing
            );
            // A half-open IKE_SA is charon's own negotiation in flight, so it
            // is reported as that and not as a lost CHILD_SA (specs/100; it
            // used to read as `ChildSaMissing`, 087 R1).
            let connecting = seed(0, "ims0: #2, CONNECTING, IKEv2, aa_i* bb_r\n");
            assert_eq!(
                engine.steady_state_health(&connecting),
                SteadyStateHealth::IkeSaConnecting { ike_id: 2 }
            );
            assert_eq!(
                engine.steady_state_health(&seed(256, "")),
                SteadyStateHealth::ViciBroken
            );
        }

        #[test]
        fn ike_sa_loss_cause_reads_this_lines_charon_log() {
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            runner
                .write_file(
                    &engine.shared.charon_log,
                    "09[IKE] <ims0|3> IKE_SA ims0[3] established between a...b\n11[IKE] <ims0|3> received DELETE for IKE_SA ims0[3]\n",
                )
                .unwrap();
            assert_eq!(
                TunnelEngine::ike_sa_loss_cause(&engine, &runner),
                IkeSaLossCause::CarrierDelete
            );
        }

        #[test]
        fn a_charon_restart_invalidates_the_log_mark_even_if_the_new_log_outgrows_it() {
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            let old = "[CFG] <ims0|1> received P-CSCF server IP 192.0.2.10\n[IKE] <ims0|1> CHILD_SA ims0{3} established x\n";
            runner.set_file(&engine.shared.charon_log, old);
            let mark = engine.log_mark(&runner);
            // charon restarts: the log is truncated, then grows past the mark.
            engine.shared.log_generation.fetch_add(1, Ordering::SeqCst);
            let new = "[CFG] <ims0|2> received P-CSCF server IP 192.0.2.11\n[IKE] <ims0|2> CHILD_SA ims0{4} established y\n[IKE] <ims0|2> padding padding padding padding\n";
            assert!(new.len() > old.len());
            runner.set_file(&engine.shared.charon_log, new);
            assert_eq!(
                engine.fresh_pcscf_since(&runner, mark),
                Some("192.0.2.11".to_string())
            );
        }

        #[test]
        fn terminate_is_scoped_to_this_lines_connection() {
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            engine.terminate(&runner);
            let calls = runner.run_calls.lock().unwrap();
            assert!(
                calls
                    .iter()
                    .any(|c| c.contains(&"--terminate".to_string())
                        && c.contains(&"ims0".to_string()))
            );
            assert!(
                !calls.iter().any(|c| c.contains(&"ims".to_string())),
                "the bare `ims` name would terminate every line's IKE_SA at once"
            );
        }

        const ENV: &str = "STRONGSWAN_CONF=/etc/strongswan-shared.conf";
        const LIST_KEY: &str = "timeout 10 env STRONGSWAN_CONF=/etc/strongswan-shared.conf swanctl --list-sas --ike ims0";

        fn argv(parts: &[&str]) -> Vec<String> {
            parts.iter().map(|p| p.to_string()).collect()
        }

        fn graceful_argv() -> Vec<String> {
            argv(&[
                "timeout",
                "8",
                "env",
                ENV,
                "swanctl",
                "--terminate",
                "--ike",
                "ims0",
                "--timeout",
                "5",
            ])
        }

        fn forced_argv() -> Vec<String> {
            argv(&[
                "timeout",
                "10",
                "env",
                ENV,
                "swanctl",
                "--terminate",
                "--ike",
                "ims0",
                "--force",
            ])
        }

        fn seed_list(runner: &MockCommandRunner, status: i32, stdout: &str) {
            use std::os::unix::process::ExitStatusExt;
            runner.set_run_output(
                LIST_KEY,
                std::process::Output {
                    status: std::process::ExitStatus::from_raw(status),
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: Vec::new(),
                },
            );
        }

        fn assert_terminate_invariants(runner: &MockCommandRunner) {
            let calls = runner.run_calls.lock().unwrap();
            assert!(
                !calls
                    .iter()
                    .any(|c| c.contains(&"--force".to_string())
                        && c.contains(&"--timeout".to_string())),
                "--timeout cancels --force's no-wait behaviour"
            );
            assert!(
                runner.sleeps.lock().unwrap().is_empty(),
                "teardown waits on the ePDG's acknowledgement, never a fixed delay"
            );
        }

        #[test]
        fn an_acknowledged_graceful_terminate_is_not_forced() {
            let runner = MockCommandRunner::new();
            seed_list(&runner, 0, "");
            strongswan_engine().terminate(&runner);
            let calls = runner.run_calls.lock().unwrap();
            assert_eq!(calls[0], graceful_argv());
            assert!(!calls.contains(&forced_argv()));
            drop(calls);
            assert_terminate_invariants(&runner);
        }

        #[test]
        fn an_unacknowledged_graceful_terminate_is_forced() {
            let runner = MockCommandRunner::new();
            seed_list(&runner, 0, "ims0: #1, ESTABLISHED, IKEv2, aa_i* bb_r\n");
            strongswan_engine().terminate(&runner);
            let calls = runner.run_calls.lock().unwrap();
            assert_eq!(calls[0], graceful_argv());
            assert_eq!(calls.iter().filter(|c| **c == forced_argv()).count(), 1);
            drop(calls);
            assert_terminate_invariants(&runner);
        }

        #[test]
        fn a_failed_presence_query_is_forced() {
            let runner = MockCommandRunner::new();
            seed_list(&runner, 256, "");
            strongswan_engine().terminate(&runner);
            let calls = runner.run_calls.lock().unwrap();
            assert_eq!(calls.iter().filter(|c| **c == forced_argv()).count(), 1);
            drop(calls);
            assert_terminate_invariants(&runner);
        }

        #[test]
        fn restart_process_clears_the_log_removes_the_pidfile_and_respawns_a_dead_charon() {
            let runner = MockCommandRunner::new();
            runner.set_born_dead_if_argv_contains("charon");
            let engine = strongswan_engine();
            engine.shared.ensure_started(&runner);
            let old_id = engine.shared.current_handle().unwrap().id();

            engine.restart_process(&runner);

            let calls = runner.run_calls.lock().unwrap();
            assert!(calls
                .iter()
                .any(|c| c == &["rm", "-f", "/var/run/charon.pid"]));
            let new = engine
                .shared
                .current_handle()
                .expect("a replacement must have been spawned");
            assert_ne!(new.id(), old_id, "the handle must be a new child");
        }

        #[test]
        fn restart_process_reaps_the_old_charon_before_spawning_the_new_one() {
            // Regression test (Greptile P1): signaling the old handle
            // without ever waiting on it left a permanently un-reaped entry
            // in RealCommandRunner's tracked-children table (a leak across
            // repeated recoveries) and let a slow-to-die old charon overlap
            // with its replacement. `wait()` must be called on the old
            // handle before restart_process returns.
            let runner = MockCommandRunner::new();
            runner.set_born_dead_if_argv_contains("charon");
            let engine = strongswan_engine();
            engine.shared.ensure_started(&runner);
            let old_id = engine.shared.current_handle().unwrap().id();

            engine.restart_process(&runner);

            // `reap` = SIGTERM, then poll is_alive until it is really gone
            // (escalating to SIGKILL). Asserting on the signals rather than
            // on a wait() call, because reap deliberately no longer waits:
            // waiting untracked the child up front, which is what made a
            // concurrently-held handle silently unusable.
            let sigs = runner.signals_for_id(old_id);
            assert!(
                sigs.contains(&crate::supervise::runner::Signal::Term),
                "the replaced child must be terminated, got {sigs:?}"
            );
        }

        #[test]
        fn restart_process_spawns_charon_through_env_not_as_argv0() {
            // Regression test (Greptile P1): a real RealCommandRunner passes
            // argv[0] straight to `Command::new`, so the spawned command's
            // first argument MUST be the literal program to exec ("env"),
            // with the `STRONGSWAN_CONF=...` assignment as its own argument
            // — not the environment-variable string itself as argv[0],
            // which would fail with ENOENT against a real runner (a mock
            // can't catch this since it never actually execs anything).
            let runner = MockCommandRunner::new();
            runner.set_born_dead_if_argv_contains("charon");
            let engine = strongswan_engine();
            engine.restart_process(&runner);
            let specs = runner.spawn_specs.lock().unwrap();
            let charon_spec = specs
                .iter()
                .find(|s| s.argv.iter().any(|a| a == "/usr/libexec/ipsec/charon"))
                .expect("a charon spawn must have been issued");
            assert_eq!(charon_spec.argv[0], "env");
            assert!(charon_spec.argv[1].starts_with("STRONGSWAN_CONF="));
        }

        #[test]
        fn restart_process_backgrounds_initiate_rather_than_blocking_on_it() {
            // Regression test (Greptile P2): the bash original runs
            // `--initiate` with a trailing `&` specifically so a slow IKE
            // negotiation can't block the establish/steady-state poll loop;
            // this must go through `spawn` (fire-and-forget), never a
            // blocking `run`.
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            engine.restart_process(&runner);

            let run_calls = runner.run_calls.lock().unwrap();
            assert!(
                !run_calls
                    .iter()
                    .any(|c| c.contains(&"--initiate".to_string())),
                "--initiate must never be issued as a blocking `run` call"
            );
            let spawn_specs = runner.spawn_specs.lock().unwrap();
            assert!(spawn_specs
                .iter()
                .any(|s| s.argv.contains(&"--initiate".to_string())));
        }

        #[test]
        fn restart_process_sleeps_before_load_all_to_let_the_vici_socket_come_up() {
            // Regression test for a real bug found live (T049/FR-009 audit):
            // the bash original has `sleep 2 # let the vici socket come up
            // before swanctl talks to it` at every one of its charon-respawn
            // sites, including this one — but this port's restart_charon
            // had it only at the *initial* startup call site (orchestrate.rs),
            // not here. Missing it here meant `--load-all` could run before
            // the freshly respawned charon's vici socket was listening,
            // silently failing to load anything, so the follow-up
            // `--initiate` failed with "CHILD_SA config 'ims' not found" —
            // and steady-state's ChildSaMissing branch only ever
            // re-initiates, never reloads, so the tunnel never recovered on
            // its own. Observed live: killing charon mid-session left the
            // line permanently stuck, while the healthcheck kept passing on
            // the stale pre-kill tun23-0 address.
            let runner = MockCommandRunner::new();
            runner.set_born_dead_if_argv_contains("charon");
            let engine = strongswan_engine();
            engine.restart_process(&runner);
            assert!(
                runner
                    .sleeps
                    .lock()
                    .unwrap()
                    .contains(&std::time::Duration::from_secs(2)),
                "a respawn must sleep 2s before --load-all, matching \
                 the bash original at every one of its charon-respawn sites"
            );
        }

        #[test]
        fn reinitiate_also_backgrounds_initiate() {
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            engine.reinitiate(&runner);
            assert!(runner.run_calls.lock().unwrap().is_empty());
            let specs = runner.spawn_specs.lock().unwrap();
            assert!(specs
                .iter()
                .any(|s| s.argv.contains(&"--initiate".to_string())));
        }

        #[test]
        fn tun_iface_missing_is_reported_as_tun_vanished() {
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            // No output seeded for the `ip link show` probe -> MockCommandRunner
            // defaults to a successful (status 0) empty Output, so seed a
            // failing one explicitly for this netns-scoped call. Key format
            // ("netns:<netns>:<argv joined with spaces>") matches
            // MockCommandRunner::run_in_netns's own lookup key.
            use std::os::unix::process::ExitStatusExt;
            runner.set_run_output(
                "netns:ims:ip -s link show tun23",
                std::process::Output {
                    status: std::process::ExitStatus::from_raw(256),
                    stdout: vec![],
                    stderr: vec![],
                },
            );
            assert_eq!(
                engine.steady_state_health(&runner),
                SteadyStateHealth::TunVanished
            );
        }

        fn swu_engine() -> SwuEngine {
            SwuEngine {
                idx: 0,
                modem: "/dev/ttyUSB2".to_string(),
                apn: "ims".to_string(),
                mcc: "404".to_string(),
                mnc: "10".to_string(),
                netns: "ims".to_string(),
                src_addr: None,
                log_file: PathBuf::from("/tmp/swu-0.log"),
                dialer_handle: RefCell::new(None),
            }
        }

        #[test]
        fn swu_terminate_and_reinitiate_are_genuinely_no_ops() {
            let runner = MockCommandRunner::new();
            let engine = swu_engine();
            engine.terminate(&runner);
            engine.reinitiate(&runner);
            assert!(runner.run_calls.lock().unwrap().is_empty());
        }

        #[test]
        fn swu_restart_process_respawns_the_dialer_with_the_same_line_params() {
            let runner = MockCommandRunner::new();
            let engine = swu_engine();
            engine.restart_process(&runner);
            let specs = runner.spawn_specs.lock().unwrap();
            assert_eq!(specs.len(), 1);
            assert!(specs[0].argv.contains(&"-m".to_string()));
            assert!(specs[0].argv.contains(&"/dev/ttyUSB2".to_string()));
        }

        #[test]
        fn swu_restart_process_reaps_the_old_dialer_before_spawning_the_new_one() {
            // Same Greptile P1 fix as StrongswanEngine::restart_charon: the
            // old dialer handle must actually be waited on, not just
            // signaled, or it leaks in RealCommandRunner's tracked-children
            // table and can overlap with its replacement.
            let runner = MockCommandRunner::new();
            let engine = swu_engine();
            let old = runner.spawn(ChildSpec::new(["swu-dialer"])).unwrap();
            let old_id = old.id();
            *engine.dialer_handle.borrow_mut() = Some(std::sync::Arc::new(old));

            engine.restart_process(&runner);

            let sigs = runner.signals_for_id(old_id);
            assert!(
                sigs.contains(&crate::supervise::runner::Signal::Term),
                "the replaced child must be terminated, got {sigs:?}"
            );
        }

        /// One closed mixed-family batch (specs/092 T009).
        const MIXED_LOG: &str = "[CFG] <ims0|1> received P-CSCF server IP 2402:8100::10\n\
             [CFG] <ims0|1> received P-CSCF server IP 10.0.1.10\n\
             [CFG] <ims0|1> received P-CSCF server IP 2402:8100::11\n\
             [CFG] <ims0|1> received P-CSCF server IP 10.0.0.10\n\
             [IKE] <ims0|1> CHILD_SA ims0{1} established with SPIs x\n";

        fn engine_with(family: TunnelAddressFamily) -> StrongswanEngine {
            StrongswanEngine {
                address_family: family,
                last_inbound: Default::default(),
                last_tun_rx: Default::default(),
                ..strongswan_engine()
            }
        }

        fn strings(v: &[&str]) -> Vec<String> {
            v.iter().map(|s| s.to_string()).collect()
        }

        #[test]
        fn pcscf_selection_and_pool_follow_the_address_family() {
            for (family, pick, pool, ignored) in [
                (
                    TunnelAddressFamily::Ipv4,
                    "10.0.0.10",
                    ["10.0.1.10", "10.0.0.10"],
                    ["2402:8100::10", "2402:8100::11"],
                ),
                (
                    TunnelAddressFamily::Ipv6,
                    "2402:8100::10",
                    ["2402:8100::10", "2402:8100::11"],
                    ["10.0.1.10", "10.0.0.10"],
                ),
            ] {
                let runner = MockCommandRunner::new();
                let engine = engine_with(family);
                let mark = engine.log_mark(&runner);
                runner.set_file(&engine.shared.charon_log, MIXED_LOG);
                assert_eq!(
                    TunnelEngine::latest_pcscf(&engine, &runner).as_deref(),
                    Some(pick),
                    "{family}"
                );
                assert_eq!(
                    TunnelEngine::latest_pcscf_pool(&engine, &runner),
                    strings(&pool),
                    "{family}"
                );
                assert_eq!(
                    engine.ignored_pcscfs(&runner),
                    strings(&ignored),
                    "{family}"
                );
                assert_eq!(
                    engine.fresh_pcscf_since(&runner, mark).as_deref(),
                    Some(pick),
                    "{family}"
                );
                assert_eq!(
                    engine.fresh_pcscf_pool_since(&runner, mark),
                    Some(strings(&pool)),
                    "{family}"
                );
            }

            // An ipv6 line on an ePDG advertising only IPv4 has no P-CSCF.
            let runner = MockCommandRunner::new();
            let engine = engine_with(TunnelAddressFamily::Ipv6);
            let mark = engine.log_mark(&runner);
            runner.set_file(
                &engine.shared.charon_log,
                "[CFG] <ims0|1> received P-CSCF server IP 10.0.0.10\n\
                 [IKE] <ims0|1> CHILD_SA ims0{1} established x\n",
            );
            assert_eq!(TunnelEngine::latest_pcscf(&engine, &runner), None);
            assert!(TunnelEngine::latest_pcscf_pool(&engine, &runner).is_empty());
            assert_eq!(engine.fresh_pcscf_since(&runner, mark), None);
            assert_eq!(engine.fresh_pcscf_pool_since(&runner, mark), None);
        }

        #[test]
        fn dual_engine_selection_and_pool_match_the_pure_extractors() {
            let runner = MockCommandRunner::new();
            let engine = engine_with(TunnelAddressFamily::Dual);
            let mark = engine.log_mark(&runner);
            runner.set_file(&engine.shared.charon_log, MIXED_LOG);
            assert_eq!(
                TunnelEngine::latest_pcscf(&engine, &runner),
                extract_latest_pcscf(MIXED_LOG, "ims0")
            );
            assert_eq!(
                TunnelEngine::latest_pcscf_pool(&engine, &runner),
                extract_latest_pcscf_pool(MIXED_LOG, "ims0")
            );
            assert!(engine.ignored_pcscfs(&runner).is_empty());
            assert_eq!(
                engine.fresh_pcscf_since(&runner, mark),
                extract_pcscf_after(MIXED_LOG, "ims0", 0)
            );
            assert_eq!(
                engine.fresh_pcscf_pool_since(&runner, mark),
                extract_pcscf_pool_after(MIXED_LOG, "ims0", 0)
            );
        }

        #[test]
        fn the_engine_reports_the_rejected_rekeys_session() {
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            assert_eq!(engine.rekey_invalid_selectors_session(&runner), None);
            runner.set_file(
                &engine.shared.charon_log,
                "<ims0|5> IKE_SA ims0[5] established between a...b\n\
                 <ims0|5> parsed CREATE_CHILD_SA response 9 [ N(INVAL_SEL) ]\n",
            );
            assert_eq!(
                engine.rekey_invalid_selectors_session(&runner).as_deref(),
                Some("ims0[5]")
            );
        }
    }
}

/// Parsers and engine methods behind the underlay guard (specs/100), over the
/// text charon and iproute2 print. Log lines are the shape the pinned 5.9.3
/// fork writes at `filelog default = 1` with `time_format` and `ike_name` set
/// (research R4), with synthetic addresses and identities.
#[cfg(test)]
mod underlay_tests {
    use super::*;
    use crate::supervise::line_supervisor::TunnelEngine;
    use crate::supervise::runner::MockCommandRunner;
    use std::os::unix::process::ExitStatusExt;

    fn engine() -> StrongswanEngine {
        StrongswanEngine {
            idx: 0,
            conn_name: "ims0".to_string(),
            netns: "ims".to_string(),
            tun_iface: "tun23".to_string(),
            if_id: "23".to_string(),
            shared: Arc::new(SharedCharon::new(
                "/etc/strongswan-shared.conf".to_string(),
                "/etc/swanctl/swanctl.conf".to_string(),
                PathBuf::from("/tmp/charon.log"),
            )),
            max_establish_attempts: None,
            address_family: TunnelAddressFamily::Dual,
            last_inbound: Default::default(),
            last_tun_rx: Default::default(),
            epdg_addr: Some("203.0.113.9".to_string()),
            last_ike_header: Default::default(),
        }
    }

    fn output(status: i32, stdout: &str) -> std::process::Output {
        std::process::Output {
            status: std::process::ExitStatus::from_raw(status << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    // ---- T004: ike_sa_header -------------------------------------------

    #[test]
    fn the_ike_sa_header_gives_its_number_and_state() {
        let established = "ims0: #1, ESTABLISHED, IKEv2, aa_i* bb_r\n  local  'x' @ 198.51.100.10[4500]\n  ims0: #4, reqid 1, INSTALLED, TUNNEL-in-UDP, ESP:AES_CBC-128/HMAC_SHA1_96\n";
        assert_eq!(
            ike_sa_header(established, "ims0"),
            Some((1, IkeHeaderState::Established))
        );
        assert_eq!(
            ike_sa_header("ims0: #17, CONNECTING, IKEv2, aa_i* bb_r\n", "ims0"),
            Some((17, IkeHeaderState::Connecting))
        );
        assert_eq!(
            ike_sa_header("ims0: #2, REKEYING, IKEv2, aa_i* bb_r\n", "ims0"),
            Some((2, IkeHeaderState::Other("REKEYING".to_string())))
        );
    }

    #[test]
    fn the_ike_sa_header_is_matched_by_whole_connection_name_and_column() {
        let sas = "ims10: #3, CONNECTING, IKEv2, aa_i* bb_r\nims1: #4, ESTABLISHED, IKEv2, cc_i* dd_r\n  ims0: #9, reqid 1, INSTALLED, TUNNEL-in-UDP, ESP:AES_CBC-128/HMAC_SHA1_96\n";
        assert_eq!(
            ike_sa_header(sas, "ims1"),
            Some((4, IkeHeaderState::Established))
        );
        assert_eq!(
            ike_sa_header(sas, "ims10"),
            Some((3, IkeHeaderState::Connecting))
        );
        // An indented CHILD_SA line of the same name is not a header.
        assert_eq!(ike_sa_header(sas, "ims0"), None);
        assert_eq!(ike_sa_header("", "ims0"), None);
    }

    // ---- T005: ike_activity ----------------------------------------------

    /// An IKE_SA_INIT retry cycle, as charon logs it while the ePDG is
    /// unreachable. Timestamps are in the container's local time.
    const INIT_RETRY: &str = "\
2026-10-20 22:30:00 08[IKE] <ims0|5> initiating IKE_SA ims0[5] to 203.0.113.9
2026-10-20 22:30:00 08[ENC] <ims0|5> generating IKE_SA_INIT request 0 [ SA KE No N(NATD_S_IP) N(NATD_D_IP) N(FRAG_SUP) N(HASH_ALG) N(REDIR_SUP) ]
2026-10-20 22:30:04 09[IKE] <ims0|5> retransmit 1 of request with message ID 0
2026-10-20 22:30:11 10[IKE] <ims0|5> retransmit 2 of request with message ID 0
2026-10-20 22:30:24 11[IKE] <ims0|5> retransmit 3 of request with message ID 0
";

    fn activity(lines: usize, last: &str) -> Option<IkeActivity> {
        Some(IkeActivity {
            lines,
            last: last.to_string(),
        })
    }

    #[test]
    fn a_retry_cycle_is_fingerprinted_by_its_ike_lines_and_the_last_of_them() {
        // The `initiating IKE_SA` line is not traffic; the other four are.
        assert_eq!(
            ike_activity(INIT_RETRY, "ims0"),
            activity(
                4,
                "2026-10-20 22:30:24 11[IKE] <ims0|5> retransmit 3 of request with message ID 0"
            )
        );
    }

    #[test]
    fn every_further_ike_line_changes_the_fingerprint() {
        let before = ike_activity(INIT_RETRY, "ims0");
        let more = format!(
            "{INIT_RETRY}2026-10-20 22:30:47 12[IKE] <ims0|5> retransmit 4 of request with message ID 0\n"
        );
        assert_ne!(ike_activity(&more, "ims0"), before);
        // The same log read twice is the same fingerprint.
        assert_eq!(ike_activity(INIT_RETRY, "ims0"), before);
    }

    #[test]
    fn a_dpd_exchange_counts_as_activity() {
        let log = "\
2026-10-20 03:10:00 08[ENC] <ims0|1> generating INFORMATIONAL request 6 [ ]
2026-10-20 03:10:00 07[ENC] <ims0|1> parsed INFORMATIONAL response 6 [ ]
2026-10-20 03:12:30 10[ENC] <ims0|1> parsed INFORMATIONAL request 13 [ ]
2026-10-20 03:12:30 10[ENC] <ims0|1> generating INFORMATIONAL response 13 [ ]
";
        let a = ike_activity(log, "ims0").unwrap();
        assert_eq!(a.lines, 4);
        assert!(a.last.contains("generating INFORMATIONAL response 13"));
    }

    #[test]
    fn other_lines_and_other_log_chatter_are_not_this_connections_activity() {
        let log = "\
2026-10-20 03:00:00 08[ENC] <ims0|5> generating IKE_SA_INIT request 0 [ SA KE No ]
2026-10-20 03:05:00 08[ENC] <ims1|2> generating IKE_SA_INIT request 0 [ SA KE No ]
2026-10-20 03:06:00 08[CFG] <ims0|5> peer not responding, trying again (2/0)
2026-10-20 03:07:00 08[IKE] <ims0|5> received P-CSCF server IP 192.0.2.10
2026-10-20 03:08:00 08[IKE] <ims0|5> sending keep alive to 203.0.113.9[4500]
";
        assert_eq!(ike_activity(log, "ims0").unwrap().lines, 1);
        assert!(ike_activity(log, "ims1").unwrap().last.contains("<ims1|2>"));
        assert_eq!(ike_activity(log, "ims2"), None);
        assert_eq!(ike_activity("", "ims0"), None);
    }

    #[test]
    fn the_fingerprint_does_not_depend_on_the_timestamps_zone() {
        // Same lines, stamped in IST and in UTC: only the text differs, and the
        // count is what it is. Nothing here parses a time.
        let utc = "2026-10-20 16:45:00 08[ENC] <ims0|5> generating IKE_SA_INIT request 0 [ SA ]\n";
        let ist = "2026-10-20 22:15:00 08[ENC] <ims0|5> generating IKE_SA_INIT request 0 [ SA ]\n";
        assert_eq!(ike_activity(utc, "ims0").unwrap().lines, 1);
        assert_eq!(ike_activity(ist, "ims0").unwrap().lines, 1);
        // And a line with no timestamp at all is still activity.
        let bare = "08[ENC] <ims0|5> generating IKE_SA_INIT request 0 [ SA ]\n";
        assert_eq!(ike_activity(bare, "ims0").unwrap().lines, 1);
    }

    // ---- T006: ike_evidence --------------------------------------------

    #[test]
    fn retransmits_with_nothing_parsed_are_unanswered() {
        let e = ike_evidence(INIT_RETRY, "ims0");
        assert!(!e.parsed_from_peer);
        assert_eq!(e.unanswered_retransmits, 3);
    }

    #[test]
    fn a_message_generated_and_not_yet_retransmitted_is_neither_life_nor_silence() {
        let log = "2026-10-20 03:00:00 08[ENC] <ims0|1> generating INFORMATIONAL request 6 [ ]\n";
        assert_eq!(ike_evidence(log, "ims0"), IkeEvidence::default());
    }

    #[test]
    fn a_parsed_response_clears_the_retransmit_count() {
        let log = "\
2026-10-20 03:00:00 08[ENC] <ims0|1> generating INFORMATIONAL request 6 [ ]
2026-10-20 03:00:04 09[IKE] <ims0|1> retransmit 1 of request with message ID 6
2026-10-20 03:00:11 10[IKE] <ims0|1> retransmit 2 of request with message ID 6
2026-10-20 03:00:12 07[ENC] <ims0|1> parsed INFORMATIONAL response 6 [ ]
";
        assert_eq!(
            ike_evidence(log, "ims0"),
            IkeEvidence {
                parsed_from_peer: true,
                unanswered_retransmits: 0
            }
        );
    }

    #[test]
    fn an_epdg_initiated_request_is_life() {
        let log = "2026-10-20 03:12:30 10[ENC] <ims0|1> parsed INFORMATIONAL request 13 [ ]\n";
        assert!(ike_evidence(log, "ims0").parsed_from_peer);
    }

    #[test]
    fn silence_after_life_counts_only_the_retransmits_since() {
        let log = "\
2026-10-20 03:00:00 07[ENC] <ims0|1> parsed INFORMATIONAL response 5 [ ]
2026-10-20 03:00:30 08[ENC] <ims0|1> generating INFORMATIONAL request 6 [ ]
2026-10-20 03:00:34 09[IKE] <ims0|1> retransmit 1 of request with message ID 6
2026-10-20 03:00:41 10[IKE] <ims0|1> retransmit 2 of request with message ID 6
";
        let e = ike_evidence(log, "ims0");
        assert!(e.parsed_from_peer);
        assert_eq!(e.unanswered_retransmits, 2);
    }

    #[test]
    fn another_connections_traffic_is_not_this_connections_evidence() {
        let log = "\
2026-10-20 03:00:04 09[IKE] <ims0|1> retransmit 1 of request with message ID 6
2026-10-20 03:00:05 07[ENC] <ims1|2> parsed INFORMATIONAL response 6 [ ]
";
        let e = ike_evidence(log, "ims0");
        assert!(!e.parsed_from_peer);
        assert_eq!(e.unanswered_retransmits, 1);
    }

    #[test]
    fn evidence_since_a_mark_reads_only_the_log_after_it() {
        let runner = MockCommandRunner::new();
        let engine = engine();
        let before =
            "2026-10-20 03:00:04 09[IKE] <ims0|1> retransmit 1 of request with message ID 6\n";
        runner.set_file(&engine.shared.charon_log, before);
        let mark = TunnelEngine::log_mark(&engine, &runner);
        // Nothing after the mark: the old retransmit does not count.
        assert_eq!(
            engine.ike_evidence_since(&runner, mark),
            IkeEvidence::default()
        );
        let after = format!(
            "{before}2026-10-20 03:00:30 07[ENC] <ims0|1> parsed INFORMATIONAL response 7 [ ]\n"
        );
        runner.set_file(&engine.shared.charon_log, after);
        assert!(engine.ike_evidence_since(&runner, mark).parsed_from_peer);
    }

    #[test]
    fn an_unreadable_log_reads_as_life_so_it_never_holds_a_line() {
        let runner = MockCommandRunner::new();
        let engine = engine();
        assert_eq!(
            engine.ike_evidence_since(&runner, LogMark::default()),
            IkeEvidence::LIFE
        );
    }

    // ---- T007: route_to ------------------------------------------------

    // MOCK JUSTIFICATION: the same as `MockCommandRunner`'s own (charon, a
    // netns and root are not available in `cargo test`); the outputs below are
    // what iproute2 printed on the rig for each kind of route (research R3).
    fn route_get(addr: &str, status: i32, stdout: &str) -> bool {
        let runner = MockCommandRunner::new();
        runner.set_run_output(&format!("ip route get {addr}"), output(status, stdout));
        route_to(&runner, addr)
    }

    #[test]
    fn a_normal_route_with_a_source_is_usable() {
        assert!(route_get(
            "192.0.2.123",
            0,
            "192.0.2.123 via 198.51.100.1 dev eth0 src 198.51.100.10 uid 0\n    cache\n"
        ));
        assert!(route_get(
            "2001:db8::1",
            0,
            "2001:db8::1 from :: via fe80::1 dev eth0 src 2001:db8::2 metric 100 pref medium\n"
        ));
    }

    #[test]
    fn every_rtnetlink_failure_and_a_missing_default_route_is_no_route() {
        // Blackhole (`Invalid argument`), unreachable (`Host is unreachable`),
        // prohibit (`Permission denied`) and no default route at all
        // (`Network is unreachable`) all exit 2 with the reason on stderr.
        assert!(!route_get("192.0.2.123", 2, ""));
    }

    #[test]
    fn exit_zero_without_a_source_address_is_no_route() {
        assert!(!route_get(
            "192.0.2.123",
            0,
            "192.0.2.123 dev eth0 uid 0\n    cache\n"
        ));
        assert!(!route_get("192.0.2.123", 0, ""));
    }

    // ---- T008: the engine seam -----------------------------------------

    #[test]
    fn route_ok_asks_about_this_lines_epdg() {
        let runner = MockCommandRunner::new();
        runner.set_run_output(
            "ip route get 203.0.113.9",
            output(
                0,
                "203.0.113.9 via 198.51.100.1 dev eth0 src 198.51.100.10\n",
            ),
        );
        let engine = engine();
        assert_eq!(engine.epdg_addr(), Some("203.0.113.9"));
        assert!(engine.route_ok(&runner));
        runner.set_run_output("ip route get 203.0.113.9", output(2, ""));
        assert!(!engine.route_ok(&runner));
    }

    #[test]
    fn an_engine_without_an_epdg_address_never_reports_no_route() {
        let runner = MockCommandRunner::new();
        let mut engine = engine();
        engine.epdg_addr = None;
        assert!(engine.route_ok(&runner));
        assert!(runner.run_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn send_dpd_runs_swanctl_dpd_scoped_to_this_connection_and_bounded() {
        let runner = MockCommandRunner::new();
        let engine = engine();
        assert!(engine.send_dpd(&runner));
        let calls = runner.run_calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![vec![
                "timeout".to_string(),
                "10".to_string(),
                "env".to_string(),
                "STRONGSWAN_CONF=/etc/strongswan-shared.conf".to_string(),
                "swanctl".to_string(),
                "--dpd".to_string(),
                "--ike".to_string(),
                "ims0".to_string(),
            ]]
        );
    }

    #[test]
    fn send_dpd_reports_a_refusal_as_not_sent() {
        let runner = MockCommandRunner::new();
        let engine = engine();
        runner.set_run_output(
            "timeout 10 env STRONGSWAN_CONF=/etc/strongswan-shared.conf swanctl --dpd --ike ims0",
            output(1, ""),
        );
        assert!(!engine.send_dpd(&runner));
    }

    #[test]
    fn a_listing_with_a_half_open_ike_sa_is_cached_and_reported_connecting() {
        let runner = MockCommandRunner::new();
        runner.set_run_output(
            "timeout 10 env STRONGSWAN_CONF=/etc/strongswan-shared.conf swanctl --list-sas --ike ims0",
            output(0, "ims0: #17, CONNECTING, IKEv2, aa_i* bb_r\n"),
        );
        let engine = engine();
        assert_eq!(
            engine.steady_state_health(&runner),
            SteadyStateHealth::IkeSaConnecting { ike_id: 17 }
        );
        assert_eq!(engine.ike_header(), Some((17, IkeHeaderState::Connecting)));
        // A listing that cannot be read drops the cached header.
        runner.set_run_output(
            "timeout 10 env STRONGSWAN_CONF=/etc/strongswan-shared.conf swanctl --list-sas --ike ims0",
            output(1, ""),
        );
        assert_eq!(
            engine.steady_state_health(&runner),
            SteadyStateHealth::ViciBroken
        );
        assert_eq!(engine.ike_header(), None);
    }
}
