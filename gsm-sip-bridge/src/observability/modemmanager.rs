use std::process::Command;

pub fn check_modemmanager() {
    let active = Command::new("systemctl")
        .args(["is-active", "ModemManager"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let dbus_present = std::path::Path::new("/run/dbus/system_bus_socket").exists();

    if active == "active" || dbus_present {
        if active == "active" {
            tracing::warn!(
                "ModemManager is active and may interfere with EC20 serial ports. \
                 Disable it with: systemctl disable --now ModemManager"
            );
        }
    }
}
