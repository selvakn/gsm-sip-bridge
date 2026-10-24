//! Shared fixture for the web UI integration tests (specs/122-management-
//! webui): the real router served on `127.0.0.1:0`, a real temp SQLite file,
//! and a real `reqwest` client. No mocks (Constitution I).
#![allow(dead_code)]

use gsm_sip_bridge::config::load_config_with_snapshot;
use gsm_sip_bridge::webui::{self, WebuiState};
use std::path::PathBuf;

pub const PASSWORD: &str = "test-password-synthetic";

pub struct Fixture {
    pub base: String,
    pub state: WebuiState,
    pub db_path: PathBuf,
    pub config_path: PathBuf,
    pub client: reqwest::Client,
    _dir: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    /// `extra` is appended to a minimal config that already enables `[webui]`
    /// and points `[sms].db_path` at a fresh temp file.
    pub async fn start(extra: &str) -> Self {
        Self::start_with_db(extra, None).await
    }

    /// Like [`Self::start`], with `[sms].db_path` pointed somewhere else --
    /// e.g. a path that cannot be opened.
    pub async fn start_with_db(extra: &str, db_override: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = match db_override {
            Some(p) => PathBuf::from(p),
            None => dir.path().join("store.db"),
        };
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "sip-password-synthetic"

[sms]
db_path = "{db}"

[webui]
enabled = true
port = 18080
password = "{PASSWORD}"
{extra}
"#,
                db = db_path.display()
            ),
        )
        .unwrap();
        Self::start_from(dir, config_path, db_path, None).await
    }

    /// Starts from a config file already written into `dir`.
    pub async fn start_from(
        dir: tempfile::TempDir,
        config_path: PathBuf,
        db_path: PathBuf,
        cs: Option<gsm_sip_bridge::modules::ControlCmdSender>,
    ) -> Self {
        let loaded = load_config_with_snapshot(&config_path).expect("test config must load");
        let state = WebuiState::new(loaded, cs);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let served = state.clone();
        let server = tokio::spawn(async move {
            let _ = webui::serve_on(listener, served).await;
        });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        Self {
            base,
            state,
            db_path,
            config_path,
            client,
            _dir: dir,
            server,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Logs in and returns the `gsb_session=<token>` cookie pair.
    pub async fn login(&self) -> String {
        self.login_as(PASSWORD).await
    }

    pub async fn login_as(&self, password: &str) -> String {
        let response = self
            .client
            .post(self.url("/api/login"))
            .json(&serde_json::json!({ "password": password }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 204, "login must succeed");
        let set_cookie = response
            .headers()
            .get("set-cookie")
            .expect("a session cookie")
            .to_str()
            .unwrap()
            .to_string();
        set_cookie.split(';').next().unwrap().to_string()
    }

    /// An authenticated GET returning the status and the JSON body.
    pub async fn get_json(&self, cookie: &str, path: &str) -> (u16, serde_json::Value) {
        let response = self
            .client
            .get(self.url(path))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.json().await.unwrap_or(serde_json::Value::Null);
        (status, body)
    }
}

/// Opens the fixture's store for writing, the way the daemon does, and keeps
/// it open so the WAL files outlive the call.
pub fn open_store(path: &std::path::Path) -> gsm_sip_bridge::store::StoreHandle {
    gsm_sip_bridge::store::StoreHandle::open(path).expect("open the test store")
}

/// Waits until `table` has `rows` rows, so a test does not race the store's
/// writer thread.
pub fn wait_for_rows(path: &std::path::Path, table: &str, rows: i64) {
    let conn = rusqlite::Connection::open(path).unwrap();
    for _ in 0..200 {
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or(0);
        if n == rows {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("{table} never reached {rows} rows");
}
