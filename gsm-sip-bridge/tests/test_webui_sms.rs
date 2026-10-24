// specs/122-management-webui, User Story 2: SMS is read from the store, never
// the modem, and nothing the UI does changes it.

mod common;

use common::{open_store, wait_for_rows, Fixture};
use gsm_sip_bridge::store::sms::SmsRecord;
use gsm_sip_bridge::store::{StoreCommand, Transport};

fn sms(module: &str, sender: &str, body: &str, at: &str, status: &str) -> SmsRecord {
    SmsRecord {
        module_id: module.to_string(),
        sender: sender.to_string(),
        body: body.to_string(),
        received_at: at.to_string(),
        forwarding_status: status.to_string(),
        transport: Transport::Vowifi,
        retry_eligible: false,
    }
}

/// Five messages over two lines, one failed to forward, one hostile, one OTP.
fn seed(f: &Fixture) -> gsm_sip_bridge::store::StoreHandle {
    let store = open_store(&f.db_path);
    let tx = store.sender();
    for record in [
        sms(
            "line-a",
            "+919000000001",
            "hello there",
            "2026-10-24T10:00:00Z",
            "sent",
        ),
        sms(
            "line-a",
            "+919000000001",
            "<script>alert(1)</script>",
            "2026-10-24T10:01:00Z",
            "sent",
        ),
        sms(
            "line-b",
            "BANK",
            "Your OTP is 482913. Do not share it.",
            "2026-10-24T10:02:00Z",
            "failed",
        ),
        sms(
            "line-b",
            "+919000000001",
            "100% sure_thing",
            "2026-10-24T10:03:00Z",
            "sent",
        ),
        sms(
            "line-a",
            "+919000000002",
            "newest of all",
            "2026-10-24T10:04:00Z",
            "pending",
        ),
    ] {
        tx.send(StoreCommand::InsertSms(record)).unwrap();
    }
    wait_for_rows(&f.db_path, "sms", 5);
    store
}

fn statuses(f: &Fixture) -> Vec<(i64, String)> {
    let conn = rusqlite::Connection::open(&f.db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT id, forwarding_status FROM sms ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn sms_comes_back_newest_first_with_paging_totals() {
    let f = Fixture::start("").await;
    let _store = seed(&f);
    let cookie = f.login().await;

    let (status, body) = f.get_json(&cookie, "/api/sms").await;
    assert_eq!(status, 200);
    assert_eq!(body["total"], 5);
    assert_eq!(body["page"], 1);
    assert_eq!(body["per_page"], 50);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    assert_eq!(items[0]["body"], "newest of all");
    assert_eq!(items[4]["body"], "hello there");

    let (_, page2) = f.get_json(&cookie, "/api/sms?per_page=2&page=2").await;
    assert_eq!(page2["total"], 5, "the total ignores paging");
    assert_eq!(page2["per_page"], 2);
    let items = page2["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["body"], "Your OTP is 482913. Do not share it.");

    let (_, huge) = f.get_json(&cookie, "/api/sms?per_page=100000").await;
    assert_eq!(huge["per_page"], 200, "an oversize page is clamped");
}

#[tokio::test]
async fn the_line_filter_and_search_narrow_the_list() {
    let f = Fixture::start("").await;
    let _store = seed(&f);
    let cookie = f.login().await;

    let (_, by_line) = f.get_json(&cookie, "/api/sms?line=line-b").await;
    assert_eq!(by_line["total"], 2);

    let (_, by_sender) = f.get_json(&cookie, "/api/sms?q=BANK").await;
    assert_eq!(by_sender["total"], 1);
    assert_eq!(by_sender["items"][0]["forwarding_status"], "failed");

    let (_, by_body) = f.get_json(&cookie, "/api/sms?q=otp").await;
    assert_eq!(by_body["total"], 1, "search is case-insensitive");

    let (_, both) = f
        .get_json(&cookie, "/api/sms?line=line-a&q=%2B919000000001")
        .await;
    assert_eq!(both["total"], 2);
}

#[tokio::test]
async fn search_wildcards_match_literally() {
    let f = Fixture::start("").await;
    let _store = seed(&f);
    let cookie = f.login().await;

    // `%` and `_` in the search text are characters, not wildcards.
    let (_, percent) = f.get_json(&cookie, "/api/sms?q=100%25").await;
    assert_eq!(percent["total"], 1);
    let (_, underscore) = f.get_json(&cookie, "/api/sms?q=sure_thing").await;
    assert_eq!(underscore["total"], 1);
    let (_, lone) = f.get_json(&cookie, "/api/sms?q=%25").await;
    assert_eq!(
        lone["total"], 1,
        "a lone % matches only the message containing one"
    );
}

#[tokio::test]
async fn a_hostile_body_and_an_injection_attempt_are_inert() {
    let f = Fixture::start("").await;
    let _store = seed(&f);
    let cookie = f.login().await;

    let (_, all) = f.get_json(&cookie, "/api/sms").await;
    assert!(
        all["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["body"] == "<script>alert(1)</script>"),
        "the body is returned byte for byte; escaping is the page's job"
    );

    let (status, _) = f
        .get_json(&cookie, "/api/sms?q=x%27%3B%20DROP%20TABLE%20sms%3B%20--")
        .await;
    assert_eq!(status, 200);
    let (_, still) = f.get_json(&cookie, "/api/sms").await;
    assert_eq!(still["total"], 5, "the table is intact");
}

#[tokio::test]
async fn reading_changes_nothing_in_the_store() {
    let f = Fixture::start("").await;
    let _store = seed(&f);
    let before = statuses(&f);
    let cookie = f.login().await;

    for path in ["/api/sms", "/api/sms?q=OTP", "/api/sms?line=line-b&page=2"] {
        assert_eq!(f.get_json(&cookie, path).await.0, 200);
    }
    assert_eq!(statuses(&f), before);
    assert!(before.iter().any(|(_, s)| s == "failed"));
}

#[tokio::test]
async fn an_unreadable_store_fails_sms_but_not_the_overview() {
    let f =
        Fixture::start_with_db("[cs]\nenabled = false\n", Some("/nonexistent/dir/store.db")).await;
    let cookie = f.login().await;

    let (status, body) = f.get_json(&cookie, "/api/sms").await;
    assert_eq!(status, 503);
    assert_eq!(body["error"], "store_unavailable");

    let (status, _) = f.get_json(&cookie, "/api/overview").await;
    assert_eq!(status, 200, "the overview does not depend on the store");
}
