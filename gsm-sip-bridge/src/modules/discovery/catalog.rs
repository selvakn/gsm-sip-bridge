//! The table of Quectel modules this project recognizes on USB.
//!
//! Split out of `discovery::mod` so adding a model is a one-file change with
//! its own tests, rather than an edit near the top of the scan.

use super::sysfs::read_sysfs_attr;
use std::path::Path;

/// One Quectel module variant this project knows how to recognize on USB.
/// `has_audio_capability` is a static property of the model — `false` for
/// modules with no usable circuit-switched audio path at all (e.g. the
/// EC200 tested here exposes no ALSA device, unlike the EC20). Unlike the
/// AT-capable interface (found by live probing, specs/013-multi-card-vowifi
/// FR-002), a model's audio capability isn't something a boot-time probe can
/// discover — an audio-capable model with no ALSA device enumerated *this*
/// boot is still audio-capable and stays eligible for the circuit-switched
/// pool (`scan_modules`), whereas an audio-less model never is, regardless of
/// what's live.
pub(super) struct KnownDevice {
    vendor_id: &'static str,
    product_id: &'static str,
    pub(super) model: &'static str,
    pub(super) has_audio_capability: bool,
}

const KNOWN_DEVICES: &[KnownDevice] = &[
    KnownDevice {
        vendor_id: "2c7c",
        product_id: "0125",
        model: "EC20",
        has_audio_capability: true,
    },
    KnownDevice {
        vendor_id: "2c7c",
        product_id: "0901",
        model: "EC200",
        has_audio_capability: false,
    },
];

pub(super) fn match_known_device(path: &Path) -> Option<&'static KnownDevice> {
    let vendor = read_sysfs_attr(path, "idVendor").unwrap_or_default();
    let product = read_sysfs_attr(path, "idProduct").unwrap_or_default();
    KNOWN_DEVICES
        .iter()
        .find(|d| d.vendor_id == vendor && d.product_id == product)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fake_device_dir(dir: &Path, vendor: &str, product: &str) {
        fs::write(dir.join("idVendor"), vendor).unwrap();
        fs::write(dir.join("idProduct"), product).unwrap();
    }

    #[test]
    fn match_known_device_recognizes_ec20() {
        let dir = tempfile::tempdir().unwrap();
        fake_device_dir(dir.path(), "2c7c", "0125");
        let device = match_known_device(dir.path()).unwrap();
        assert_eq!(device.model, "EC20");
        assert!(device.has_audio_capability);
    }

    #[test]
    fn match_known_device_recognizes_ec200_as_vowifi_only() {
        let dir = tempfile::tempdir().unwrap();
        fake_device_dir(dir.path(), "2c7c", "0901");
        let device = match_known_device(dir.path()).unwrap();
        assert_eq!(device.model, "EC200");
        assert!(
            !device.has_audio_capability,
            "EC200 has no circuit-switched audio path, but is still recognized \
             (not skipped) so it can be probed for VoWiFi (FR-003)"
        );
    }

    #[test]
    fn match_known_device_returns_none_for_unrelated_vendor() {
        let dir = tempfile::tempdir().unwrap();
        fake_device_dir(dir.path(), "1234", "5678");
        assert!(match_known_device(dir.path()).is_none());
    }

    #[test]
    fn match_known_device_returns_none_when_sysfs_attrs_missing() {
        let dir = tempfile::tempdir().unwrap();
        // No idVendor/idProduct files at all — e.g. a non-device directory
        // that happened to be listed under /sys/bus/usb/devices.
        assert!(match_known_device(dir.path()).is_none());
    }
}
