//! Reading and writing the line manifests that separately-spawned processes
//! use to agree on the same line table without each re-scanning USB.
//!
//! # Why these carry a schema version
//!
//! A manifest is a contract between processes that are **not guaranteed to be
//! the same build**. `supervise` writes it once at startup and then spawns
//! per-line agents that read it back; during a rolling image update, or when
//! an operator runs a `volte-status` from a different binary against a
//! running container, writer and reader can disagree about the shape.
//!
//! Both manifests previously used bare `#[derive(Deserialize)]` with
//! `#[serde(default)]` on several fields, which means a shape change is
//! silently *tolerated*: a renamed field deserialises as its default, and a
//! line comes up with an empty APN or an empty netns rather than failing. An
//! empty APN is not hypothetical — it is documented in `volte::discovery` as
//! having made `AT+CGDCONT` request the network's default bearer instead of
//! the IMS one, producing a line that attached and looked fully configured
//! while the P-CSCF was unreachable.
//!
//! Refusing to read a manifest we do not understand converts that class of
//! silent misbehaviour into a startup error naming the mismatch.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Current manifest schema. Bump on any change to a manifest entry's shape
/// that an older reader would misinterpret — a renamed or removed field, or a
/// new field whose absence is not safely defaulted.
pub const SCHEMA_VERSION: u32 = 1;

/// A versioned manifest envelope. The payload is whatever the transport's own
/// line entry is; only the version and the reading/writing are shared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Manifest<T> {
    /// Absent in manifests written before versioning existed, which is
    /// exactly the case that must be rejected rather than defaulted — so
    /// this deserialises to 0, a version no writer ever emits.
    #[serde(default)]
    pub schema_version: u32,
    pub lines: Vec<T>,
}

impl<T> Manifest<T> {
    pub fn new(lines: Vec<T>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            lines,
        }
    }
}

