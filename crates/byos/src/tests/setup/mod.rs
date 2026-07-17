//! Harness: spawns the real server on an ephemeral port via `run()`'s bind
//! channel, plus signed-request fixtures. Tests assert on raw JSON so the
//! wire format (camelCase keys, PascalCase kinds, decimal-string amounts)
//! stays pinned to the ADR-0001 contract.

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
};

/// Chain id passed to the spawned server.
pub const CHAIN_ID: u64 = 1;

/// TrampolineFactory address passed to the spawned server (EIP-712
/// `verifyingContract`).
pub const TRAMPOLINE_FACTORY: &str = "0x00000000000000000000000000000000DeaDBeef";

// ---------------------------------------------------------------------------
// TestServer
// ---------------------------------------------------------------------------

/// A running byos server and an HTTP client pointed at it.
pub struct TestServer {
    base_url: String,
    client: reqwest::Client,
}

impl TestServer {
    /// Spawns the real server (full `run()` path) on an ephemeral port and
    /// waits for the bound address. The bind channel fires after the listener
    /// is bound, so the server accepts connections as soon as this returns.
    pub async fn spawn() -> Self {
        let (bind_tx, bind_rx) = tokio::sync::oneshot::channel();
        let args = [
            "byos".to_string(),
            "--public-addr".to_string(),
            "127.0.0.1:0".to_string(),
            "--chain-id".to_string(),
            CHAIN_ID.to_string(),
            "--trampoline-factory".to_string(),
            TRAMPOLINE_FACTORY.to_string(),
        ];
        // Detach: the server task runs until the test's runtime is torn down.
        drop(tokio::spawn(crate::run(args, bind_tx)));
        let addr = bind_rx.await.expect("server failed to bind");
        Self {
            base_url: format!("http://{addr}"),
            client: reqwest::Client::new(),
        }
    }

    /// The EIP-712 domain matching the spawned server's args.
    pub fn domain(&self) -> Eip712Domain {
        eip712::byos_domain(CHAIN_ID, TRAMPOLINE_FACTORY.parse().unwrap())
    }

    /// POST a JSON body; returns status and response JSON.
    pub async fn post_json(&self, path: &str, body: &Value) -> (StatusCode, Value) {
        let resp = self
            .client
            .post(format!("{}{path}", self.base_url))
            // Manual header + string body: the workspace reqwest has no
            // `json` feature.
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .expect("request failed");
        json_of(resp).await
    }

    /// GET a path; returns status and response JSON.
    pub async fn get_json(&self, path: &str) -> (StatusCode, Value) {
        let resp = self
            .client
            .get(format!("{}{path}", self.base_url))
            .send()
            .await
            .expect("request failed");
        json_of(resp).await
    }

    /// DELETE a path, optionally with an `X-Signature` header; returns status
    /// and response JSON (`Null` for empty bodies, e.g. 204).
    pub async fn delete(&self, path: &str, signature: Option<&str>) -> (StatusCode, Value) {
        let mut req = self.client.delete(format!("{}{path}", self.base_url));
        if let Some(sig) = signature {
            req = req.header("X-Signature", sig);
        }
        let resp = req.send().await.expect("request failed");
        json_of(resp).await
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
            valid_until: U256::from(1_700_000_000u64),
            nonce: U256::from(1u64),
            interactions: vec![Interaction {
                target: Address::repeat_byte(0x11),
                value: U256::ZERO,
                callData: vec![0x01, 0x02].into(),
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
    pub async fn signed_body(&self, signer: &PrivateKeySigner, domain: &Eip712Domain) -> Value {
        let sig = eip712::sign_proposal(signer, domain, &self.as_proposal(), &self.interactions)
            .await
            .expect("signing should succeed");
        self.body_with_signature(&hex_bytes(&sig.as_bytes()))
    }

    /// Renders the JSON body with an arbitrary signature string.
    pub fn body_with_signature(&self, signature: &str) -> Value {
        json!({
            "orderUid": hex_bytes(&self.order_uid),
            "sellAmount": self.sell_amount.to_string(),
            "buyAmount": self.buy_amount.to_string(),
            "interactions": self.interactions.iter().map(|i| json!({
                "target": i.target.to_string(),
                "value": i.value.to_string(),
                "callData": hex_bytes(&i.callData),
            })).collect::<Vec<_>>(),
            "validUntil": self.valid_until.to_string(),
            "nonce": self.nonce.to_string(),
            "signature": signature,
        })
    }
}

/// Signs a `CancelProposal` message and renders it as an `X-Signature` value.
pub async fn cancel_signature(
    signer: &PrivateKeySigner,
    domain: &Eip712Domain,
    proposal_id: u64,
) -> String {
    let sig = eip712::sign_cancellation(signer, domain, U256::from(proposal_id))
        .await
        .expect("signing should succeed");
    hex_bytes(&sig.as_bytes())
}

/// `0x`-prefixed lowercase hex.
pub fn hex_bytes(bytes: &[u8]) -> String {
    format!("0x{}", alloy::hex::encode(bytes))
}
