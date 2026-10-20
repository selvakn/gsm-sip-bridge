//! The per-line P-CSCF files beside `/tmp/pcscf-N` (specs/091,
//! `contracts/files.md`): the pool the supervisor publishes and the state the
//! agent keeps across its own restarts, which the supervisor also reads to tell
//! when the agent has given up on every P-CSCF (specs/097). Plus the agent's
//! active-call count, which the supervisor reads to hold a stuck-rekey rebuild
//! for a call (specs/093 `contracts/files.md`).
//!
//! Shared by both sides so the writer and the reader cannot disagree on a
//! path or a format. The selection file itself (`/tmp/pcscf-N`) is untouched:
//! `healthcheck`, the keepalive loop and `EpdgTransport` all read it as a
//! single address (FR-004).

use crate::control::protocol::UplinkState;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// `/tmp/pcscf-N.pool` for the selection file `/tmp/pcscf-N`.
pub fn pool_path(selection_path: &str) -> PathBuf {
    PathBuf::from(format!("{selection_path}.pool"))
}

/// `/tmp/pcscf-N.state` for the selection file `/tmp/pcscf-N`.
pub fn state_path(selection_path: &str) -> PathBuf {
    PathBuf::from(format!("{selection_path}.state"))
}

/// `/tmp/pcscf-N.calls` for the selection file `/tmp/pcscf-N`.
pub fn calls_path(selection_path: &str) -> PathBuf {
    PathBuf::from(format!("{selection_path}.calls"))
}

/// Writes the active-call count (`"1\n"`) atomically.
pub fn write_active_calls(path: &Path, n: u32) -> io::Result<()> {
    write_atomic(path, &format!("{n}\n"))
}

/// The active-call count in a `.calls` file's contents; `None` for anything
/// that is not a plain unsigned number, which the reader treats as "no call".
pub fn parse_active_calls(contents: &str) -> Option<u32> {
    contents.trim().parse().ok()
}

/// `/tmp/pcscf-N.uplink` for the selection file `/tmp/pcscf-N` (specs/100).
pub fn uplink_path(selection_path: &str) -> PathBuf {
    PathBuf::from(format!("{selection_path}.uplink"))
}

/// The `.uplink` file's version (specs/100 `contracts/status-file.md`).
pub const UPLINK_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
struct UplinkFile {
    v: u32,
    state: UplinkState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    since: Option<u64>,
}

/// One line of JSON for the `.uplink` file: the supervisor's verdict on the
/// path to the line's ePDG, and when a hold began (only while held).
pub fn render_uplink(state: UplinkState, since: Option<u64>) -> String {
    let file = UplinkFile {
        v: UPLINK_VERSION,
        state,
        since: since.filter(|_| state != UplinkState::Up),
    };
    // Serializing three plain fields cannot fail.
    format!("{}\n", serde_json::to_string(&file).unwrap_or_default())
}

/// An `.uplink` file's contents; `None` for anything that does not parse, names
/// an unknown state, or carries another version, which `vowifi-status` prints
/// as `unknown`.
pub fn parse_uplink(contents: &str) -> Option<(UplinkState, Option<u64>)> {
    serde_json::from_str::<UplinkFile>(contents.trim())
        .ok()
        .filter(|f| f.v == UPLINK_VERSION)
        .map(|f| (f.state, f.since))
}

/// One address per line, trailing newline.
pub fn render_pool<S: AsRef<str>>(pool: &[S]) -> String {
    let mut out = String::new();
    for a in pool {
        out.push_str(a.as_ref());
        out.push('\n');
    }
    out
}

/// Parses a `.pool` file: blank lines and anything that is not an IP address
/// are skipped, duplicates dropped, order kept.
pub fn parse_pool(contents: &str) -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::new();
    for addr in contents.lines().filter_map(|l| l.trim().parse().ok()) {
        if !out.contains(&addr) {
            out.push(addr);
        }
    }
    out
}

/// The published pool, or empty when there is none (a missing or unreadable
/// file means "no pool published", FR-006).
pub fn read_pool(path: &Path) -> Vec<IpAddr> {
    std::fs::read_to_string(path)
        .map(|c| parse_pool(&c))
        .unwrap_or_default()
}

