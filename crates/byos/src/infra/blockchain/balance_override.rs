//! Heuristic ERC-20 balance slot detection and state override builder.
//!
//! Given a token and a holder, detects which storage slot backs
//! `balanceOf(holder)` by probing known mapping layouts (Solidity, Solady)
//! with a sentinel write-and-readback. Results are cached per token.
//!
//! Inspired by [`cowprotocol/services` `balance-overrides`][cow-bo] but
//! limited to heuristic probing (no `debug_traceCall`).
//!
//! [cow-bo]: https://github.com/cowprotocol/services/tree/main/crates/balance-overrides

use {
    alloy::{
        primitives::{Address, B256, U256, keccak256},
        providers::Provider,
        rpc::types::state::AccountOverride,
        sol_types::SolCall,
    },
    byos_common::contracts::ERC20,
    parking_lot::Mutex,
    std::collections::HashMap,
};

/// Solady magic bytes for the balance slot seed.
/// <https://github.com/Vectorized/solady/blob/main/src/tokens/ERC20.sol#L81>
const SOLADY_BALANCE_SLOT_SEED: [u8; 4] = [0x87, 0xa2, 0x11, 0xa2];

/// Distinct-byte sentinel written to candidate slots during verification.
/// Every byte is unique and non-zero, so a successful `balanceOf` readback
/// that equals this value confirms the slot is correct.
const SENTINEL: U256 = U256::from_be_bytes([
    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10,
    0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20,
]);

/// Default number of Solidity mapping slot indices to probe (0..depth).
pub const DEFAULT_PROBING_DEPTH: u8 = 11;

/// Resolved balance override strategy for a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BalanceSlotStrategy {
    /// Standard Solidity `mapping(address => uint256)` at `map_slot`.
    /// Storage key = `keccak256(pad32(holder) ++ pad32(map_slot))`.
    SolidityMapping { map_slot: U256 },
    /// Solady ERC-20 balance layout.
    /// Storage key = `keccak256(holder[0..20] ++ 0x00000000_87a211a2)`.
    SoladyMapping,
}

impl BalanceSlotStrategy {
    /// Compute the storage slot key for a given holder.
    fn storage_key(&self, holder: &Address) -> B256 {
        match self {
            Self::SolidityMapping { map_slot } => {
                let mut buf = [0u8; 64];
                buf[12..32].copy_from_slice(holder.as_slice());
                buf[32..64].copy_from_slice(&map_slot.to_be_bytes::<32>());
                keccak256(buf)
            }
            Self::SoladyMapping => {
                let mut buf = [0u8; 32];
                buf[0..20].copy_from_slice(holder.as_slice());
                buf[28..32].copy_from_slice(&SOLADY_BALANCE_SLOT_SEED);
                keccak256(buf)
            }
        }
    }
}

/// Build an [`AccountOverride`] that sets `balanceOf[holder] = amount` on
/// the given token using the detected strategy.
pub(crate) fn build_override(
    strategy: &BalanceSlotStrategy,
    token: Address,
    holder: &Address,
    amount: &U256,
) -> (Address, AccountOverride) {
    let key = strategy.storage_key(holder);
    let state_override = AccountOverride {
        state_diff: Some(std::iter::once((key, B256::new(amount.to_be_bytes::<32>()))).collect()),
        ..Default::default()
    };
    (token, state_override)
}

/// Heuristic balance slot detector with per-token caching.
pub struct BalanceSlotDetector<P> {
    provider: P,
    probing_depth: u8,
    cache: Mutex<HashMap<Address, Option<BalanceSlotStrategy>>>,
}

