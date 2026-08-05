//! Tier-1 e2e: GPv2Settlement accepts partial-fill settlement encoding.
//!
//! A same-token (WETH→WETH) sell order is presigned on GPv2, then settled
//! twice at different partial fill amounts. No trampoline — the settlement
//! has no interactions so GPv2's trade mechanics run in isolation: flag
//! decoding, `executedAmount` interpretation, cumulative fill tracking,
//! and the scaled limit-price check.

use {
    alloy::{
        network::EthereumWallet,
        primitives::{Address, B256, Bytes, U256, address, keccak256},
        providers::{Provider, ProviderBuilder},
        rpc::types::TransactionRequest,
        signers::local::PrivateKeySigner,
        sol,
        sol_types::SolCall,
    },
    byos_common::contracts::{GPv2InteractionData, GPv2Settlement, GPv2TradeData},
    e2e::chain::{Chain, GPV2_SETTLEMENT},
};

/// WETH at its mainnet address (present in the offline-mode state).
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

/// The GPv2VaultRelayer — the spender the user must approve.
const VAULT_RELAYER: Address = address!("C92E8bdf79f0507f65a392b0ab4667716BFE0110");

// Minimal interface bindings for functions not on the re-exported
// GPv2Settlement (which only carries `settle`).
sol! {
    #[sol(rpc)]
    interface ISettlement {
        function domainSeparator() external view returns (bytes32);
        function setPreSignature(bytes calldata orderUid, bool signed) external;
    }

    #[sol(rpc)]
    interface IWETH {
        function deposit() external payable;
        function approve(address spender, uint256 amount) external returns (bool);
    }
}

// ---------------------------------------------------------------------------
// GPv2 order hashing (enough for presign — no EIP-712 user signature needed)
// ---------------------------------------------------------------------------

/// GPv2's EIP-712 order type hash.
fn order_type_hash() -> B256 {
    keccak256(
        "Order(address sellToken,address buyToken,address receiver,uint256 sellAmount,uint256 \
         buyAmount,uint32 validTo,bytes32 appData,uint256 feeAmount,string kind,bool \
         partiallyFillable,string sellTokenBalance,string buyTokenBalance)",
    )
}

/// EIP-712 struct hash of a GPv2 sell order with `erc20` balance locations.
fn gpv2_struct_hash(
    sell_token: Address,
    buy_token: Address,
    receiver: Address,
    sell_amount: U256,
    buy_amount: U256,
    valid_to: u32,
    partially_fillable: bool,
) -> B256 {
    let mut buf = Vec::with_capacity(14 * 32);
    let addr = |a: Address| B256::left_padding_from(a.as_slice());
    buf.extend_from_slice(order_type_hash().as_slice());
    buf.extend_from_slice(addr(sell_token).as_slice());
    buf.extend_from_slice(addr(buy_token).as_slice());
    buf.extend_from_slice(addr(receiver).as_slice());
    buf.extend_from_slice(&sell_amount.to_be_bytes::<32>());
    buf.extend_from_slice(&buy_amount.to_be_bytes::<32>());
    buf.extend_from_slice(&U256::from(valid_to).to_be_bytes::<32>());
    buf.extend_from_slice(B256::ZERO.as_slice()); // appData
    buf.extend_from_slice(&U256::ZERO.to_be_bytes::<32>()); // feeAmount
    buf.extend_from_slice(keccak256("sell").as_slice());
    buf.extend_from_slice(&U256::from(partially_fillable as u64).to_be_bytes::<32>());
    buf.extend_from_slice(keccak256("erc20").as_slice());
    buf.extend_from_slice(keccak256("erc20").as_slice());
    keccak256(&buf)
}

/// `keccak256("\x19\x01" || domainSeparator || structHash)`.
fn eip712_digest(domain_separator: B256, struct_hash: B256) -> B256 {
    let mut buf = [0u8; 66];
    buf[0] = 0x19;
    buf[1] = 0x01;
    buf[2..34].copy_from_slice(domain_separator.as_slice());
    buf[34..66].copy_from_slice(struct_hash.as_slice());
    keccak256(buf)
}

/// GPv2 order UID: `orderDigest ++ owner ++ validTo` (56 bytes).
fn order_uid(digest: B256, owner: Address, valid_to: u32) -> [u8; 56] {
    let mut uid = [0u8; 56];
    uid[..32].copy_from_slice(digest.as_slice());
    uid[32..52].copy_from_slice(owner.as_slice());
    uid[52..56].copy_from_slice(&valid_to.to_be_bytes());
    uid
}

// ---------------------------------------------------------------------------
// Settlement calldata builder (no trampoline, same-token, presign)
// ---------------------------------------------------------------------------

