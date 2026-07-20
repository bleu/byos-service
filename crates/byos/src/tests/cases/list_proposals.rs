//! `GET /proposals/{order_uid}` and `GET /proposals/by-solver`: owner-scoped
//! listing (ADR-0011) and the metadata-only response shape (ADR-0001).

use {
    crate::tests::setup::{self, ProposalFixture, TestApp, TestDb},
    alloy::{primitives::U256, signers::local::PrivateKeySigner},
    reqwest::StatusCode,
    std::collections::BTreeSet,
};

#[ignore]
#[tokio::test]
async fn order_listing_is_owner_scoped_and_metadata_only() {
    let db = TestDb::create().await;
    // Fast validation tick: the order listing only shows active proposals.
    let app = TestApp::spawn_with_validation_interval(&db.url, 1).await;
    let a = PrivateKeySigner::random();
    let b = PrivateKeySigner::random();

    // Two proposals for the same order, from different sub-solvers.
    let fixture = ProposalFixture::default();
    let body_a = fixture.signed_body(&a).await;
    let fixture_b = ProposalFixture {
        nonce: U256::from(2u64),
        ..Default::default()
    };
    let body_b = fixture_b.signed_body(&b).await;
    let (_, created_a) = app.post_json("/proposals", &body_a).await;
    let (_, created_b) = app.post_json("/proposals", &body_b).await;

    let auth_a = setup::read_auth_signature(&a).await;
    let auth_b = setup::read_auth_signature(&b).await;
    let id_a = created_a["id"].as_u64().unwrap();
    let id_b = created_b["id"].as_u64().unwrap();
    setup::wait_for_status(&app, id_a, &auth_a, "active").await;
    setup::wait_for_status(&app, id_b, &auth_b, "active").await;

    // Owner-scoped reads (ADR-0011): `a` sees only its own proposal, never
    // the competitor's on the same order.
    let order_uid = body_a["orderUid"].as_str().unwrap();
    let (status, listed) = app
        .get_json(&format!("/proposals/{order_uid}"), Some(&auth_a))
        .await;
    assert_eq!(status, StatusCode::OK);

    let proposals = listed["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0]["id"].as_u64(), Some(id_a));

    // Metadata only (ADR-0001): no interactions, amounts, or signature leak.
    let expected: BTreeSet<&str> = ["id", "subSolver", "validUntil", "status"].into();
    let keys: BTreeSet<&str> = proposals[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, expected, "listing must expose metadata fields only");

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn by_solver_listing_uses_the_signature_identity() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let a = PrivateKeySigner::random();
    let b = PrivateKeySigner::random();

    // Two proposals from `a` (distinct orders), one from `b`. Submitted
    // proposals count here — the by-solver listing shows submitted + active.
    let first = ProposalFixture::default();
    let second = ProposalFixture {
        order_uid: [0xcd; 56],
        ..Default::default()
    };
    for (fixture, signer) in [(&first, &a), (&second, &a), (&first, &b)] {
        let body = fixture.signed_body(signer).await;
        let (status, _) = app.post_json("/proposals", &body).await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    // No address parameter: the caller's identity comes entirely from the
    // ReadAuth signature (ADR-0011).
    let auth_a = setup::read_auth_signature(&a).await;
    let (status, listed) = app.get_json("/proposals/by-solver", Some(&auth_a)).await;
    assert_eq!(status, StatusCode::OK);

    let proposals = listed["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 2);
    for p in proposals {
        let solver: alloy::primitives::Address = p["subSolver"].as_str().unwrap().parse().unwrap();
        assert_eq!(solver, a.address());
    }

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn unknown_order_uid_returns_empty_list() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let signer = PrivateKeySigner::random();

    let auth = setup::read_auth_signature(&signer).await;
    let unknown = alloy::hex::encode_prefixed([0xef; 56]);
    let (status, listed) = app
        .get_json(&format!("/proposals/{unknown}"), Some(&auth))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["proposals"].as_array().unwrap().len(), 0);

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn listing_without_read_auth_is_rejected() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;

    let unknown = alloy::hex::encode_prefixed([0xef; 56]);
    let (status, err) = app.get_json(&format!("/proposals/{unknown}"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["kind"], "InvalidSignature");

    app.stop().await;
}
