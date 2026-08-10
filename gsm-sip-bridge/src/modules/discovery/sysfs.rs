//! Reading the USB device tree out of sysfs: which serial interfaces a modem
//! exposes, and which ALSA/network devices hang off it.
//!
//! Pure filesystem walking — no AT, no config, no serde. Split out of
//! `discovery::mod` because it is the one layer that can be exercised against
//! a `tempfile` directory tree instead of real hardware, and keeping it
//! separate makes that boundary explicit.

use std::fs;
use std::path::{Path, PathBuf};

/// One serial interface a modem exposes: its `/dev/ttyUSB*` device path and the
/// sysfs USB interface directory it lives under. The interface directory's name
/// is the stable USB-topology fragment (e.g. `5-1.2.1.2:1.1`) — carried
/// alongside the device path so a hung-port timeout can log it and the operator
/// blocklist can match on it (specs/030-bad-port-isolation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CandidatePort {
    pub(super) device_path: PathBuf,
    pub(super) iface_path: PathBuf,
}

/// Every `ttyUSB*` serial interface this USB device exposes, in a stable
/// (sorted) order — regardless of `bInterfaceNumber`, since which interface
/// answers AT varies by model/firmware (FR-002) and is no longer assumed.
pub(super) fn candidate_tty_ports(dev_path: &Path) -> Vec<CandidatePort> {
    let mut candidates = Vec::new();
    let Ok(entries) = fs::read_dir(dev_path) else {
        return candidates;
    };
    for entry in entries.flatten() {
        let iface_path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.contains(':') {
            continue;
        }
        if let Some(tty) = find_tty_in_path(&iface_path) {
            candidates.push(CandidatePort {
                device_path: PathBuf::from(format!("/dev/{tty}")),
                iface_path,
            });
        }
    }
    candidates.sort_by(|a, b| a.device_path.cmp(&b.device_path));
    candidates
}

fn find_tty_in_path(iface_path: &Path) -> Option<String> {
    let entries = fs::read_dir(iface_path).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("ttyUSB") {
            let tty_dir = entry.path().join("tty");
            if let Ok(inner) = fs::read_dir(&tty_dir) {
                for tty_entry in inner.flatten() {
                    let tty_name = tty_entry.file_name().to_string_lossy().to_string();
                    if tty_name.starts_with("ttyUSB") {
                        return Some(tty_name);
                    }
                }
            }
            return Some(name);
        }
    }
    None
}

/// Walks this device's USB interface directories whose name contains
/// `iface_marker`, looks inside each one's `subdir`, and returns the first
/// entry `pick` accepts.
///
/// `find_alsa_card` and `find_net_iface` were the same walk written twice —
/// the difference is only the subdirectory and how the entry name maps to a
/// result. Note the markers genuinely differ (`":1."` vs `":"`), so that stays
/// a parameter rather than being unified away.
fn find_in_iface_subdir<T>(
    dev_path: &Path,
    iface_marker: &str,
    subdir: &str,
    pick: impl Fn(&str) -> Option<T>,
) -> Option<T> {
    let entries = fs::read_dir(dev_path).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.contains(iface_marker) {
            continue;
        }
        let Ok(inner) = fs::read_dir(entry.path().join(subdir)) else {
            continue;
        };
        for inner_entry in inner.flatten() {
            if let Some(found) = pick(&inner_entry.file_name().to_string_lossy()) {
                return Some(found);
            }
        }
    }
    None
}

pub(super) fn find_alsa_card(dev_path: &Path) -> Option<String> {
    find_in_iface_subdir(dev_path, ":1.", "sound", |name| {
        name.strip_prefix("card")
            .map(|card_num| format!("hw:{card_num},0"))
    })
}

