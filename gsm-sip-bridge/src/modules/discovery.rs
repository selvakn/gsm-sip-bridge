use crate::error::BridgeResult;
use std::fs;
use std::path::{Path, PathBuf};

/// One Quectel module variant this project knows how to recognize on USB.
/// `at_interface_number` is `None` for models with no usable
/// circuit-switched audio path (e.g. the EC200 tested here exposes no ALSA
/// device, unlike the EC20) — such a module is VoWiFi-only: still fully
/// usable via `[vowifi].modem_port` (which takes a plain user-set serial
/// path and never goes through this discovery table at all), but
/// deliberately excluded from circuit-switched-bridge discovery below
/// rather than partially discovered and left to fail later, more
/// confusingly, when a call actually tries to bridge audio.
struct KnownDevice {
    vendor_id: &'static str,
    product_id: &'static str,
    model: &'static str,
    at_interface_number: Option<&'static str>,
}

const KNOWN_DEVICES: &[KnownDevice] = &[
    KnownDevice {
        vendor_id: "2c7c",
        product_id: "0125",
        model: "EC20",
        at_interface_number: Some("04"),
    },
    KnownDevice {
        vendor_id: "2c7c",
        product_id: "0901",
        model: "EC200",
        at_interface_number: None,
    },
];

#[derive(Debug, Clone)]
pub struct DiscoveredModule {
    pub id: String,
    pub serial_port: PathBuf,
    pub audio_device: String,
    pub usb_serial: String,
}

pub fn derive_module_id(identifier: &str) -> String {
    let clean: String = identifier.chars().filter(|c| c.is_alphanumeric()).collect();
    let suffix = if clean.len() >= 6 {
        &clean[clean.len() - 6..]
    } else {
        &clean
    };
    format!("ec20-{}", suffix.to_ascii_uppercase())
}

pub fn scan_modules() -> BridgeResult<Vec<DiscoveredModule>> {
    let mut modules = Vec::new();

    let usb_devices = Path::new("/sys/bus/usb/devices");
    let entries = match fs::read_dir(usb_devices) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read /sys/bus/usb/devices");
            return Ok(modules);
        }
    };

    for entry in entries.flatten() {
        let dev_path = entry.path();
        let Some(device) = match_known_device(&dev_path) else {
            continue;
        };
        let usb_name = entry.file_name().to_string_lossy().to_string();

        let Some(at_interface_number) = device.at_interface_number else {
            tracing::info!(
                model = device.model,
                usb_path = %usb_name,
                "detected a VoWiFi-only module (no circuit-switched audio path) — \
                 not eligible for the circuit-switched bridge; use [vowifi].modem_port \
                 to enable VoWiFi mode on it instead"
            );
            continue;
        };

        let serial = read_sysfs_attr(&dev_path, "serial").unwrap_or_default();
        let identifier = if serial.is_empty() {
            usb_name.clone()
        } else {
            serial.clone()
        };
        let id = derive_module_id(&identifier);

        let serial_port = find_at_port(&dev_path, at_interface_number);
        let audio_device = find_alsa_card(&dev_path);

        match (&serial_port, &audio_device) {
            (Some(port), Some(card)) => {
                tracing::debug!(
                    module_id = %id,
                    model = device.model,
                    usb_path = %usb_name,
                    serial_port = %port.display(),
                    audio_device = %card,
                    "discovered module"
                );
                modules.push(DiscoveredModule {
                    id,
                    serial_port: port.clone(),
                    audio_device: card.clone(),
                    usb_serial: serial,
                });
            }
            (Some(port), None) => {
                tracing::warn!(
                    module_id = %id,
                    model = device.model,
                    usb_path = %usb_name,
                    serial_port = %port.display(),
                    "module found but no ALSA audio device — audio bridging unavailable"
                );
                modules.push(DiscoveredModule {
                    id,
                    serial_port: port.clone(),
                    audio_device: String::new(),
                    usb_serial: serial,
                });
            }
            _ => {
                tracing::warn!(
                    model = device.model,
                    usb_path = %usb_name,
                    "module found but cannot resolve serial port"
                );
            }
        }
    }

    Ok(modules)
}

fn match_known_device(path: &Path) -> Option<&'static KnownDevice> {
    let vendor = read_sysfs_attr(path, "idVendor").unwrap_or_default();
    let product = read_sysfs_attr(path, "idProduct").unwrap_or_default();
    KNOWN_DEVICES
        .iter()
        .find(|d| d.vendor_id == vendor && d.product_id == product)
}

fn find_at_port(dev_path: &Path, at_interface_number: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(dev_path).ok()?;
    for entry in entries.flatten() {
        let iface_path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.contains(":") {
            continue;
        }
        let iface_num = read_sysfs_attr(&iface_path, "bInterfaceNumber").unwrap_or_default();
        if iface_num == at_interface_number {
            if let Some(tty) = find_tty_in_path(&iface_path) {
                return Some(PathBuf::from(format!("/dev/{tty}")));
            }
        }
    }
    None
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

fn find_alsa_card(dev_path: &Path) -> Option<String> {
    let entries = fs::read_dir(dev_path).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.contains(":1.") {
            continue;
        }
        let sound_dir = entry.path().join("sound");
        if let Ok(sound_entries) = fs::read_dir(&sound_dir) {
            for sound_entry in sound_entries.flatten() {
                let card_name = sound_entry.file_name().to_string_lossy().to_string();
                if let Some(card_num) = card_name.strip_prefix("card") {
                    return Some(format!("hw:{card_num},0"));
                }
            }
        }
    }
    None
}

fn read_sysfs_attr(path: &Path, attr: &str) -> Option<String> {
    fs::read_to_string(path.join(attr))
        .ok()
        .map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(device.at_interface_number, Some("04"));
    }

    #[test]
    fn match_known_device_recognizes_ec200_as_vowifi_only() {
        let dir = tempfile::tempdir().unwrap();
        fake_device_dir(dir.path(), "2c7c", "0901");
        let device = match_known_device(dir.path()).unwrap();
        assert_eq!(device.model, "EC200");
        assert_eq!(
            device.at_interface_number, None,
            "EC200 has no circuit-switched audio path, so no AT interface is scanned for it"
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
