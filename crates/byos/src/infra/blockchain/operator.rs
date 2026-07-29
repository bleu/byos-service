//! Escrow operator (ADR-0003, COW-1205): the transaction-sending chain edge
//! behind [`DebitEscrow`]. The only place in the service that signs and
//! submits transactions — everything else is read-only RPC. Costs come from
//! `eth_getTransactionReceipt` on the reverted settlement; debits go out as
//! operator-signed `Escrow.debit(subSolver, amount, reason)` calls.

use {
    crate::domain::penalty::{DebitError, DebitEscrow},
    alloy::{
        primitives::{Address, B256, U256},
        providers::Provider,
    },
};

/// Sends Track A debits from the operator account. `provider` must carry the
/// operator signer (wallet filler) — construction happens in `run.rs` from
/// `--operator-private-key`.
pub struct EscrowOperator<P> {
    provider: P,
    escrow_address: Address,
}

impl<P> EscrowOperator<P> {
    pub fn new(provider: P, escrow_address: Address) -> Self {
        Self {
            provider,
            escrow_address,
        }
    }
}

impl<P: Provider + Send + Sync> DebitEscrow for EscrowOperator<P> {
    async fn settlement_cost(&self, tx: B256) -> Result<U256, DebitError> {
        let receipt = self
            .provider
            .get_transaction_receipt(tx)
            .await
            .map_err(|e| DebitError::Transient(e.to_string()))?
            // The driver cited this tx, so it exists — a missing receipt is
            // RPC lag, not a permanent condition.
            .ok_or_else(|| DebitError::Transient(format!("no receipt yet for {tx:#x}")))?;
        Ok(U256::from(receipt.gas_used).saturating_mul(U256::from(receipt.effective_gas_price)))
    }

    async fn debit(
        &self,
        sub_solver: Address,
        amount: U256,
        reason: B256,
    ) -> Result<B256, DebitError> {
        let escrow = byos_common::contracts::Escrow::new(self.escrow_address, &self.provider);
        let receipt = escrow
            .debit(sub_solver, amount, reason)
            .send()
            .await
            .map_err(|e| DebitError::Transient(e.to_string()))?
            .get_receipt()
            .await
            .map_err(|e| DebitError::Transient(e.to_string()))?;
        // A reverted debit retries next tick like any other failure; if it
        // keeps reverting (operator lacks the role, escrow paused) the
        // per-tick warn is the ops page.
        if !receipt.status() {
            return Err(DebitError::Transient(format!(
                "debit tx {:#x} reverted",
                receipt.transaction_hash
            )));
        }
        Ok(receipt.transaction_hash)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        alloy::{
            hex,
            primitives::{Bytes, address, b256},
            providers::ProviderBuilder,
            signers::local::PrivateKeySigner,
            sol_types::SolCall,
        },
        std::sync::{Arc, Mutex},
        wiremock::{Mock, MockServer, ResponseTemplate, matchers::method},
    };

    const ESCROW: Address = address!("00000000000000000000000000000000000000ee");
    const SUB_SOLVER: Address = address!("0000000000000000000000000000000000000001");
    const SETTLEMENT_TX: B256 =
        b256!("2222222222222222222222222222222222222222222222222222222222222222");
    const DEBIT_TX: B256 =
        b256!("7777777777777777777777777777777777777777777777777777777777777777");

    /// A receipt in the shape `eth_getTransactionReceipt` returns.
    fn receipt_json(hash: B256, gas_used: u64, effective_gas_price: u128) -> serde_json::Value {
        serde_json::json!({
            "transactionHash": hash,
            "transactionIndex": "0x0",
            "blockHash": format!("0x{}", "bb".repeat(32)),
            "blockNumber": "0x10",
            "from": format!("{SUB_SOLVER:#x}"),
            "to": format!("{ESCROW:#x}"),
            "cumulativeGasUsed": format!("{gas_used:#x}"),
            "gasUsed": format!("{gas_used:#x}"),
            "effectiveGasPrice": format!("{effective_gas_price:#x}"),
            "contractAddress": null,
            "logs": [],
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "status": "0x1",
            "type": "0x2",
        })
    }

    /// Fake JSON-RPC node: serves the known settlement receipt (200_000 gas
    /// at 30 gwei), accepts the signed debit (capturing its raw bytes), and
    /// confirms it as [`DEBIT_TX`].
    struct RpcResponder {
        sent: Arc<Mutex<Option<Bytes>>>,
    }

