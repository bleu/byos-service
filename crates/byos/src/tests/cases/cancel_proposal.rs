//! `DELETE /proposal/{id}`: ownership-checked cancellation via the
//! `X-Signature` header.

use {
    crate::tests::setup::{self, ProposalFixture, TestApp, TestDb},
    alloy::signers::local::PrivateKeySigner,
    reqwest::StatusCode,
};

#[ignore]
#[tokio::test]
async fn owner_cancel_round_trip() {
    let db = TestDb::create().await;
    // Fast validation tick: the order listing only shows *active* proposals,
    // so the cancel must happen from Active for the listing check to bite.
    let app = TestApp::spawn_with_validation_interval(&db.url, 1).await;
    let signer = PrivateKeySigner::random();
    let body = ProposalFixture::default().signed_body(&signer).await;
    let (_, created) = app.post_json("/proposals", &body).await;
    let id = created["id"].as_u64().expect("response must carry an id");

    let auth = setup::read_auth_signature(&signer).await;
    setup::wait_for_status(&app, id, &auth, "active").await;

    let sig = setup::cancel_signature(&signer, id).await;
    let (status, _) = app.delete(&format!("/proposal/{id}"), Some(&sig)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, got) = app.get_json(&format!("/proposal/{id}"), Some(&auth)).await;
    assert_eq!(got["status"], "cancelled");

    // Cancelled proposals drop out of the active listing.
    let order_uid = body["orderUid"].as_str().unwrap();
    let (status, listed) = app
        .get_json(&format!("/proposals/{order_uid}"), Some(&auth))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["proposals"].as_array().unwrap().len(), 0);

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn non_owner_cancel_is_forbidden() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let owner = PrivateKeySigner::random();
    let intruder = PrivateKeySigner::random();
    let body = ProposalFixture::default().signed_body(&owner).await;
    let (_, created) = app.post_json("/proposals", &body).await;
    let id = created["id"].as_u64().expect("response must carry an id");

    let sig = setup::cancel_signature(&intruder, id).await;
    let (status, err) = app.delete(&format!("/proposal/{id}"), Some(&sig)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(err["kind"], "NotProposalOwner");

    // The proposal is untouched (still awaiting the parked validator).
    let auth = setup::read_auth_signature(&owner).await;
    let (_, got) = app.get_json(&format!("/proposal/{id}"), Some(&auth)).await;
    assert_eq!(got["status"], "submitted");

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn double_cancel_is_a_conflict() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let signer = PrivateKeySigner::random();
    let body = ProposalFixture::default().signed_body(&signer).await;
    let (_, created) = app.post_json("/proposals", &body).await;
    let id = created["id"].as_u64().expect("response must carry an id");

    let sig = setup::cancel_signature(&signer, id).await;
    let (status, _) = app.delete(&format!("/proposal/{id}"), Some(&sig)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Cancelled is terminal: a second cancel is a stale transition.
    let (status, err) = app.delete(&format!("/proposal/{id}"), Some(&sig)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(err["kind"], "ProposalNotCancellable");

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn cancel_unknown_id_is_not_found() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let signer = PrivateKeySigner::random();

    let sig = setup::cancel_signature(&signer, 999).await;
    let (status, err) = app.delete("/proposal/999", Some(&sig)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err["kind"], "ProposalNotFound");

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn missing_signature_header_is_rejected() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let signer = PrivateKeySigner::random();
    let body = ProposalFixture::default().signed_body(&signer).await;
    let (_, created) = app.post_json("/proposals", &body).await;
    let id = created["id"].as_u64().expect("response must carry an id");

    let (status, err) = app.delete(&format!("/proposal/{id}"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["kind"], "InvalidSignature");

    app.stop().await;
}
