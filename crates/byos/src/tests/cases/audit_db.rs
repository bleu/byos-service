//! Write-behind audit trail against a real Postgres (COW-1172).

use {
    crate::tests::setup::{self, TestApp, TestDb},
    alloy::signers::local::PrivateKeySigner,
};

/// The full pipeline: handler → store → channel → writer → Postgres. A signed
/// POST and DELETE must each leave one durable evidence row.
#[ignore]
#[tokio::test]
async fn audit_db_write_behind_round_trip() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let signer = PrivateKeySigner::random();
    let order_uid = [0xab; 56];
    let client = reqwest::Client::new();

    let resp = client
        .post(app.url("/proposals"))
        .json(&setup::signed_proposal_body(&signer, order_uid).await)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let id = resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_u64()
        .unwrap();

    let resp = client
        .delete(app.url(&format!("/proposal/{id}")))
        .header("X-Signature", setup::cancel_signature(&signer, id).await)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    let rows = setup::wait_for_audit_rows(&db.pool().await, 2).await;
    let expected_solver = format!("{:#x}", signer.address());
    let expected_uid = alloy::hex::encode_prefixed(order_uid);

    let received = &rows[0];
    assert_eq!(received.event_type, "received");
    assert_eq!(received.proposal_id, i64::try_from(id).unwrap());
    assert_eq!(received.sub_solver, expected_solver);
    assert_eq!(received.order_uid, expected_uid);
    assert_eq!(received.settlement_tx_hash, None);
    assert_eq!(received.payload["sellAmount"], "1000000");
    assert_eq!(received.payload["orderUid"], expected_uid);
    assert!(
        received.payload["signature"]
            .as_str()
            .is_some_and(|s| s.len() == 2 + 65 * 2),
        "received payload must carry the EIP-712 signature as evidence"
    );

    let cancelled = &rows[1];
    assert_eq!(cancelled.event_type, "cancelled");
    assert_eq!(cancelled.proposal_id, received.proposal_id);
    assert_eq!(cancelled.sub_solver, expected_solver);
    assert_eq!(cancelled.payload, serde_json::json!({}));

    app.stop().await;
}

/// Background validator verdicts are evidence too: with a fast tick, the
/// AcceptAll stub flips the submitted proposal to Active and a `validated`
/// row lands next to the `received` one.
#[ignore]
#[tokio::test]
async fn audit_db_validator_verdict_leaves_evidence() {
    let db = TestDb::create().await;
    let app = TestApp::spawn_with_validation_interval(&db.url, 1).await;
    let signer = PrivateKeySigner::random();
    let client = reqwest::Client::new();

    let resp = client
        .post(app.url("/proposals"))
        .json(&setup::signed_proposal_body(&signer, [0xcd; 56]).await)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let rows = setup::wait_for_audit_rows(&db.pool().await, 2).await;
    assert_eq!(rows[0].event_type, "received");
    let validated = &rows[1];
    assert_eq!(validated.event_type, "validated");
    assert_eq!(validated.proposal_id, rows[0].proposal_id);
    assert_eq!(validated.payload["from"], "submitted");
    assert_eq!(validated.payload["to"], "active");

    app.stop().await;
}

/// Proposal IDs must stay unique across restarts — the audit trail is the ID
/// authority. Otherwise a `cancelled` event for id 42 could attach to the
/// wrong `received` row months later, in a dispute.
#[ignore]
#[tokio::test]
async fn audit_db_ids_continue_across_restart() {
    let db = TestDb::create().await;
    let signer = PrivateKeySigner::random();
    let client = reqwest::Client::new();

    let post = async |app: &TestApp, uid: [u8; 56]| {
        let resp = client
            .post(app.url("/proposals"))
            .json(&setup::signed_proposal_body(&signer, uid).await)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        resp.json::<serde_json::Value>().await.unwrap()["id"]
            .as_u64()
            .unwrap()
    };

    let app = TestApp::spawn(&db.url).await;
    let first = post(&app, [0x01; 56]).await;
    app.stop().await;

    let app = TestApp::spawn(&db.url).await;
    let second = post(&app, [0x02; 56]).await;
    app.stop().await;

    assert!(
        second > first,
        "restarted service reissued proposal id {second} (first run reached {first})"
    );
}

/// Graceful shutdown must flush the write-behind queue: every event emitted
/// before stop() is durable by the time stop() returns, with no polling.
#[ignore]
#[tokio::test]
async fn audit_db_shutdown_drains_queued_events() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let signer = PrivateKeySigner::random();
    let client = reqwest::Client::new();

    const BURST: i64 = 25;
    for i in 0..BURST {
        let mut uid = [0u8; 56];
        uid[0] = u8::try_from(i).unwrap();
        let resp = client
            .post(app.url("/proposals"))
            .json(&setup::signed_proposal_body(&signer, uid).await)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
    }

    app.stop().await;

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events")
        .fetch_one(&db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, BURST, "shutdown lost queued audit events");
}

/// A failing insert must not lose evidence or take the service down: the
/// writer retries with backoff while the API keeps serving, and the event
/// lands once the database recovers. Renaming the table away produces the
/// same insert failure as an outage, deterministically.
#[ignore]
#[tokio::test]
async fn audit_db_writer_retries_until_database_recovers() {
    let db = TestDb::create().await;
    let app = TestApp::spawn(&db.url).await;
    let signer = PrivateKeySigner::random();
    let client = reqwest::Client::new();
    let pool = db.pool().await;

    sqlx::query("ALTER TABLE audit_events RENAME TO audit_events_hidden")
        .execute(&pool)
        .await
        .unwrap();

    let resp = client
        .post(app.url("/proposals"))
        .json(&setup::signed_proposal_body(&signer, [0xab; 56]).await)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        202,
        "audit outage must not block the hot path"
    );

    // Give the writer time to attempt (and fail) the insert a few times.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events_hidden")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "event landed while the table was hidden?");
    assert_eq!(
        client
            .get(app.url("/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        200,
        "service must stay up during an audit outage"
    );

    sqlx::query("ALTER TABLE audit_events_hidden RENAME TO audit_events")
        .execute(&pool)
        .await
        .unwrap();

    let rows = setup::wait_for_audit_rows(&pool, 1).await;
    assert_eq!(rows[0].event_type, "received");

    app.stop().await;
}
