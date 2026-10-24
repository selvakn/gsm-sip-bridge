// specs/122-management-webui, User Story 5: the Config page shows what the
// bridge is running with and never a secret. Every secret in the shipped
// example is replaced by a unique sentinel; none may appear in the response.

mod common;

use common::Fixture;
use gsm_sip_bridge::webui::config_view::is_secret_key;
use std::collections::BTreeSet;

const EXAMPLE: &str = include_str!("../../config.toml.example");
const RAW: &str = include_str!("../src/config/raw.rs");

/// The names of every `Secret<..>` field the parser declares.
fn secret_field_names() -> BTreeSet<String> {
    RAW.lines()
        .filter(|l| l.contains("Secret<"))
        .filter_map(|l| l.trim().strip_prefix("pub "))
        .filter_map(|l| l.split(':').next())
        .map(str::to_string)
        .collect()
}

#[test]
fn every_secret_field_in_the_parser_is_caught_by_the_redaction_rule() {
    let names = secret_field_names();
    assert!(
        names.contains("password") && names.contains("discord_webhook_url"),
        "the scan must find the known secrets, or it proves nothing: {names:?}"
    );
    for name in names {
        assert!(
            is_secret_key(&name),
            "`{name}` is a Secret field but would be shown on the Config page; \
             extend config_view::is_secret_key"
        );
    }
}

/// `[name]` as a mutable table, created if the document lacks it.
fn table<'a>(doc: &'a mut toml::Table, name: &str) -> &'a mut toml::Table {
    doc.entry(name)
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .unwrap()
}

fn text(value: &str) -> toml::Value {
    toml::Value::String(value.to_string())
}

#[tokio::test]
async fn no_secret_reaches_the_config_page_but_ordinary_values_do() {
    std::env::set_var("SIP_PASSWORD", "SENTINEL-env-resolved-sip-password");
    std::env::set_var(
        "DISCORD_WEBHOOK_URL",
        "https://discord.example/SENTINEL-env-webhook",
    );
    std::env::set_var("WEBUI_TEST_SECRET", "SENTINEL-env-webui-secret");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let db_path = dir.path().join("store.db");

    let mut doc: toml::Table = toml::from_str(EXAMPLE).unwrap();
    // A literal secret in every place the parser takes one.
    table(&mut doc, "sip").insert("password".into(), text("SENTINEL-sip-password"));
    let sms = table(&mut doc, "sms");
    sms.insert(
        "discord_webhook_url".into(),
        text("https://discord.example/SENTINEL-sms"),
    );
    sms.insert("db_path".into(), text(db_path.to_str().unwrap()));
    let alerts = table(&mut doc, "alerts");
    alerts.insert(
        "discord_webhook_url".into(),
        text("https://discord.example/SENTINEL-alerts"),
    );
    for category in [
        "sms",
        "module_lifecycle",
        "registration_loss",
        "tunnel_failure",
        "missed_call",
        "line_discovery_failed",
        "gm_connection_lost",
        "pcscf_pool_exhausted",
    ] {
        table(alerts, category).insert(
            "discord_webhook_url".into(),
            text(&format!("https://discord.example/SENTINEL-{category}")),
        );
    }
    // One secret is an `env:` reference whose variable resolves to a sentinel:
    // the reference may be shown, the value may not.
    let webui = table(&mut doc, "webui");
    webui.insert("enabled".into(), toml::Value::Boolean(true));
    webui.insert("password".into(), text("env:WEBUI_TEST_SECRET"));
    webui.insert("port".into(), toml::Value::Integer(18081));
    // An account on the embedded registrar, with a literal password. The
    // registrar itself stays off, so the example's other validation is moot.
    let mut account = toml::Table::new();
    account.insert("username".into(), text("1001"));
    account.insert("password".into(), text("SENTINEL-phone-password"));
    let sip_server = table(&mut doc, "sip_server");
    sip_server.insert("enabled".into(), toml::Value::Boolean(false));
    sip_server.insert(
        "account".into(),
        toml::Value::Array(vec![toml::Value::Table(account)]),
    );
    std::fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();

    let f = Fixture::start_from(dir, path, db_path, None).await;
    let cookie = f.login_as("SENTINEL-env-webui-secret").await;
    let response = f
        .client
        .get(f.url("/api/config"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();

    assert!(
        !text.contains("SENTINEL"),
        "a secret reached the Config page:\n{}",
        text.lines()
            .filter(|l| l.contains("SENTINEL"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        text.contains("env:WEBUI_TEST_SECRET"),
        "an env: reference is shown as written"
    );
    assert!(text.contains("[REDACTED]"));

    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        body["document"]["metrics"]["port"], 9091,
        "ordinary values are unchanged"
    );
    assert_eq!(body["document"]["webui"]["port"], 18081);
    assert!(body["loaded_at"].is_string());
    assert_eq!(
        body["note"],
        "As loaded at startup; later edits to the file are not shown."
    );
    assert!(body["source"].as_str().unwrap().ends_with("config.toml"));
}

#[tokio::test]
async fn the_page_needs_a_session() {
    let f = Fixture::start("").await;
    let response = f.client.get(f.url("/api/config")).send().await.unwrap();
    assert_eq!(response.status(), 401);
}
