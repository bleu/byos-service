//! Restarts are not lossy (ADR-0013): proposals live in Postgres, so live
//! submissions survive a service restart and the next validator tick picks
//! them up — sub-solver resubmission is no longer part of the design.

use {
    crate::tests::setup::{self, TestApp, TestDb},
    alloy::signers::local::PrivateKeySigner,
};

/// A `Submitted` proposal survives a restart: the first instance never
/// validates it (parked interval), the second instance's validator flips it
/// to `Active` without any resubmission.
#[ignore]
#[tokio::test]
async fn live_proposal_survives_restart_and_is_validated() {
    let db = TestDb::create().await;
    let signer = PrivateKeySigner::random();

    let app = TestApp::spawn(&db.url).await;
    let (status, body) = app
        .post_json(
            "/proposals",
            &setup::signed_proposal_body(&signer, [0xab; 56]).await,
        )
        .await;
    assert_eq!(status, 202, "{body}");
    let id = body["id"].as_u64().unwrap();

    let read_auth = setup::read_auth_signature(&signer).await;
    let (status, body) = app
        .get_json(&format!("/proposal/{id}"), Some(&read_auth))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "submitted", "validation is parked");

    app.stop().await;

    // Same database, fast validator: the restarted service must see the
    // stored proposal and validate it.
    let app = TestApp::spawn_with_validation_interval(&db.url, 1).await;
    setup::wait_for_status(&app, id, &read_auth, "active").await;

    app.stop().await;
}
