//! Retention sweep (ADR-0013): dropped proposals disappear after
//! `--dropped-retention`, their audit history does not.

use {
    crate::tests::setup::{self, TestApp, TestDb},
    alloy::signers::local::PrivateKeySigner,
    reqwest::StatusCode,
    std::time::Duration,
};

/// With a zero retention window and a fast sweep, a cancelled proposal 404s
/// shortly after dying — while both its audit rows (received + cancelled)
/// stay queryable forever.
#[ignore]
#[tokio::test]
async fn dropped_proposal_is_swept_but_its_audit_trail_remains() {
    let db = TestDb::create().await;
    let app = TestApp::spawn_with_retention(&db.url, "0s", 1).await;
    let signer = PrivateKeySigner::random();

    let (status, body) = app
        .post_json(
            "/proposals",
            &setup::signed_proposal_body(&signer, [0xab; 56]).await,
        )
        .await;
    assert_eq!(status, 202, "{body}");
    let id = body["id"].as_u64().unwrap();

    let (status, _) = app
        .delete(
            &format!("/proposal/{id}"),
            Some(&setup::cancel_signature(&signer, id).await),
        )
        .await;
    assert_eq!(status, 204);

    // The sweep (1s cadence, 0s window) deletes the cancelled row; the
    // owner's read flips from 200 to 404.
    let read_auth = setup::read_auth_signature(&signer).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (status, _) = app
            .get_json(&format!("/proposal/{id}"), Some(&read_auth))
            .await;
        if status == StatusCode::NOT_FOUND {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "proposal {id} was never swept, still {status}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let rows = setup::wait_for_audit_rows(&db.pool().await, 2).await;
    assert_eq!(rows[0].event_type, "received");
    assert_eq!(rows[1].event_type, "cancelled");

    app.stop().await;
}