impl<T> Default for Manifest<T> {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// Resolves the path a manifest's readers and writers should agree on:
/// `env_var` if set, else `default_path`.
pub fn path_from_env(env_var: &str, default_path: &str) -> PathBuf {
    PathBuf::from(std::env::var(env_var).unwrap_or_else(|_| default_path.to_string()))
}

/// Where the VoWiFi line resolution lives. Written by `discover`, read by the
/// circuit-switched scan (for its port exclusions), every `--line`-selecting
/// agent, `vowifi-status`, and `healthcheck`.
pub const VOWIFI_LINES_DEFAULT_PATH: &str = "/tmp/gsm-sip-bridge-lines.json";
pub const VOWIFI_LINES_ENV: &str = "GSM_SIP_BRIDGE_LINES_FILE";

/// Where the VoLTE line manifest lives. Written by `volte-discover-lines`,
/// read by `volte-carrier-agent`, `volte-bridge`, `volte-status`, and
/// teardown.
pub const VOLTE_LINES_DEFAULT_PATH: &str = "/run/volte-lines.json";
pub const VOLTE_LINES_ENV: &str = "GSM_SIP_BRIDGE_VOLTE_LINES_FILE";

/// These four constants live here, below both subsystems, for a specific
/// reason. `modules::discovery` is the shared USB/AT scan underneath *both*
/// VoWiFi and VoLTE, so it cannot import from either — and it needs both
/// paths, to exclude ports already claimed by a line. Its previous answer was
/// to keep *private copies* of VoLTE's two constants with a comment saying
/// "keep both copies in sync if the manifest path ever changes". Two sources
/// of truth for a path shared across process boundaries, maintained by
/// remembering.
///
/// Putting them in a layer below everything that needs them removes the
/// dilemma rather than documenting it.
pub fn vowifi_lines_path() -> PathBuf {
    path_from_env(VOWIFI_LINES_ENV, VOWIFI_LINES_DEFAULT_PATH)
}

pub fn volte_lines_path() -> PathBuf {
    path_from_env(VOLTE_LINES_ENV, VOLTE_LINES_DEFAULT_PATH)
}

/// Serialises and writes a manifest. Best-effort by contract: a write failure
/// degrades cleanup and status reporting, not the calls themselves, so the
/// caller logs it rather than aborting.
pub fn write<T: Serialize>(path: &Path, manifest: &Manifest<T>) -> Result<(), String> {
    let json = serde_json::to_string_pretty(manifest)
        .map_err(|e| format!("failed to serialize manifest: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

/// Reads a manifest, refusing one written by an incompatible schema.
pub fn read<T: DeserializeOwned>(path: &Path) -> Result<Manifest<T>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let manifest: Manifest<T> = serde_json::from_str(&raw)
        .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;

    if manifest.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "{}: manifest schema version {} is not the {} this binary writes \
             — it was produced by a different build. Delete it and let \
             `supervise` regenerate it rather than reading fields that may \
             have moved.",
            path.display(),
            manifest.schema_version,
            SCHEMA_VERSION,
        ));
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Entry {
        index: u32,
        apn: String,
    }

    fn entry(index: u32) -> Entry {
        Entry {
            index,
            apn: "ims".to_string(),
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gsb-manifest-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn a_manifest_round_trips_through_the_filesystem() {
        let p = tmp("roundtrip.json");
        let m = Manifest::new(vec![entry(0), entry(1)]);

        write(&p, &m).unwrap();
        let back: Manifest<Entry> = read(&p).unwrap();

        assert_eq!(back, m);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn a_newly_written_manifest_always_carries_the_current_version() {
        let m: Manifest<Entry> = Manifest::new(vec![]);
        assert_eq!(m.schema_version, SCHEMA_VERSION);
        assert_eq!(Manifest::<Entry>::default().schema_version, SCHEMA_VERSION);
    }

    /// The whole point: a manifest from a different build is refused, not
    /// silently reinterpreted with defaulted fields.
    #[test]
    fn a_future_schema_version_is_refused_with_a_message_naming_both_versions() {
        let p = tmp("future.json");
        std::fs::write(&p, r#"{"schema_version": 99, "lines": []}"#).unwrap();

        let err = read::<Entry>(&p).unwrap_err();

        assert!(
            err.contains("99"),
            "error must name the found version: {err}"
        );
        assert!(
            err.contains(&SCHEMA_VERSION.to_string()),
            "error must name the expected version: {err}"
        );
    }

    /// A pre-versioning manifest has no `schema_version` at all. It must be
    /// rejected rather than defaulting to "current" — its fields are exactly
    /// the ones that may have moved.
    #[test]
    fn an_unversioned_manifest_is_refused_rather_than_assumed_current() {
        let p = tmp("unversioned.json");
        std::fs::write(&p, r#"{"lines": [{"index": 0, "apn": "ims"}]}"#).unwrap();

        assert!(read::<Entry>(&p).is_err());
    }

    #[test]
    fn a_missing_file_is_an_error_naming_the_path() {
        let p = tmp("nope.json");
        let _ = std::fs::remove_file(&p);
        let err = read::<Entry>(&p).unwrap_err();
        assert!(err.contains("nope.json"), "{err}");
    }

    #[test]
    fn malformed_json_is_an_error_naming_the_path_not_a_panic() {
        let p = tmp("garbage.json");
        std::fs::write(&p, "{not json").unwrap();
        let err = read::<Entry>(&p).unwrap_err();
        assert!(err.contains("garbage.json"), "{err}");
    }

    #[test]
    fn the_path_env_var_overrides_the_default_when_set() {
        let var = "GSM_SIP_BRIDGE_TEST_MANIFEST_PATH";
        std::env::remove_var(var);
        assert_eq!(
            path_from_env(var, "/run/default.json"),
            PathBuf::from("/run/default.json")
        );

        std::env::set_var(var, "/tmp/override.json");
        assert_eq!(
            path_from_env(var, "/run/default.json"),
            PathBuf::from("/tmp/override.json")
        );
        std::env::remove_var(var);
    }
}
