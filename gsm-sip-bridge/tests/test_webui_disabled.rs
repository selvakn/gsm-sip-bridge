// specs/122-management-webui, FR-001/FR-004: the UI is off unless asked for,
// and a UI that cannot bind reports an error instead of taking the bridge
// down. `commands::daemon::run` blocks on OS signals, so (as in
// test_cs_disabled.rs) this pins the pieces its gate is built from.

use gsm_sip_bridge::config::load_config_with_snapshot;
use gsm_sip_bridge::webui::{self, WebuiState};
use std::io::Write;

fn load(extra: &str) -> gsm_sip_bridge::config::LoadedConfig {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    write!(
        f,
        "[sip]\nserver = \"127.0.0.1\"\nusername = \"user\"\npassword = \"p\"\n{extra}"
    )
    .unwrap();
    load_config_with_snapshot(f.path()).unwrap()
}

#[test]
fn a_config_without_webui_does_not_enable_it() {
    let loaded = load("");
    assert!(
        !loaded.config.webui.enabled,
        "the daemon spawns the listener only when this is true"
    );
}

#[test]
fn a_disabled_webui_table_with_no_password_still_loads() {
    let loaded = load("[webui]\nenabled = false\n");
    assert!(!loaded.config.webui.enabled);
}

#[tokio::test]
async fn a_port_already_in_use_is_an_error_not_a_panic() {
    let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = taken.local_addr().unwrap().port();
    let loaded = load(&format!(
        "[webui]\nenabled = true\npassword = \"x\"\nlisten_addr = \"127.0.0.1\"\nport = {port}\n"
    ));
    let result = webui::serve(WebuiState::new(loaded, None)).await;
    assert!(result.is_err(), "binding a taken port must report failure");
}
