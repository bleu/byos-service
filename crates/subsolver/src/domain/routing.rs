//! Baseline single-hop routing against Uniswap V2, mirroring the example
//! solvers in cowprotocol/services: constant-product math with the 0.3% fee,
//! plus deterministic CREATE2 pair-address derivation. Pure math — reserve
//! fetching lives in `infra`.

use alloy::primitives::{Address, B256, U256, keccak256};

const FEE_NUMERATOR: u64 = 997;
const FEE_DENOMINATOR: u64 = 1000;

/// Output amount of a Uniswap V2 swap after the 0.3% fee, `None` when the
/// pool cannot fill it (empty reserves or arithmetic overflow).
pub fn amount_out(amount_in: U256, reserve_in: U256, reserve_out: U256) -> Option<U256> {
    if reserve_in.is_zero() || reserve_out.is_zero() {
        return None;
    }
    let amount_in_with_fee = amount_in.checked_mul(U256::from(FEE_NUMERATOR))?;
    let numerator = amount_in_with_fee.checked_mul(reserve_out)?;
    let denominator = reserve_in
        .checked_mul(U256::from(FEE_DENOMINATOR))?
        .checked_add(amount_in_with_fee)?;
    Some(numerator / denominator)
}

/// Input amount required to receive `amount_out` from a Uniswap V2 swap,
/// `None` when the pool cannot fill it (the ask meets or exceeds the output
/// reserve, empty reserves, or arithmetic overflow).
pub fn amount_in(amount_out: U256, reserve_in: U256, reserve_out: U256) -> Option<U256> {
    if reserve_in.is_zero() || amount_out >= reserve_out {
        return None;
    }
    let numerator = reserve_in
        .checked_mul(amount_out)?
        .checked_mul(U256::from(FEE_DENOMINATOR))?;
    let denominator = (reserve_out - amount_out).checked_mul(U256::from(FEE_NUMERATOR))?;
    (numerator / denominator).checked_add(U256::ONE)
}

/// Deterministic CREATE2 address of the Uniswap V2 pair for two tokens.
pub fn pair_address(factory: Address, init_code_hash: B256, token_a: Address, token_b: Address) -> Address {
    let (token0, token1) = if token_a < token_b { (token_a, token_b) } else { (token_b, token_a) };
    let salt = keccak256([token0.as_slice(), token1.as_slice()].concat());
    factory.create2(salt, init_code_hash)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{U256, address, b256};

    use super::*;

    /// Canonical Uniswap V2 mainnet deployment parameters.
    const FACTORY: alloy::primitives::Address = address!("0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f");
    const INIT_CODE_HASH: alloy::primitives::B256 =
        b256!("0x96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f");

    #[test]
    fn amount_out_applies_the_constant_product_formula_with_fee() {
        // 997 * 1000 * 10000 / (10000 * 1000 + 997 * 1000) = 906.6... -> 906
        assert_eq!(
            amount_out(U256::from(1000), U256::from(10_000), U256::from(10_000)),
            Some(U256::from(906))
        );
    }

    #[test]
    fn amount_in_is_the_inverse_of_amount_out() {
        // Buying 906 out of 10000/10000 reserves costs at most the 1000 that
        // amount_out said would yield 906.
        let cost = amount_in(U256::from(906), U256::from(10_000), U256::from(10_000)).unwrap();
        assert_eq!(cost, U256::from(1000));
        assert_eq!(
            amount_out(cost, U256::from(10_000), U256::from(10_000)),
            Some(U256::from(906))
        );
    }

    #[test]
    fn draining_or_exceeding_the_output_reserve_is_unfillable() {
        // Asking for the whole reserve (or more) can never be filled.
        assert_eq!(amount_in(U256::from(10_000), U256::from(10_000), U256::from(10_000)), None);
        assert_eq!(amount_in(U256::from(10_001), U256::from(10_000), U256::from(10_000)), None);
    }

    #[test]
    fn empty_reserves_are_unfillable() {
        assert_eq!(amount_out(U256::from(1000), U256::ZERO, U256::from(10_000)), None);
        assert_eq!(amount_out(U256::from(1000), U256::from(10_000), U256::ZERO), None);
        assert_eq!(amount_in(U256::from(10), U256::ZERO, U256::from(10_000)), None);
    }

    #[test]
    fn overflowing_amounts_are_unfillable_rather_than_wrong() {
        assert_eq!(amount_out(U256::MAX, U256::MAX, U256::MAX), None);
    }

    #[test]
    fn pair_address_matches_the_mainnet_usdc_weth_pool() {
        let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let pair = address!("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");

        assert_eq!(pair_address(FACTORY, INIT_CODE_HASH, usdc, weth), pair);
        // Token order must not matter: the pair sorts its tokens.
        assert_eq!(pair_address(FACTORY, INIT_CODE_HASH, weth, usdc), pair);
    }
}
