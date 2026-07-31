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

/// Current unix timestamp in seconds.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_secs()
}

// ---------------------------------------------------------------------------
// TestDb
// ---------------------------------------------------------------------------

/// Databases older than this are fair game for the sweep. Comfortably longer
/// than any tier here takes (the db tier is ~10s, e2e minutes), so a database
/// still in use by a concurrent run is never a candidate.
const STALE_AFTER: Duration = Duration::from_secs(3 * 3600);

/// Prefix every test database shares, so the sweep can recognize its own.
const TEST_DB_PREFIX: &str = "byos_test_";

/// A uniquely-named database created for one test. Dropped by the sweep in
/// [`TestDb::create`] rather than in `Drop`, which would need blocking IO in a
/// destructor.
pub struct TestDb {
    pub url: String,
}

/// Drop test databases left by earlier runs.
///
/// Nothing dropped these before, so they accumulated one per test per run —
/// a few thousand after a day of iterating, at which point Postgres starts
/// failing to allocate shared memory and every db-tier test dies on
/// "No space left on device". The failure looks like a bug in whatever test
/// happens to run first, which is what makes it worth fixing rather than
/// cleaning up by hand.
///
/// Age comes from the timestamp already in the name. Individual failures are
/// ignored: `DROP DATABASE` refuses while connections remain, which is exactly
/// the outcome wanted if a long-running process still holds one.
async fn sweep_stale_databases(admin: &PgPool) {
    let cutoff = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .saturating_sub(STALE_AFTER)
        .as_nanos();

    let names: Vec<String> =
        match sqlx::query_scalar("SELECT datname FROM pg_database WHERE datname LIKE $1")
            .bind(format!("{TEST_DB_PREFIX}%"))
            .fetch_all(admin)
            .await
        {
            Ok(names) => names,
            // A sweep is housekeeping; never fail a test over it.
            Err(_) => return,
        };

    for name in names {
        // byos_test_<pid>_<nanos>_<counter>
        let Some(nanos) = name
            .strip_prefix(TEST_DB_PREFIX)
            .and_then(|rest| rest.split('_').nth(1))
            .and_then(|nanos| nanos.parse::<u128>().ok())
        else {
            continue;
        };
        if nanos >= cutoff {
            continue;
        }
        let _ = sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{name}""#))
            .execute(admin)
            .await;
    }
}

/// The sweep drops databases past the window and leaves the rest alone.
///
/// Without it these accumulated one per test per run until Postgres ran out of
/// shared memory and every db-tier test failed on "No space left on device" —
/// so a regression here is invisible until it takes the whole tier down.
/// Names are seeded directly rather than by creating real test databases,
/// because the point is the age filter, and a fresh `TestDb` cannot be old.
#[ignore]
#[tokio::test]
async fn the_sweep_drops_stale_test_databases_and_spares_current_ones() {
    let admin = PgPool::connect(&format!("{}/postgres", admin_url()))
        .await
        .expect("test Postgres unreachable — run `docker compose up -d postgres`");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch");
    // A pid no test process will use, so this cannot collide with a live run.
    let stale = format!(
        "{TEST_DB_PREFIX}999999_{}_0",
        (now - STALE_AFTER * 2).as_nanos()
    );
    let current = format!("{TEST_DB_PREFIX}999999_{}_1", now.as_nanos());

    for name in [&stale, &current] {
        sqlx::query(&format!(r#"CREATE DATABASE "{name}""#))
            .execute(&admin)
            .await
            .expect("seed database");
    }

    sweep_stale_databases(&admin).await;

    let survivors: Vec<String> =
        sqlx::query_scalar("SELECT datname FROM pg_database WHERE datname LIKE $1")
            .bind(format!("{TEST_DB_PREFIX}999999_%"))
            .fetch_all(&admin)
            .await
            .expect("list survivors");

    // Clean up before asserting, so a failure does not leave seeds behind.
    let _ = sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{current}""#))
        .execute(&admin)
        .await;

    assert!(
        !survivors.contains(&stale),
        "a database past the window must be dropped"
    );
    assert!(
        survivors.contains(&current),
        "a database inside the window must survive — a concurrent run may be using it"
    );
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
            "{TEST_DB_PREFIX}{}_{}_{}",
            std::process::id(),
            nanos,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );

        let admin = PgPool::connect(&format!("{}/postgres", admin_url()))
            .await
            .expect("test Postgres unreachable — run `docker compose up -d postgres`");

        // Once per process. nextest gives each test its own process, so this is
        // effectively once per test — the steady-state cost is a single indexed
        // scan of pg_database that finds nothing.
        static SWEPT: std::sync::Once = std::sync::Once::new();
        let mut sweep = false;
        SWEPT.call_once(|| sweep = true);
        if sweep {
            sweep_stale_databases(&admin).await;
        }

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
    /// Public listener (proposal CRUD).
    pub addr: SocketAddr,
    /// Internal listener (`/solve`, driver-only).
    pub internal_addr: SocketAddr,
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
        Self::spawn_with(database_url, validation_interval_secs, &[]).await
    }

    /// Spawn with a `--solve-bearer-token`, so tests can exercise the
    /// driver-auth path end to end.
    pub async fn spawn_with_solve_bearer_token(database_url: &str, token: &str) -> Self {
        Self::spawn_with(database_url, 3600, &["--solve-bearer-token", token]).await
    }

    /// Spawn with a custom retention window and sweep cadence (validation
    /// stays parked), so tests can watch the sweep delete dropped proposals.
    pub async fn spawn_with_retention(
        database_url: &str,
        dropped_retention: &str,
        sweep_interval_secs: u64,
    ) -> Self {
        Self::spawn_with(
            database_url,
            3600,
            &[
                "--dropped-retention",
                dropped_retention,
                "--retention-sweep-interval-secs",
                &sweep_interval_secs.to_string(),
            ],
        )
        .await
    }

    async fn spawn_with(
        database_url: &str,
        validation_interval_secs: u64,
        extra_args: &[&str],
    ) -> Self {
        let args = [
            "byos",
            "--public-addr",
            "127.0.0.1:0",
            "--internal-addr",
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
        .into_iter()
        .chain(extra_args.iter().copied())
        .map(String::from)
        .collect::<Vec<_>>();

        let (bind_tx, bind_rx) = oneshot::channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(crate::run_until(args, bind_tx, shutdown_rx));
        let addrs = bind_rx.await.expect("service failed to bind");

        Self {
            addr: addrs.public,
            internal_addr: addrs.internal,
            client: reqwest::Client::new(),
            shutdown: shutdown_tx,
            handle,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn internal_url(&self, path: &str) -> String {
        format!("http://{}{path}", self.internal_addr)
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
            // Inside the default 5-minute lifetime cap (ADR-0013), yet far
            // enough out that the background expiry sweep never reaps a
            // fixture mid-test.
            valid_until: U256::from(unix_now() + 240),
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