/// The `.state` format's version; a file carrying any other is ignored whole.
pub const POOL_STATE_VERSION: u32 = 1;

/// The agent's `.state` file (specs/091 `contracts/files.md`): the P-CSCF it
/// last registered through and the ones it has marked unavailable. Written by
/// the agent only; read back by the agent across its own restarts and, since
/// specs/097, by the supervisor every steady-state tick.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PoolState {
    pub version: u32,
    pub last_registered: Option<IpAddr>,
    #[serde(default)]
    pub marks: Vec<PoolStateMark>,
}

/// One P-CSCF the agent is avoiding, and until when.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PoolStateMark {
    pub addr: IpAddr,
    pub until_unix: u64,
    pub failures: u32,
    /// The agent's `FailureCause` rendering: `gm-keepalive`,
    /// `register-timeout`, `transport`, `endpoint-failure:<c>`, `retry-after`.
    #[serde(default)]
    pub cause: String,
    /// When the mark was last set or renewed (specs/097). `0` in a file
    /// written before the field existed, which reads as "unknown, too old to
    /// count" -- no version bump, so an older agent still loads the file.
    #[serde(default)]
    pub marked_at_unix: u64,
}

/// A `.state` file's contents, or `None` when they do not parse or carry an
/// unsupported version.
pub fn parse_pool_state(contents: &str) -> Option<PoolState> {
    serde_json::from_str::<PoolState>(contents)
        .ok()
        .filter(|s| s.version == POOL_STATE_VERSION)
}

/// [`parse_pool_state`] of the file at `path`; `None` when it is missing or
/// unreadable too.
pub fn read_pool_state(path: &Path) -> Option<PoolState> {
    parse_pool_state(&std::fs::read_to_string(path).ok()?)
}

