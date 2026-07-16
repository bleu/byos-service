//! Read-only RPC edge: Uniswap V2 pair reserves and the sub-solver's
//! Trampoline address (`TrampolineFactory.addressOf`). The sub-solver never
//! sends transactions — onboarding (escrow deposit, trampoline deployment)
//! is assumed done.

use alloy::{
    primitives::{Address, U256},
    providers::{DynProvider, Provider},
    rpc::types::TransactionRequest,
    sol,
    sol_types::SolCall,
};

sol! {
    function getReserves() returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    function addressOf(address subSolver) returns (address trampoline);
}

/// Read-only chain queries against any JSON-RPC provider.
pub struct ChainClient {
    provider: DynProvider,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Rpc(#[from] alloy::transports::TransportError),
    #[error("call returned undecodable data: {0}")]
    Decode(#[from] alloy::sol_types::Error),
}

impl ChainClient {
    pub fn new(provider: DynProvider) -> Self {
        Self { provider }
    }

    /// The pair's reserves oriented by trade direction:
    /// `(reserve of sell_token, reserve of buy_token)`. Uniswap V2 stores
    /// reserves sorted by token address; this undoes that.
    pub async fn reserves(
        &self,
        pair: Address,
        sell_token: Address,
        buy_token: Address,
    ) -> Result<(U256, U256), Error> {
        let returned = self.call(pair, getReservesCall {}.abi_encode()).await?;
        let reserves = getReservesCall::abi_decode_returns(&returned)?;
        let (reserve0, reserve1) = (U256::from(reserves.reserve0), U256::from(reserves.reserve1));
        if sell_token < buy_token {
            Ok((reserve0, reserve1))
        } else {
            Ok((reserve1, reserve0))
        }
    }

    /// The sub-solver's Trampoline instance, as derived by the factory
    /// (`TrampolineFactory.addressOf`). Works whether or not the instance is
    /// deployed yet — the address is a pure CREATE2 derivation.
    pub async fn trampoline(
        &self,
        factory: Address,
        sub_solver: Address,
    ) -> Result<Address, Error> {
        let returned = self
            .call(
                factory,
                addressOfCall {
                    subSolver: sub_solver,
                }
                .abi_encode(),
            )
            .await?;
        Ok(addressOfCall::abi_decode_returns(&returned)?)
    }

    async fn call(&self, to: Address, input: Vec<u8>) -> Result<alloy::primitives::Bytes, Error> {
        let request = TransactionRequest::default().to(to).input(input.into());
        Ok(self.provider.call(request).await?)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        alloy::{
            primitives::{Address, B256, Bytes, U256, address},
            providers::{Provider, ProviderBuilder},
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

    #[tokio::test]
    async fn reserves_are_oriented_by_the_order_tokens_not_pool_sort_order() {
        // USDC < WETH, so the pool's reserve0 is USDC. An order selling WETH
        // for USDC must see (reserve_sell = reserve1, reserve_buy = reserve0).
        let asserter = Asserter::new();
        asserter.push_success(&reserves_return(5_000_000, 2_000));
        let (reserve_sell, reserve_buy) = chain(&asserter)
            .reserves(Address::ZERO, WETH, USDC)
            .await
            .unwrap();
        assert_eq!(reserve_sell, U256::from(2_000));
        assert_eq!(reserve_buy, U256::from(5_000_000));

        // The opposite direction flips the orientation.
        let asserter = Asserter::new();
        asserter.push_success(&reserves_return(5_000_000, 2_000));
        let (reserve_sell, reserve_buy) = chain(&asserter)
            .reserves(Address::ZERO, USDC, WETH)
            .await
            .unwrap();
        assert_eq!(reserve_sell, U256::from(5_000_000));
        assert_eq!(reserve_buy, U256::from(2_000));
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
