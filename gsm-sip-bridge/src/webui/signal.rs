//! The latest signal reading for each circuit-switched line
//! (specs/122-management-webui, data-model.md).
//!
//! The CS `ModuleWorker` runs in the daemon process, so it can write here
//! directly. VoWiFi/VoLTE readings arrive in `AgentReport.details` instead.

use crate::modules::signal::SignalReading;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

fn readings() -> &'static Mutex<HashMap<String, SignalReading>> {
    static READINGS: OnceLock<Mutex<HashMap<String, SignalReading>>> = OnceLock::new();
    READINGS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Records the latest reading for `module_id`.
pub fn update(module_id: &str, reading: SignalReading) {
    readings()
        .lock()
        .unwrap()
        .insert(module_id.to_string(), reading);
}

/// Every CS line's latest reading.
pub fn all() -> HashMap<String, SignalReading> {
    readings().lock().unwrap().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_reading_replaces_the_previous_one() {
        update(
            "sig-test-a",
            SignalReading {
                rssi_dbm: Some(-90),
                ..Default::default()
            },
        );
        update(
            "sig-test-a",
            SignalReading {
                rssi_dbm: Some(-70),
                ..Default::default()
            },
        );
        assert_eq!(all()["sig-test-a"].rssi_dbm, Some(-70));
    }
}