/// Writes `<path>.tmp` and renames it over `path`, so a concurrent reader sees
/// the old contents or the new, never a torn mix.
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pcscf-files-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mark(addr: &str, marked_at_unix: u64) -> PoolStateMark {
        PoolStateMark {
            addr: addr.parse().unwrap(),
            until_unix: 1_790_000_330,
            failures: 1,
            cause: "gm-keepalive".into(),
            marked_at_unix,
        }
    }

    #[test]
    fn a_pool_state_round_trips_with_its_marking_time() {
        let state = PoolState {
            version: POOL_STATE_VERSION,
            last_registered: Some("10.0.0.10".parse().unwrap()),
            marks: vec![mark("10.0.0.11", 1_790_000_000)],
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"marked_at_unix\":1790000000"), "{json}");
        assert_eq!(parse_pool_state(&json), Some(state));
    }

    #[test]
    fn a_state_file_from_before_the_marking_time_reads_it_as_zero() {
        let old = r#"{"version":1,"last_registered":null,"marks":[
            {"addr":"10.0.0.11","until_unix":1790000330,"failures":2,"cause":"transport"}]}"#;
        let state = parse_pool_state(old).unwrap();
        assert_eq!(state.marks[0].marked_at_unix, 0);
        assert_eq!(state.marks[0].failures, 2);
    }

    #[test]
    fn an_unusable_state_file_reads_as_none() {
        let dir = temp_dir();
        assert_eq!(read_pool_state(&dir.join("absent.state")), None);
        let garbage = dir.join("garbage.state");
        std::fs::write(&garbage, "not json").unwrap();
        assert_eq!(read_pool_state(&garbage), None);
        let v2 = dir.join("v2.state");
        std::fs::write(&v2, r#"{"version":2,"last_registered":null,"marks":[]}"#).unwrap();
        assert_eq!(read_pool_state(&v2), None);
        let ok = dir.join("ok.state");
        std::fs::write(&ok, r#"{"version":1,"last_registered":null}"#).unwrap();
        assert_eq!(read_pool_state(&ok).map(|s| s.marks.len()), Some(0));
    }

    #[test]
    fn paths_sit_beside_the_selection_file() {
        assert_eq!(
            pool_path("/tmp/pcscf-0"),
            PathBuf::from("/tmp/pcscf-0.pool")
        );
        assert_eq!(
            state_path("/tmp/pcscf-1"),
            PathBuf::from("/tmp/pcscf-1.state")
        );
    }

    #[test]
    fn the_calls_file_sits_beside_the_selection_and_round_trips() {
        assert_eq!(
            calls_path("/tmp/pcscf-1"),
            PathBuf::from("/tmp/pcscf-1.calls")
        );
        let dir = temp_dir();
        let path = dir.join("pcscf-0.calls");
        write_active_calls(&path, 1).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, "1\n");
        assert_eq!(parse_active_calls(&written), Some(1));
        write_active_calls(&path, 0).unwrap();
        assert_eq!(
            parse_active_calls(&std::fs::read_to_string(&path).unwrap()),
            Some(0)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_calls_file_that_is_not_a_count_reads_as_none() {
        for garbage in ["", "\n", "-1\n", "one\n", "1 2\n"] {
            assert_eq!(parse_active_calls(garbage), None, "{garbage:?}");
        }
    }

    #[test]
    fn parse_pool_skips_garbage_and_duplicates_and_keeps_order() {
        let pool =
            parse_pool("10.0.1.10\n\nnot-an-address\n2402:8100::10\n 10.0.0.10 \n10.0.1.10\n");
        let want: Vec<IpAddr> = ["10.0.1.10", "2402:8100::10", "10.0.0.10"]
            .iter()
            .map(|a| a.parse().unwrap())
            .collect();
        assert_eq!(pool, want);
    }

    #[test]
    fn a_rendered_pool_parses_back() {
        let rendered = render_pool(&["10.0.0.10", "10.0.1.10"]);
        assert_eq!(rendered, "10.0.0.10\n10.0.1.10\n");
        assert_eq!(parse_pool(&rendered).len(), 2);
    }

    #[test]
    fn write_atomic_replaces_the_file_and_leaves_no_temp_behind() {
        let dir = temp_dir();
        let path = dir.join("pcscf-0.pool");
        write_atomic(&path, "10.0.0.10\n").unwrap();
        write_atomic(&path, "10.0.1.10\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "10.0.1.10\n");
        assert!(!dir.join("pcscf-0.pool.tmp").exists());
        assert!(read_pool(&dir.join("missing.pool")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_uplink_file_sits_beside_the_selection_file() {
        assert_eq!(
            uplink_path("/tmp/pcscf-1"),
            PathBuf::from("/tmp/pcscf-1.uplink")
        );
    }

    #[test]
    fn the_uplink_file_round_trips_each_state() {
        let up = render_uplink(UplinkState::Up, Some(1_792_500_000));
        assert_eq!(up, "{\"v\":1,\"state\":\"up\"}\n", "no `since` while up");
        assert_eq!(parse_uplink(&up), Some((UplinkState::Up, None)));
        let held = render_uplink(UplinkState::NoRoute, Some(1_792_500_000));
        assert_eq!(
            held,
            "{\"v\":1,\"state\":\"no_route\",\"since\":1792500000}\n"
        );
        assert_eq!(
            parse_uplink(&held),
            Some((UplinkState::NoRoute, Some(1_792_500_000)))
        );
        assert_eq!(
            parse_uplink(&render_uplink(UplinkState::NoResponse, Some(5))),
            Some((UplinkState::NoResponse, Some(5)))
        );
    }

    #[test]
    fn an_unreadable_uplink_file_parses_to_nothing() {
        for bad in [
            "",
            "not json",
            "{\"v\":2,\"state\":\"up\"}",
            "{\"v\":1,\"state\":\"sideways\"}",
            "{\"state\":\"up\"}",
        ] {
            assert_eq!(parse_uplink(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_uplink_file_is_written_atomically() {
        let dir = temp_dir();
        let path = dir.join("pcscf-9.uplink");
        write_atomic(&path, &render_uplink(UplinkState::NoRoute, Some(7))).unwrap();
        let read = std::fs::read_to_string(&path).unwrap();
        assert_eq!(parse_uplink(&read), Some((UplinkState::NoRoute, Some(7))));
    }
}
