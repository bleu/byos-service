//! Harness: per-test databases, in-process service instances, and EIP-712
//! signing helpers mirroring what a real sub-solver client does.

use {
    alloy::{
        primitives::{Address, B256, U256},
        signers::{SignerSync, local::PrivateKeySigner},
        sol_types::SolStruct,
    },
    byos_common::{
        contracts::Interaction,
        eip712::{self, CancelProposal},
    },
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

/// One in-process service instance.
pub struct TestApp {
    pub addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<anyhow::Result<()>>,
}

impl TestApp {
    pub async fn spawn(database_url: &str) -> Self {
        // Background validation parked far out: several tests count exact
        // audit rows, so ticks must not inject verdict events mid-test.
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
            shutdown: shutdown_tx,
            handle,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
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

fn domain() -> alloy::sol_types::Eip712Domain {
    eip712::byos_domain(CHAIN_ID, TRAMPOLINE_FACTORY)
}

/// Build a validly-signed POST /proposals body, the way a sub-solver would.
pub fn signed_proposal_body(signer: &PrivateKeySigner, order_uid: [u8; 56]) -> serde_json::Value {
    let interactions = vec![Interaction {
        target: alloy::primitives::address!("00000000000000000000000000000000000000dd"),
        value: U256::ZERO,
        callData: vec![0xde, 0xad].into(),
    }];
    let sell_amount = U256::from(1_000_000u64);
    let buy_amount = U256::from(990_000u64);
    let valid_until = U256::from(u32::MAX);
    let nonce = U256::from(1u64);

    let interactions_hash = eip712::compute_interactions_hash(&interactions);
    let proposal = byos_common::contracts::Proposal {
        orderUidHash: alloy::primitives::keccak256(order_uid),
        sellAmount: sell_amount,
        buyAmount: buy_amount,
        validUntil: valid_until,
        nonce,
    };
    let signing_hash: B256 =
        eip712::proposal_data(&proposal, interactions_hash).eip712_signing_hash(&domain());
    let signature = signer.sign_hash_sync(&signing_hash).unwrap();

    serde_json::json!({
        "orderUid": alloy::hex::encode_prefixed(order_uid),
        "sellAmount": sell_amount.to_string(),
        "buyAmount": buy_amount.to_string(),
        "interactions": [{
            "target": interactions[0].target,
            "value": interactions[0].value.to_string(),
            "callData": alloy::hex::encode_prefixed(&interactions[0].callData),
        }],
        "validUntil": valid_until.to_string(),
        "nonce": nonce.to_string(),
        "signature": alloy::hex::encode_prefixed(signature.as_bytes()),
    })
}

/// Sign the `CancelProposal` message for DELETE's `X-Signature` header.
pub fn cancel_signature_hex(signer: &PrivateKeySigner, proposal_id: u64) -> String {
    let cancel = CancelProposal {
        proposalId: U256::from(proposal_id),
    };
    let signature = signer
        .sign_hash_sync(&cancel.eip712_signing_hash(&domain()))
        .unwrap();
    alloy::hex::encode_prefixed(signature.as_bytes())
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
