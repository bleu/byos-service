//! `/solve` is driver-only. It lives on the internal listener,
//! never on the public one, and can additionally demand a bearer token.

use {
    crate::tests::setup::{TestApp, TestDb},
    reqwest::StatusCode,
    serde_json::json,
};

/// An auction with no orders — enough for `/solve` to answer 200 with an
/// empty solution list, which is all these routing/auth assertions need.
fn empty_auction() -> serde_json::Value {
    json!({
        "tokens": {},
        "orders": [],
        "liquidity": [],
        "effectiveGasPrice": "0",
        "deadline": "2099-01-01T00:00:00Z",
        "surplusCapturingJitOrderOwners": []
    })
}

#[ignore]
#[tokio::test]
async fn solve_is_only_reachable_on_the_internal_listener() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let client = reqwest::Client::new();

    // The public listener must not know /solve at all.
    let public = client
        .post(app.url("/solve"))
        .json(&empty_auction())
        .send()
        .await
        .expect("request failed");
    assert_eq!(public.status(), StatusCode::NOT_FOUND);

    // The internal listener serves it.
    let internal = client
        .post(app.internal_url("/solve"))
        .json(&empty_auction())
        .send()
        .await
        .expect("request failed");
    assert_eq!(internal.status(), StatusCode::OK);

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn solve_bearer_token_is_enforced_end_to_end() {
    let db = TestDb::create().await;
    let app = TestApp::spawn_with_solve_bearer_token(&db.url, "driver-secret").await;
    let client = reqwest::Client::new();

    // No token → rejected before the handler runs.
    let unauthorized = client
        .post(app.internal_url("/solve"))
        .json(&empty_auction())
        .send()
        .await
        .expect("request failed");
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    // The driver's configured header gets through.
    let authorized = client
        .post(app.internal_url("/solve"))
        .header("Authorization", "Bearer driver-secret")
        .json(&empty_auction())
        .send()
        .await
        .expect("request failed");
    assert_eq!(authorized.status(), StatusCode::OK);

    app.stop().await;
}

#[ignore]
#[tokio::test]
async fn proposal_ingestion_is_not_reachable_on_the_internal_listener() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;

    let response = reqwest::Client::new()
        .post(app.internal_url("/proposals"))
        .json(&json!({}))
        .send()
        .await
        .expect("request failed");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    app.stop().await;
}