/// The host network interface a modem's data path exposes, if any — the
/// `net/<ifname>` under one of the device's USB interface directories (a
/// QMI/ECM `wwan*`/`usb*`/`enx*` device on the Quectel modules). Best-effort:
/// `None` when the modem exposes no netdev this boot, in which case the LTE
/// bridge falls back to the configured `iface`.
pub(super) fn find_net_iface(dev_path: &Path) -> Option<String> {
    find_in_iface_subdir(dev_path, ":", "net", |name| Some(name.to_string()))
}

pub(super) fn read_sysfs_attr(path: &Path, attr: &str) -> Option<String> {
    fs::read_to_string(path.join(attr))
        .ok()
        .map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_tty_interface(dev_dir: &Path, iface_name: &str, tty_name: &str, iface_num: &str) {
        let iface_dir = dev_dir.join(iface_name);
        fs::create_dir_all(&iface_dir).unwrap();
        fs::write(iface_dir.join("bInterfaceNumber"), iface_num).unwrap();
        let tty_tty_dir = iface_dir.join(tty_name).join("tty").join(tty_name);
        fs::create_dir_all(&tty_tty_dir).unwrap();
    }

    #[test]
    fn candidate_tty_ports_finds_every_interface_regardless_of_number() {
        let dir = tempfile::tempdir().unwrap();
        // Three candidate interfaces, arbitrary bInterfaceNumber values —
        // acceptance scenario 4: probing must not assume a fixed one.
        fake_tty_interface(dir.path(), "1-1:1.0", "ttyUSB0", "00");
        fake_tty_interface(dir.path(), "1-1:1.2", "ttyUSB2", "02");
        fake_tty_interface(dir.path(), "1-1:1.4", "ttyUSB4", "04");
        let candidates = candidate_tty_ports(dir.path());
        let device_paths: Vec<PathBuf> = candidates.iter().map(|c| c.device_path.clone()).collect();
        assert_eq!(
            device_paths,
            vec![
                PathBuf::from("/dev/ttyUSB0"),
                PathBuf::from("/dev/ttyUSB2"),
                PathBuf::from("/dev/ttyUSB4"),
            ]
        );
        // The USB interface (topology) path is captured alongside each device
        // path (specs/030-bad-port-isolation): the timeout log and the operator
        // blocklist both key off it.
        assert_eq!(
            candidates[0].iface_path.file_name().unwrap(),
            std::ffi::OsStr::new("1-1:1.0")
        );
    }

    #[test]
    fn candidate_tty_ports_empty_when_no_interfaces() {
        let dir = tempfile::tempdir().unwrap();
        assert!(candidate_tty_ports(dir.path()).is_empty());
    }

    #[test]
    fn candidate_tty_ports_ignores_non_interface_entries() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("idVendor"), "2c7c").unwrap();
        fake_tty_interface(dir.path(), "1-1:1.4", "ttyUSB4", "04");
        let candidates = candidate_tty_ports(dir.path());
        let device_paths: Vec<PathBuf> = candidates.iter().map(|c| c.device_path.clone()).collect();
        assert_eq!(device_paths, vec![PathBuf::from("/dev/ttyUSB4")]);
    }

    /// `find_alsa_card` and `find_net_iface` share one walk helper but must
    /// keep their different interface-name markers: `:1.` (configuration 1
    /// interfaces only) versus any `:`.
    #[test]
    fn alsa_and_net_lookups_read_their_own_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let iface = dir.path().join("1-1:1.0");
        fs::create_dir_all(iface.join("sound").join("card2")).unwrap();
        fs::create_dir_all(iface.join("net").join("wwan0")).unwrap();

        assert_eq!(find_alsa_card(dir.path()), Some("hw:2,0".to_string()));
        assert_eq!(find_net_iface(dir.path()), Some("wwan0".to_string()));
    }

    #[test]
    fn alsa_and_net_lookups_are_none_when_the_subdirectory_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("1-1:1.0")).unwrap();
        assert_eq!(find_alsa_card(dir.path()), None);
        assert_eq!(find_net_iface(dir.path()), None);
    }
}
