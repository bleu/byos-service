//! `POST /proposals`: round-trip through `GET /proposal/{id}`, field
//! validation rejections, and signature handling.

use {
    crate::tests::setup::{self, ProposalFixture, TestApp, TestDb},
    alloy::{primitives::Address, signers::local::PrivateKeySigner},
    reqwest::StatusCode,
};

#[ignore]
#[tokio::test]
async fn create_and_get_round_trip() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let signer = PrivateKeySigner::random();
    let fixture = ProposalFixture::default();

    let body = fixture.signed_body(&signer).await;
    let (status, created) = app.post_json("/proposals", &body).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = created["id"].as_u64().expect("response must carry an id");

    // Reads are signature-gated (ADR-0011); the owner's ReadAuth token
    // unlocks the proposal.
    let auth = setup::read_auth_signature(&signer).await;
    let (status, got) = app.get_json(&format!("/proposal/{id}"), Some(&auth)).await;
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
    assert_eq!(got["validUntil"], u32::MAX.to_string());
    // Ingestion is async: POST stores the proposal as `submitted`; the
    // background validator (parked far out here) is what flips it to active.
    assert_eq!(got["status"], "submitted");

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn get_without_read_auth_is_rejected() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let signer = PrivateKeySigner::random();

    let body = ProposalFixture::default().signed_body(&signer).await;
    let (_, created) = app.post_json("/proposals", &body).await;
    let id = created["id"].as_u64().expect("response must carry an id");

    let (status, err) = app.get_json(&format!("/proposal/{id}"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["kind"], "InvalidSignature");

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn non_owner_get_is_not_found() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let owner = PrivateKeySigner::random();
    let intruder = PrivateKeySigner::random();

    let body = ProposalFixture::default().signed_body(&owner).await;
    let (_, created) = app.post_json("/proposals", &body).await;
    let id = created["id"].as_u64().expect("response must carry an id");

    // Non-owners get the same 404 as a genuine miss so proposal IDs cannot
    // be probed for existence (ADR-0011).
    let auth = setup::read_auth_signature(&intruder).await;
    let (status, err) = app.get_json(&format!("/proposal/{id}"), Some(&auth)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err["kind"], "ProposalNotFound");

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn rejects_malformed_order_uid_hex() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let signer = PrivateKeySigner::random();
    let mut body = ProposalFixture::default().signed_body(&signer).await;
    body["orderUid"] = "0xnothex".into();

    let (status, err) = app.post_json("/proposals", &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["kind"], "BadRequest");

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn rejects_wrong_length_order_uid() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let signer = PrivateKeySigner::random();
    let mut body = ProposalFixture::default().signed_body(&signer).await;
    // 55 bytes instead of 56.
    body["orderUid"] = format!("0x{}", "ab".repeat(55)).into();

    let (status, err) = app.post_json("/proposals", &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["kind"], "BadRequest");

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn rejects_non_decimal_sell_amount() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let signer = PrivateKeySigner::random();
    let mut body = ProposalFixture::default().signed_body(&signer).await;
    body["sellAmount"] = "0x1000".into();

    let (status, _err) = app.post_json("/proposals", &body).await;
    // Serde rejects the non-decimal string at deserialization time; axum
    // returns 422 Unprocessable Entity for malformed request bodies.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn rejects_malformed_signature_bytes() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    // Valid hex, but not 65 signature bytes.
    let body = ProposalFixture::default().body_with_signature("0x1234");

    let (status, err) = app.post_json("/proposals", &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["kind"], "InvalidSignature");

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn tampered_body_is_accepted_under_a_different_sub_solver() {
    // A well-formed signature over *different* data still recovers — just to
    // some other address, which the handler stores as the sub-solver. That is
    // the intended M1 contract: the escrow balance check (COW-1162) is the
    // layer that rejects proposals whose recovered address has no deposit.
    // When COW-1162 lands, flip this test to expect that rejection.
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let signer = PrivateKeySigner::random();
    let mut body = ProposalFixture::default().signed_body(&signer).await;
    body["sellAmount"] = "2000000".into(); // not what was signed

    let (status, created) = app.post_json("/proposals", &body).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = created["id"].as_u64().expect("response must carry an id");

    // Owner-scoped reads (ADR-0011) make the proposal invisible to the real
    // signer: it was stored under the garbage recovered address, not theirs.
    let auth = setup::read_auth_signature(&signer).await;
    let (status, err) = app.get_json(&format!("/proposal/{id}"), Some(&auth)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        err["kind"], "ProposalNotFound",
        "tampered payload must not be readable as the real signer"
    );

    app.stop().await;
}