    impl wiremock::Respond for RpcResponder {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let result = match body["method"].as_str().unwrap_or_default() {
                "eth_chainId" => serde_json::json!("0x1"),
                "eth_blockNumber" => serde_json::json!("0x10"),
                "eth_getTransactionCount" => serde_json::json!("0x0"),
                "eth_estimateGas" => serde_json::json!("0x186a0"),
                "eth_gasPrice" => serde_json::json!("0x3b9aca00"),
                "eth_maxPriorityFeePerGas" => serde_json::json!("0x3b9aca00"),
                "eth_feeHistory" => serde_json::json!({
                    "oldestBlock": "0xf",
                    "baseFeePerGas": ["0x3b9aca00", "0x3b9aca00"],
                    "gasUsedRatio": [0.5],
                    "reward": [["0x3b9aca00"]],
                }),
                "eth_sendRawTransaction" => {
                    let raw = body["params"][0].as_str().unwrap();
                    *self.sent.lock().unwrap() = Some(hex::decode(raw).expect("raw tx hex").into());
                    serde_json::json!(DEBIT_TX)
                }
                "eth_getTransactionReceipt" => {
                    let hash: B256 = body["params"][0].as_str().unwrap().parse().unwrap();
                    if hash == SETTLEMENT_TX {
                        // 200_000 gas at 30 gwei: 0.006 ETH on-chain cost.
                        receipt_json(SETTLEMENT_TX, 200_000, 30_000_000_000)
                    } else {
                        receipt_json(DEBIT_TX, 50_000, 1_000_000_000)
                    }
                }
                other => panic!("unexpected RPC method {other}"),
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": body["id"],
                "result": result,
            }))
        }
    }

    /// Mounts the responder and returns the server plus the captured raw
    /// debit tx slot.
    async fn rpc_server() -> (MockServer, Arc<Mutex<Option<Bytes>>>) {
        let sent = Arc::new(Mutex::new(None));
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(RpcResponder { sent: sent.clone() })
            .mount(&server)
            .await;
        (server, sent)
    }

    fn operator_at(uri: String) -> EscrowOperator<impl Provider> {
        let provider = ProviderBuilder::new()
            .wallet(PrivateKeySigner::random())
            .connect_http(uri.parse().unwrap());
        EscrowOperator::new(provider, ESCROW)
    }

    /// Acceptance (COW-1205): the debit prices the revert from the real
    /// receipt — `gas_used × effective_gas_price`.
    #[tokio::test]
    async fn settlement_cost_is_gas_used_times_effective_gas_price() {
        let (server, _) = rpc_server().await;
        let operator = operator_at(server.uri());

        let cost = operator
            .settlement_cost(SETTLEMENT_TX)
            .await
            .expect("receipt exists");

        assert_eq!(
            cost,
            U256::from(6_000_000_000_000_000u64),
            "200_000 gas at 30 gwei must price as 0.006 ETH"
        );
    }

    /// The debit goes on-chain as `Escrow.debit(subSolver, amount, reason)`
    /// from the operator account, and resolves with the landed tx hash.
    #[tokio::test]
    async fn debit_submits_the_signed_escrow_call_and_returns_the_landed_tx() {
        let (server, sent) = rpc_server().await;
        let operator = operator_at(server.uri());
        let amount = U256::from(16_000_000_000_000_000u64); // 0.006 gas + 0.010 c_l

        let landed = operator
            .debit(SUB_SOLVER, amount, SETTLEMENT_TX)
            .await
            .expect("debit lands");
        assert_eq!(landed, DEBIT_TX);

        let raw = sent
            .lock()
            .unwrap()
            .clone()
            .expect("a raw tx was submitted");
        let envelope =
            <alloy::consensus::TxEnvelope as alloy::eips::eip2718::Decodable2718>::decode_2718(
                &mut raw.as_ref(),
            )
            .expect("submitted bytes decode as a signed tx");
        use alloy::consensus::Transaction;
        assert_eq!(
            envelope.to(),
            Some(ESCROW),
            "the call must target the escrow"
        );
        assert_eq!(
            envelope.input(),
            &Bytes::from(
                byos_common::contracts::Escrow::debitCall {
                    _subSolver: SUB_SOLVER,
                    _amount: amount,
                    _reason: SETTLEMENT_TX,
                }
                .abi_encode()
            ),
            "calldata must be debit(subSolver, amount, reason) with the settlement tx as reason"
        );
    }

    /// A receipt the node cannot serve yet is a retry, not a crash — the
    /// proposal stays in `SettleFailed` until a later tick prices it.
    #[tokio::test]
    async fn missing_receipt_defers_the_debit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "result": null,
            })))
            .mount(&server)
            .await;
        let operator = operator_at(server.uri());

        let result = operator.settlement_cost(SETTLEMENT_TX).await;

        assert!(
            matches!(result, Err(DebitError::Transient(_))),
            "missing receipt must defer, got {result:?}"
        );
    }
}
