//! Harness: per-test databases, in-process service instances, and EIP-712
//! signing fixtures mirroring what a real sub-solver client does. Tests
//! assert on raw JSON so the wire format (camelCase keys, PascalCase kinds,
//! decimal-string amounts) stays pinned to the ADR-0001 contract.

use {
    alloy::{
        primitives::{Address, U256, keccak256},
        signers::local::PrivateKeySigner,
        sol_types::Eip712Domain,
    },
    byos_common::{
        contracts::{Interaction, Proposal},
        eip712,
    },
    reqwest::StatusCode,
    serde_json::{Value, json},
    sqlx::postgres::PgPool,
    std::{
        net::SocketAddr,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, SystemTime, UNIX_EPOCH},
    },
    tokio::{sync::oneshot, task::JoinHandle},
};

/// Chain ID and factory address baked into every test instance; signing
/// helpers must use the same EIP-712 domain.
pub const CHAIN_ID: u64 = 1;
pub const TRAMPOLINE_FACTORY: Address =
    alloy::primitives::address!("00000000000000000000000000000000000000cc");

fn admin_url() -> String {
    std::env::var("BYOS_TEST_DB_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432".into())
}

fn domain() -> Eip712Domain {
    eip712::byos_domain(CHAIN_ID, TRAMPOLINE_FACTORY)
}

// ---------------------------------------------------------------------------
// TestDb
// ---------------------------------------------------------------------------

/// A uniquely-named database created for one test. Left behind on purpose —
/// the compose Postgres is ephemeral, and keeping it avoids async-drop
/// gymnastics.
pub struct TestDb {
    pub url: String,
}

impl TestDb {
    pub async fn create() -> Self {
        // PID + timestamp + counter: nextest runs each test in its own
        // process, so a timestamp alone collides when tests start within the
        // clock's resolution.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!(
            "byos_test_{}_{}_{}",
            std::process::id(),
            nanos,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );

        let admin = PgPool::connect(&format!("{}/postgres", admin_url()))
            .await
            .expect("test Postgres unreachable — run `docker compose up -d postgres`");
        sqlx::query(&format!(r#"CREATE DATABASE "{name}""#))
            .execute(&admin)
            .await
            .expect("create test database");

        Self {
            url: format!("{}/{name}", admin_url()),
        }
    }

    pub async fn pool(&self) -> PgPool {
        PgPool::connect(&self.url).await.expect("connect test db")
    }
}

// ---------------------------------------------------------------------------
// TestApp
// ---------------------------------------------------------------------------

/// One in-process service instance and an HTTP client pointed at it.
pub struct TestApp {
    pub addr: SocketAddr,
    client: reqwest::Client,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<anyhow::Result<()>>,
}

impl TestApp {
    pub async fn spawn(database_url: &str) -> Self {
        // Background validation parked far out: several tests count exact
        // audit rows or pin the `submitted` status, so ticks must not flip
        // proposals mid-test.
        Self::spawn_with_validation_interval(database_url, 3600).await
    }

