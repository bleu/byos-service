//! `POST /proposals`: round-trip through `GET /proposal/{id}`, field
//! validation rejections, and signature handling.

use {
    crate::tests::setup::{ProposalFixture, TestServer},
    alloy::{primitives::Address, signers::local::PrivateKeySigner},
    reqwest::StatusCode,
};

#[tokio::test]
async fn create_and_get_round_trip() {
    let server = TestServer::spawn().await;
    let signer = PrivateKeySigner::random();
    let fixture = ProposalFixture::default();

    let body = fixture.signed_body(&signer, &server.domain()).await;
    let (status, created) = server.post_json("/proposals", &body).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_u64().expect("response must carry an id");

    let (status, got) = server.get_json(&format!("/proposal/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["id"].as_u64(), Some(id));
    let sub_solver: Address = got["subSolver"]
        .as_str()
        .expect("subSolver must be a string")
        .parse()
        .expect("subSolver must be an address");
    assert_eq!(sub_solver, signer.address());
    assert_eq!(got["orderUid"], body["orderUid"]);
    assert_eq!(got["sellAmount"], "1000000");
    assert_eq!(got["buyAmount"], "990000");
    assert_eq!(got["validUntil"], "1700000000");
    assert_eq!(got["status"], "active");
}

#[tokio::test]
async fn rejects_malformed_order_uid_hex() {
    let server = TestServer::spawn().await;
    let signer = PrivateKeySigner::random();
    let mut body = ProposalFixture::default()
        .signed_body(&signer, &server.domain())
        .await;
    body["orderUid"] = "0xnothex".into();

    let (status, err) = server.post_json("/proposals", &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["kind"], "BadRequest");
}

#[tokio::test]
async fn rejects_wrong_length_order_uid() {
    let server = TestServer::spawn().await;
    let signer = PrivateKeySigner::random();
    let mut body = ProposalFixture::default()
        .signed_body(&signer, &server.domain())
        .await;
    // 55 bytes instead of 56.
    body["orderUid"] = format!("0x{}", "ab".repeat(55)).into();

    let (status, err) = server.post_json("/proposals", &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["kind"], "BadRequest");
}

#[tokio::test]
async fn rejects_non_decimal_sell_amount() {
    let server = TestServer::spawn().await;
    let signer = PrivateKeySigner::random();
    let mut body = ProposalFixture::default()
        .signed_body(&signer, &server.domain())
        .await;
    body["sellAmount"] = "0x1000".into();

    let (status, err) = server.post_json("/proposals", &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["kind"], "BadRequest");
}

#[tokio::test]
async fn rejects_malformed_signature_bytes() {
    let server = TestServer::spawn().await;
    // Valid hex, but not 65 signature bytes.
    let body = ProposalFixture::default().body_with_signature("0x1234");

    let (status, err) = server.post_json("/proposals", &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["kind"], "InvalidSignature");
}

#[tokio::test]
async fn tampered_body_is_accepted_under_a_different_sub_solver() {
    // A well-formed signature over *different* data still recovers — just to
    // some other address, which the handler stores as the sub-solver. That is
    // the intended M1 contract: the escrow balance check (COW-1162) is the
    // layer that rejects proposals whose recovered address has no deposit.
    // When COW-1162 lands, flip this test to expect that rejection.
    let server = TestServer::spawn().await;
    let signer = PrivateKeySigner::random();
    let mut body = ProposalFixture::default()
        .signed_body(&signer, &server.domain())
        .await;
    body["sellAmount"] = "2000000".into(); // not what was signed

    let (status, created) = server.post_json("/proposals", &body).await;
    assert_eq!(status, StatusCode::CREATED);

    let id = created["id"].as_u64().expect("response must carry an id");
    let (_, got) = server.get_json(&format!("/proposal/{id}")).await;
    let recovered: Address = got["subSolver"].as_str().unwrap().parse().unwrap();
    assert_ne!(
        recovered,
        signer.address(),
        "tampered payload must not recover to the real signer"
    );
}
