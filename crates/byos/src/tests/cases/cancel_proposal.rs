//! `DELETE /proposal/{id}`: ownership-checked cancellation via the
//! `X-Signature` header.

use {
    crate::tests::setup::{ProposalFixture, TestServer, cancel_signature},
    alloy::signers::local::PrivateKeySigner,
    reqwest::StatusCode,
};

#[tokio::test]
async fn owner_cancel_round_trip() {
    let server = TestServer::spawn().await;
    let signer = PrivateKeySigner::random();
    let body = ProposalFixture::default()
        .signed_body(&signer, &server.domain())
        .await;
    let (_, created) = server.post_json("/proposals", &body).await;
    let id = created["id"].as_u64().expect("response must carry an id");

    let sig = cancel_signature(&signer, &server.domain(), id).await;
    let (status, _) = server.delete(&format!("/proposal/{id}"), Some(&sig)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, got) = server.get_json(&format!("/proposal/{id}")).await;
    assert_eq!(got["status"], "cancelled");

    // Cancelled proposals drop out of the active listing.
    let order_uid = body["orderUid"].as_str().unwrap();
    let (status, listed) = server.get_json(&format!("/proposals/{order_uid}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["proposals"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn non_owner_cancel_is_forbidden() {
    let server = TestServer::spawn().await;
    let owner = PrivateKeySigner::random();
    let intruder = PrivateKeySigner::random();
    let body = ProposalFixture::default()
        .signed_body(&owner, &server.domain())
        .await;
    let (_, created) = server.post_json("/proposals", &body).await;
    let id = created["id"].as_u64().expect("response must carry an id");

    let sig = cancel_signature(&intruder, &server.domain(), id).await;
    let (status, err) = server.delete(&format!("/proposal/{id}"), Some(&sig)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(err["kind"], "NotProposalOwner");

    // The proposal is untouched.
    let (_, got) = server.get_json(&format!("/proposal/{id}")).await;
    assert_eq!(got["status"], "active");
}

#[tokio::test]
async fn cancel_unknown_id_is_not_found() {
    let server = TestServer::spawn().await;
    let signer = PrivateKeySigner::random();

    let sig = cancel_signature(&signer, &server.domain(), 999).await;
    let (status, err) = server.delete("/proposal/999", Some(&sig)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err["kind"], "ProposalNotFound");
}

#[tokio::test]
async fn missing_signature_header_is_rejected() {
    let server = TestServer::spawn().await;
    let signer = PrivateKeySigner::random();
    let body = ProposalFixture::default()
        .signed_body(&signer, &server.domain())
        .await;
    let (_, created) = server.post_json("/proposals", &body).await;
    let id = created["id"].as_u64().expect("response must carry an id");

    let (status, err) = server.delete(&format!("/proposal/{id}"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["kind"], "InvalidSignature");
}
