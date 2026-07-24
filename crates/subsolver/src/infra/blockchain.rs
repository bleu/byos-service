//! Read-only RPC edge: Uniswap V2 pair reserves and the sub-solver's
//! Trampoline address (`TrampolineFactory.addressOf`, via the vendored ABI
//! binding in `byos_common::contracts`). The sub-solver never sends
//! transactions — onboarding (escrow deposit, trampoline deployment) is
//! assumed done.

use alloy::{
    primitives::{Address, U256},
    providers::{CallItem, DynProvider, MulticallBuilder},
    sol,
    sol_types::SolCall,
};

sol! {
    function getReserves() returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
}

/// Read-only chain queries against any JSON-RPC provider.
pub struct ChainClient {
    provider: DynProvider,
}

/// One pair whose reserves a poll needs, oriented by trade direction.
pub struct ReservesQuery {
    pub pair: Address,
    pub sell_token: Address,
    pub buy_token: Address,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("multicall failed: {0}")]
    Multicall(#[from] alloy::providers::MulticallError),
    #[error("contract call failed: {0}")]
    Contract(#[from] alloy::contract::Error),
}

impl ChainClient {
    pub fn new(provider: DynProvider) -> Self {
        Self { provider }
    }

    /// The reserves of every queried pair, fetched in a single Multicall3
    /// `aggregate3` round-trip (the canonical deployment, present on mainnet
    /// and anvil), each oriented as `(reserve of sell_token, reserve of
    /// buy_token)` — Uniswap V2 stores reserves sorted by token address;
    /// this undoes that. Entries whose call fails or returns undecodable
    /// data (typically: the pool does not exist) come back as `None`.
    pub async fn reserves(
        &self,
        queries: &[ReservesQuery],
    ) -> Result<Vec<Option<(U256, U256)>>, Error> {
        if queries.is_empty() {
            return Ok(vec![]);
        }
        let mut multicall = MulticallBuilder::new_dynamic(self.provider.clone());
        for query in queries {
            multicall = multicall.add_call_dynamic(
                CallItem::<getReservesCall>::new(
                    query.pair,
                    getReservesCall {}.abi_encode().into(),
                )
                .allow_failure(true),
            );
        }
        let results = multicall.aggregate3().await?;
        Ok(queries
            .iter()
            .zip(results)
            .map(|(query, result)| {
                let reserves = result.ok()?;
                let (reserve0, reserve1) =
                    (U256::from(reserves.reserve0), U256::from(reserves.reserve1));
                Some(if query.sell_token < query.buy_token {
                    (reserve0, reserve1)
                } else {
                    (reserve1, reserve0)
                })
            })
            .collect())
    }

    /// The sub-solver's Trampoline instance, as derived by the factory
    /// (`TrampolineFactory.addressOf`). Works whether or not the instance is
    /// deployed yet — the address is a pure CREATE2 derivation.
    pub async fn trampoline(
        &self,
        factory: Address,
        sub_solver: Address,
    ) -> Result<Address, Error> {
        let factory =
            byos_common::contracts::TrampolineFactory::new(factory, self.provider.clone());
        Ok(factory.addressOf(sub_solver).call().await?)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        alloy::{
            primitives::{Address, B256, Bytes, U256, address},
            providers::{Provider, ProviderBuilder, bindings::IMulticall3},
            transports::mock::Asserter,
        },
    };

    const WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const USDC: Address = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

    fn chain(asserter: &Asserter) -> ChainClient {
        ChainClient::new(
            ProviderBuilder::new()
                .connect_mocked_client(asserter.clone())
                .erased(),
        )
    }

    /// ABI-encodes a `getReserves()` return: reserve0, reserve1, timestamp.
    fn reserves_return(reserve0: u64, reserve1: u64) -> Bytes {
        let words = [
            U256::from(reserve0),
            U256::from(reserve1),
            U256::from(1_750_000_000u64),
        ];
        words
            .iter()
            .flat_map(|word| B256::from(*word).0)
            .collect::<Vec<u8>>()
            .into()
    }

    /// ABI-encodes an `aggregate3` return wrapping each entry: `Some(bytes)`
    /// is a successful call, `None` a failed one.
    fn aggregate3_return(entries: Vec<Option<Bytes>>) -> Bytes {
        let results: Vec<IMulticall3::Result> = entries
            .into_iter()
            .map(|entry| match entry {
                Some(data) => IMulticall3::Result {
                    success: true,
                    returnData: data,
                },
                None => IMulticall3::Result {
                    success: false,
                    returnData: Bytes::new(),
                },
            })
            .collect();
        IMulticall3::aggregate3Call::abi_encode_returns(&results).into()
    }

    #[tokio::test]
    async fn reserves_are_oriented_by_the_order_tokens_not_pool_sort_order() {
        // USDC < WETH, so the pool's reserve0 is USDC. An order selling WETH
        // for USDC must see (reserve_sell = reserve1, reserve_buy = reserve0),
        // while the opposite direction keeps the pool order. Both directions
        // travel in the same single multicall.
        let asserter = Asserter::new();
        asserter.push_success(&aggregate3_return(vec![
            Some(reserves_return(5_000_000, 2_000)),
            Some(reserves_return(5_000_000, 2_000)),
        ]));

        let reserves = chain(&asserter)
            .reserves(&[
                ReservesQuery {
                    pair: Address::ZERO,
                    sell_token: WETH,
                    buy_token: USDC,
                },
                ReservesQuery {
                    pair: Address::ZERO,
                    sell_token: USDC,
                    buy_token: WETH,
                },
            ])
            .await
            .unwrap();

        assert_eq!(
            reserves,
            vec![
                Some((U256::from(2_000), U256::from(5_000_000))),
                Some((U256::from(5_000_000), U256::from(2_000))),
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_pair_yields_none_without_poisoning_the_batch() {
        // First pair reverts (no pool), second succeeds with empty return
        // data (an address with no code), third is a real pool.
        let asserter = Asserter::new();
        asserter.push_success(&aggregate3_return(vec![
            None,
            Some(Bytes::new()),
            Some(reserves_return(5_000_000, 2_000)),
        ]));

        let query = |pair| ReservesQuery {
            pair,
            sell_token: USDC,
            buy_token: WETH,
        };
        let reserves = chain(&asserter)
            .reserves(&[
                query(Address::ZERO),
                query(Address::repeat_byte(1)),
                query(Address::repeat_byte(2)),
            ])
            .await
            .unwrap();

        assert_eq!(
            reserves,
            vec![None, None, Some((U256::from(5_000_000), U256::from(2_000)))]
        );
    }

    #[tokio::test]
    async fn no_queries_means_no_rpc_call() {
        // The asserter has no queued responses: any request would error.
        let reserves = chain(&Asserter::new()).reserves(&[]).await.unwrap();
        assert!(reserves.is_empty());
    }

    #[tokio::test]
    async fn trampoline_address_comes_from_the_factory() {
        let trampoline = address!("0x00000000000000000000000000000000f00dbabe");
        let asserter = Asserter::new();
        asserter.push_success(&Bytes::from(
            B256::left_padding_from(trampoline.as_slice()).0,
        ));

        let resolved = chain(&asserter)
            .trampoline(
                Address::ZERO,
                address!("0x00000000000000000000000000000000000a11ce"),
            )
            .await
            .unwrap();
        assert_eq!(resolved, trampoline);
    }
}
