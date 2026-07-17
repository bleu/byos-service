//! `GET /proposals/{order_uid}` and `GET /proposals/by-solver/{address}`:
//! listing filters and the metadata-only response shape (ADR-0001).

use {
    crate::tests::setup::{ProposalFixture, TestServer, hex_bytes},
    alloy::{primitives::U256, signers::local::PrivateKeySigner},
    reqwest::StatusCode,
    std::collections::BTreeSet,
};

#[tokio::test]
async fn lists_all_proposals_for_an_order_metadata_only() {
    let server = TestServer::spawn().await;
    let a = PrivateKeySigner::random();
    let b = PrivateKeySigner::random();

    // Two proposals for the same order, from different sub-solvers.
    let fixture = ProposalFixture::default();
    let body_a = fixture.signed_body(&a, &server.domain()).await;
    let fixture_b = ProposalFixture {
        nonce: U256::from(2u64),
        ..Default::default()
    };
    let body_b = fixture_b.signed_body(&b, &server.domain()).await;
    server.post_json("/proposals", &body_a).await;
    server.post_json("/proposals", &body_b).await;

    let order_uid = body_a["orderUid"].as_str().unwrap();
    let (status, listed) = server.get_json(&format!("/proposals/{order_uid}")).await;
    assert_eq!(status, StatusCode::OK);

    let proposals = listed["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 2);

    // Metadata only (ADR-0001): no interactions, amounts, or signature leak.
    let expected: BTreeSet<&str> = ["id", "subSolver", "validUntil", "status"].into();
    for p in proposals {
        let keys: BTreeSet<&str> = p.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, expected, "listing must expose metadata fields only");
    }
}

#[tokio::test]
async fn lists_by_sub_solver_filters_correctly() {
    let server = TestServer::spawn().await;
    let a = PrivateKeySigner::random();
    let b = PrivateKeySigner::random();

    // Two proposals from `a` (distinct orders), one from `b`.
    let first = ProposalFixture::default();
    let second = ProposalFixture {
        order_uid: [0xcd; 56],
        ..Default::default()
    };
    for (fixture, signer) in [(&first, &a), (&second, &a), (&first, &b)] {
        let body = fixture.signed_body(signer, &server.domain()).await;
        let (status, _) = server.post_json("/proposals", &body).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let (status, listed) = server
        .get_json(&format!("/proposals/by-solver/{}", a.address()))
        .await;
    assert_eq!(status, StatusCode::OK);

    let proposals = listed["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 2);
    for p in proposals {
        let solver: alloy::primitives::Address = p["subSolver"].as_str().unwrap().parse().unwrap();
        assert_eq!(solver, a.address());
    }
}

#[tokio::test]
async fn unknown_order_uid_returns_empty_list() {
    let server = TestServer::spawn().await;

    let unknown = hex_bytes(&[0xefu8; 56]);
    let (status, listed) = server.get_json(&format!("/proposals/{unknown}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["proposals"].as_array().unwrap().len(), 0);
}