/// Build `settle()` calldata for a partial fill of a same-token presigned
/// sell order with no interactions. Clearing prices are 1:1 (same token),
/// so the user gets back exactly `executed_amount` of the same token.
fn partial_fill_settle_calldata(
    sell_amount: U256,
    buy_amount: U256,
    valid_to: u32,
    executed_amount: U256,
    owner: Address,
) -> Bytes {
    // Flags: sell (bit 0 = 0), partially fillable (bit 1),
    // erc20 balances (bits 2-4 = 0), presign (bits 5-6 = 0b11).
    let flags = U256::from(2u64 | (3u64 << 5));

    GPv2Settlement::settleCall {
        tokens: vec![WETH, WETH],
        clearingPrices: vec![executed_amount, executed_amount],
        trades: vec![GPv2TradeData {
            sellTokenIndex: U256::ZERO,
            buyTokenIndex: U256::ONE,
            receiver: Address::ZERO,
            sellAmount: sell_amount,
            buyAmount: buy_amount,
            validTo: valid_to,
            appData: B256::ZERO,
            feeAmount: U256::ZERO,
            flags,
            executedAmount: executed_amount,
            // Presign signature = 20-byte owner address.
            signature: Bytes::copy_from_slice(owner.as_slice()),
        }],
        interactions: [Vec::<GPv2InteractionData>::new(), Vec::new(), Vec::new()],
    }
    .abi_encode()
    .into()
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Settles a partially fillable order in two rounds and verifies GPv2 accepts
/// both: the first at 50% fill, the second at 30% (cumulative 80%).
#[tokio::test]
#[ignore = "tier-1 e2e: needs anvil and the offline-mode submodule"]
async fn partial_fill_settlement_accepted_by_gpv2() {
    let chain = Chain::spawn().await.expect("chain fixture should boot");

    let user = chain.anvil().addresses()[3];
    let user_signer: PrivateKeySigner = chain.anvil().keys()[3].clone().into();
    let user_provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(user_signer))
        .connect_http(chain.anvil().endpoint_url())
        .erased();

    // -- setup: wrap ETH and approve the vault relayer -----------------------

    let weth = IWETH::new(WETH, &user_provider);
    let sell_amount = U256::from(1_000_000_000_000_000_000u128); // 1 WETH
    weth.deposit()
        .value(sell_amount * U256::from(2)) // 2 WETH headroom
        .send()
        .await
        .expect("WETH deposit tx")
        .get_receipt()
        .await
        .expect("WETH deposit receipt");
    weth.approve(VAULT_RELAYER, U256::MAX)
        .send()
        .await
        .expect("WETH approve tx")
        .get_receipt()
        .await
        .expect("WETH approve receipt");

    // -- presign the partially fillable order on GPv2 ------------------------

    let buy_amount = U256::from(900_000_000_000_000_000u128); // 0.9 WETH limit
    let valid_to = u32::MAX;

    let settlement = ISettlement::new(GPV2_SETTLEMENT, &user_provider);
    let domain_separator: B256 = settlement
        .domainSeparator()
        .call()
        .await
        .expect("domainSeparator read");

    let struct_hash = gpv2_struct_hash(
        WETH,
        WETH,
        Address::ZERO,
        sell_amount,
        buy_amount,
        valid_to,
        true,
    );
    let digest = eip712_digest(domain_separator, struct_hash);
    let uid = order_uid(digest, user, valid_to);

    settlement
        .setPreSignature(Bytes::from(uid.to_vec()), true)
        .send()
        .await
        .expect("presign tx")
        .get_receipt()
        .await
        .expect("presign receipt");

    // -- first partial fill: 50% of the order --------------------------------

    let fill_1 = U256::from(500_000_000_000_000_000u128); // 0.5 WETH
    let calldata_1 = partial_fill_settle_calldata(sell_amount, buy_amount, valid_to, fill_1, user);

    let receipt_1 = chain
        .provider()
        .send_transaction(
            TransactionRequest::default()
                .to(GPV2_SETTLEMENT)
                .input(calldata_1.into()),
        )
        .await
        .expect("send first partial fill")
        .get_receipt()
        .await
        .expect("first partial fill receipt");

    assert!(
        receipt_1.status(),
        "first partial fill (50%) should succeed on GPv2"
    );

    // -- second partial fill: 30% (cumulative 80% < 100%) --------------------

    let fill_2 = U256::from(300_000_000_000_000_000u128); // 0.3 WETH
    let calldata_2 = partial_fill_settle_calldata(sell_amount, buy_amount, valid_to, fill_2, user);

    let receipt_2 = chain
        .provider()
        .send_transaction(
            TransactionRequest::default()
                .to(GPV2_SETTLEMENT)
                .input(calldata_2.into()),
        )
        .await
        .expect("send second partial fill")
        .get_receipt()
        .await
        .expect("second partial fill receipt");

    assert!(
        receipt_2.status(),
        "second partial fill (30%, cumulative 80%) should succeed on GPv2"
    );
}
