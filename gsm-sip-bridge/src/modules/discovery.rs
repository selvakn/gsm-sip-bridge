use crate::error::BridgeResult;
use std::fs;
use std::path::{Path, PathBuf};

const EC20_VENDOR_ID: &str = "2c7c";
const EC20_PRODUCT_ID: &str = "0125";
const AT_INTERFACE_NUMBER: &str = "04";

#[derive(Debug, Clone)]
pub struct DiscoveredModule {
    pub id: String,
    pub serial_port: PathBuf,
    pub audio_device: String,
    pub usb_serial: String,
}

pub fn derive_module_id(identifier: &str) -> String {
    let clean: String = identifier
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
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
        if !is_ec20_device(&dev_path) {
            continue;
        }

        let usb_name = entry.file_name().to_string_lossy().to_string();
        let serial = read_sysfs_attr(&dev_path, "serial").unwrap_or_default();
        let identifier = if serial.is_empty() {
            usb_name.clone()
        } else {
            serial.clone()
        };
        let id = derive_module_id(&identifier);

        let serial_port = find_at_port(&dev_path, &usb_name);
        let audio_device = find_alsa_card(&dev_path);

        match (&serial_port, &audio_device) {
            (Some(port), Some(card)) => {
                tracing::info!(
                    module_id = %id,
                    usb_path = %usb_name,
                    serial_port = %port.display(),
                    audio_device = %card,
                    "discovered EC20 module"
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
                    usb_path = %usb_name,
                    serial_port = %port.display(),
                    "EC20 found but no ALSA audio device — audio bridging unavailable"
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
                    usb_path = %usb_name,
                    "EC20 found but cannot resolve serial port"
                );
            }
        }
    }

    Ok(modules)
}

fn is_ec20_device(path: &Path) -> bool {
    let vendor = read_sysfs_attr(path, "idVendor").unwrap_or_default();
    let product = read_sysfs_attr(path, "idProduct").unwrap_or_default();
    vendor == EC20_VENDOR_ID && product == EC20_PRODUCT_ID
}

fn find_at_port(dev_path: &Path, _usb_name: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(dev_path).ok()?;
    for entry in entries.flatten() {
        let iface_path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.contains(":") {
            continue;
        }
        let iface_num = read_sysfs_attr(&iface_path, "bInterfaceNumber").unwrap_or_default();
        if iface_num == AT_INTERFACE_NUMBER {
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
