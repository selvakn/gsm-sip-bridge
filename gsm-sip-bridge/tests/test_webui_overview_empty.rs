// specs/122-management-webui, US1 edge case: with no lines at all the
// Overview says so instead of showing an empty page. A separate binary,
// because agent reports live in process-wide state and the other overview
// tests add lines to it.

mod common;

use common::Fixture;

#[tokio::test]
async fn no_agents_and_cs_disabled_gives_the_no_lines_note() {
    let f = Fixture::start("[cs]\nenabled = false\n").await;
    let cookie = f.login().await;
    let (status, overview) = f.get_json(&cookie, "/api/overview").await;
    assert_eq!(status, 200);
    assert_eq!(overview["lines"].as_array().unwrap().len(), 0);
    assert_eq!(overview["discovery_note"], "no_lines");
}