impl<P: Provider + Clone + Send + Sync> BalanceSlotDetector<P> {
    pub fn new(provider: P, probing_depth: u8) -> Self {
        Self {
            provider,
            probing_depth,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Detect the balance slot strategy for `token`, using `holder` as
    /// the probe address. Returns `None` if no supported layout was found
    /// (cached as a permanent failure for this token).
    pub(crate) async fn detect(
        &self,
        token: Address,
        holder: Address,
    ) -> Option<BalanceSlotStrategy> {
        // Check cache first.
        if let Some(cached) = self.cache.lock().get(&token) {
            return cached.clone();
        }

        let result = self.probe(token, holder).await;

        if result.is_none() {
            tracing::warn!(
                ?token,
                "balance slot detection failed — token uses an unsupported storage layout",
            );
        }

        self.cache.lock().insert(token, result.clone());
        result
    }

    /// Probe candidate strategies and verify with a sentinel readback.
    async fn probe(&self, token: Address, holder: Address) -> Option<BalanceSlotStrategy> {
        // Try Solidity mapping slots 0..depth.
        for i in 0..self.probing_depth {
            let strategy = BalanceSlotStrategy::SolidityMapping {
                map_slot: U256::from(i),
            };
            if self.verify(token, &holder, &strategy).await {
                tracing::debug!(?token, slot = i, "detected Solidity mapping balance slot",);
                return Some(strategy);
            }
        }

        // Try Solady mapping.
        let strategy = BalanceSlotStrategy::SoladyMapping;
        if self.verify(token, &holder, &strategy).await {
            tracing::debug!(?token, "detected Solady mapping balance slot");
            return Some(strategy);
        }

        None
    }

    /// Write a sentinel to the candidate slot via state override and read
    /// back `balanceOf(holder)`. Returns `true` if the readback matches.
    async fn verify(
        &self,
        token: Address,
        holder: &Address,
        strategy: &BalanceSlotStrategy,
    ) -> bool {
        let (override_addr, account_override) = build_override(strategy, token, holder, &SENTINEL);

        let call = ERC20::balanceOfCall { owner: *holder };
        let calldata = call.abi_encode();

        let tx = alloy::rpc::types::TransactionRequest::default()
            .to(token)
            .input(alloy::rpc::types::TransactionInput::new(calldata.into()));

        let result = self
            .provider
            .call(tx)
            .account_override(override_addr, account_override)
            .await;

        match result {
            Ok(output) => match ERC20::balanceOfCall::abi_decode_returns(&output) {
                Ok(balance) => balance == SENTINEL,
                Err(_) => false,
            },
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, alloy::primitives::address};

    #[test]
    fn solidity_mapping_slot_computation() {
        // WETH uses slot 3 for balances. For holder 0x0001, the storage key
        // should be keccak256(pad32(0x0001) ++ pad32(3)).
        let holder = address!("0000000000000000000000000000000000000001");
        let strategy = BalanceSlotStrategy::SolidityMapping {
            map_slot: U256::from(3),
        };

        let mut buf = [0u8; 64];
        buf[12..32].copy_from_slice(holder.as_slice());
        buf[32..64].copy_from_slice(&U256::from(3).to_be_bytes::<32>());
        let expected = keccak256(buf);

        assert_eq!(strategy.storage_key(&holder), expected);
    }

    #[test]
    fn solady_mapping_slot_computation() {
        let holder = address!("d8dA6BF26964aF9D7eEd9e03E53415D37aA96045");
        let strategy = BalanceSlotStrategy::SoladyMapping;

        let mut buf = [0u8; 32];
        buf[0..20].copy_from_slice(holder.as_slice());
        buf[28..32].copy_from_slice(&SOLADY_BALANCE_SLOT_SEED);
        let expected = keccak256(buf);

        assert_eq!(strategy.storage_key(&holder), expected);
    }

    #[test]
    fn build_override_produces_correct_state_diff() {
        let token = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let holder = address!("0000000000000000000000000000000000000001");
        let amount = U256::from(1_000_000u64);
        let strategy = BalanceSlotStrategy::SolidityMapping {
            map_slot: U256::from(3),
        };

        let (addr, account_override) = build_override(&strategy, token, &holder, &amount);

        assert_eq!(addr, token);
        let state_diff = account_override.state_diff.expect("should have state_diff");
        assert_eq!(state_diff.len(), 1);

        let expected_key = strategy.storage_key(&holder);
        let expected_value = B256::new(amount.to_be_bytes::<32>());
        assert_eq!(state_diff.get(&expected_key), Some(&expected_value));
    }

    #[test]
    fn cache_returns_none_for_failed_detection() {
        let provider = alloy::providers::ProviderBuilder::new()
            .connect_http("http://127.0.0.1:1".parse().unwrap());
        let detector = BalanceSlotDetector::new(provider, DEFAULT_PROBING_DEPTH);

        let token = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

        // Pre-populate cache with a failure.
        detector.cache.lock().insert(token, None);

        // Synchronous cache check — no RPC needed.
        let cached = detector.cache.lock().get(&token).cloned();
        assert_eq!(cached, Some(None));
    }

    #[test]
    fn cache_returns_strategy_for_detected_token() {
        let provider = alloy::providers::ProviderBuilder::new()
            .connect_http("http://127.0.0.1:1".parse().unwrap());
        let detector = BalanceSlotDetector::new(provider, DEFAULT_PROBING_DEPTH);

        let token = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let strategy = BalanceSlotStrategy::SolidityMapping {
            map_slot: U256::from(3),
        };

        detector.cache.lock().insert(token, Some(strategy.clone()));

        let cached = detector.cache.lock().get(&token).cloned();
        assert_eq!(cached, Some(Some(strategy)));
    }

    #[test]
    fn sentinel_has_distinct_bytes() {
        let bytes = SENTINEL.to_be_bytes::<32>();
        let mut seen = std::collections::HashSet::new();
        for &b in &bytes {
            assert_ne!(b, 0, "sentinel bytes should be non-zero");
            assert!(seen.insert(b), "sentinel bytes should be distinct");
        }
    }
}