    pub async fn spawn_with_validation_interval(
        database_url: &str,
        validation_interval_secs: u64,
    ) -> Self {
        let args = [
            "byos",
            "--public-addr",
            "127.0.0.1:0",
            "--chain-id",
            &CHAIN_ID.to_string(),
            "--trampoline-factory",
            &TRAMPOLINE_FACTORY.to_string(),
            "--database-url",
            database_url,
            "--validation-interval-secs",
            &validation_interval_secs.to_string(),
        ]
        .map(String::from);

        let (bind_tx, bind_rx) = oneshot::channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(crate::run_until(args, bind_tx, shutdown_rx));
        let addr = bind_rx.await.expect("service failed to bind");

        Self {
            addr,
            client: reqwest::Client::new(),
            shutdown: shutdown_tx,
            handle,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// POST a JSON body; returns status and response JSON.
    pub async fn post_json(&self, path: &str, body: &Value) -> (StatusCode, Value) {
        let resp = self
            .client
            .post(self.url(path))
            .json(body)
            .send()
            .await
            .expect("request failed");
        json_of(resp).await
    }

    /// GET a path, optionally with an `X-Signature` `ReadAuth` bearer token
    /// (ADR-0011); returns status and response JSON.
    pub async fn get_json(&self, path: &str, signature: Option<&str>) -> (StatusCode, Value) {
        let mut req = self.client.get(self.url(path));
        if let Some(sig) = signature {
            req = req.header("X-Signature", sig);
        }
        let resp = req.send().await.expect("request failed");
        json_of(resp).await
    }

    /// DELETE a path, optionally with an `X-Signature` header; returns status
    /// and response JSON (`Null` for empty bodies, e.g. 204).
    pub async fn delete(&self, path: &str, signature: Option<&str>) -> (StatusCode, Value) {
        let mut req = self.client.delete(self.url(path));
        if let Some(sig) = signature {
            req = req.header("X-Signature", sig);
        }
        let resp = req.send().await.expect("request failed");
        json_of(resp).await
    }

    /// Graceful shutdown; returns only after the audit writer has flushed.
    pub async fn stop(self) {
        let _ = self.shutdown.send(());
        self.handle
            .await
            .expect("service task panicked")
            .expect("service exited with error");
    }
}

async fn json_of(resp: reqwest::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let text = resp.text().await.expect("failed to read body");
    let json = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, json)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A signable proposal. Tests tweak fields, then render a request body with
/// [`ProposalFixture::signed_body`] (or
/// [`ProposalFixture::body_with_signature`] to send tampered/malformed
/// signatures).
pub struct ProposalFixture {
    pub order_uid: [u8; 56],
    pub sell_amount: U256,
    pub buy_amount: U256,
    pub sell_token: Address,
    pub buy_token: Address,
    pub valid_until: U256,
    pub nonce: U256,
    pub interactions: Vec<Interaction>,
}

impl Default for ProposalFixture {
    fn default() -> Self {
        Self {
            order_uid: [0xab; 56],
            sell_amount: U256::from(1_000_000u64),
            buy_amount: U256::from(990_000u64),
            sell_token: alloy::primitives::address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            buy_token: alloy::primitives::address!("6B175474E89094C44Da98b954EedeAC495271d0F"),
            // Far future: the background expiry sweep must never reap a
            // fixture mid-test.
            valid_until: U256::from(u32::MAX),
            nonce: U256::from(1u64),
            interactions: vec![Interaction {
                target: alloy::primitives::address!("00000000000000000000000000000000000000dd"),
                value: U256::ZERO,
                callData: vec![0xde, 0xad].into(),
            }],
        }
    }
}

impl ProposalFixture {
    /// The on-chain [`Proposal`] struct this fixture signs over.
    fn as_proposal(&self) -> Proposal {
        Proposal {
            orderUidHash: keccak256(self.order_uid),
            sellAmount: self.sell_amount,
            buyAmount: self.buy_amount,
            validUntil: self.valid_until,
            nonce: self.nonce,
        }
    }

    /// Signs the fixture and renders the `POST /proposals` JSON body.
    pub async fn signed_body(&self, signer: &PrivateKeySigner) -> Value {
        let sig = eip712::sign_proposal(signer, &domain(), &self.as_proposal(), &self.interactions)
            .await
            .expect("signing should succeed");
        self.body_with_signature(&alloy::hex::encode_prefixed(sig.as_bytes()))
    }

    /// Renders the JSON body with an arbitrary signature string.
    pub fn body_with_signature(&self, signature: &str) -> Value {
        json!({
            "orderUid": alloy::hex::encode_prefixed(self.order_uid),
            "sellAmount": self.sell_amount.to_string(),
            "buyAmount": self.buy_amount.to_string(),
            "sellToken": self.sell_token,
            "buyToken": self.buy_token,
            "interactions": self.interactions.iter().map(|i| json!({
                "target": i.target.to_string(),
                "value": i.value.to_string(),
                "callData": alloy::hex::encode_prefixed(&i.callData),
            })).collect::<Vec<_>>(),
            "validUntil": self.valid_until.to_string(),
            "nonce": self.nonce.to_string(),
            "signature": signature,
        })
    }
}

/// Build a validly-signed POST /proposals body, the way a sub-solver would.
pub async fn signed_proposal_body(signer: &PrivateKeySigner, order_uid: [u8; 56]) -> Value {
    ProposalFixture {
        order_uid,
        ..Default::default()
    }
    .signed_body(signer)
    .await
}

/// Sign the `CancelProposal` message for DELETE's `X-Signature` header.
pub async fn cancel_signature(signer: &PrivateKeySigner, proposal_id: u64) -> String {
    let sig = eip712::sign_cancellation(signer, &domain(), U256::from(proposal_id))
        .await
        .expect("signing should succeed");
    alloy::hex::encode_prefixed(sig.as_bytes())
}

/// Sign the `ReadAuth` bearer message for GET's `X-Signature` header
/// (ADR-0011).
pub async fn read_auth_signature(signer: &PrivateKeySigner) -> String {
    let sig = eip712::sign_read_auth(signer, &domain())
        .await
        .expect("signing should succeed");
    alloy::hex::encode_prefixed(sig.as_bytes())
}

// ---------------------------------------------------------------------------
// Polling
// ---------------------------------------------------------------------------

/// Poll `GET /proposal/{id}` until its status matches `want` (background
/// validation is async).
pub async fn wait_for_status(app: &TestApp, id: u64, read_auth: &str, want: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (status, got) = app
            .get_json(&format!("/proposal/{id}"), Some(read_auth))
            .await;
        assert_eq!(status, StatusCode::OK);
        if got["status"] == want {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for proposal {id} to become {want}, still {}",
            got["status"]
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll until `audit_events` holds `expected` rows (write-behind is async).
pub async fn wait_for_audit_rows(pool: &PgPool, expected: usize) -> Vec<AuditRow> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let rows: Vec<AuditRow> = sqlx::query_as(
            "SELECT proposal_id, event_type, sub_solver, order_uid, settlement_tx_hash, payload \
             FROM audit_events ORDER BY id",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        if rows.len() >= expected {
            return rows;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {expected} audit rows, have {}",
            rows.len()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[derive(sqlx::FromRow)]
pub struct AuditRow {
    pub proposal_id: i64,
    pub event_type: String,
    pub sub_solver: String,
    pub order_uid: String,
    pub settlement_tx_hash: Option<String>,
    pub payload: serde_json::Value,
}
