//! Tier-1 e2e: partially fillable order with hooks.
//!
//! Combines the hooks (COW-1243) and partial fills (COW-1244) features:
//! a partially fillable USDC→USDC sell order is presigned on GPv2 and
//! settled in two rounds. The first fill (50%) includes both pre and post
//! hooks; the second fill (30%) includes only the post-hook — matching the
//! CoW Protocol social consensus: pre-hooks run only on the first fill,
//! post-hooks run on every fill.
//!
//! The pre-hook is an ERC-2612 `permit` that grants VAULT_RELAYER the full
//! sell amount. If it does not execute on the first fill, the settlement
//! reverts for lack of approval. On the second fill, the remaining
//! allowance is already in place (permit granted more than the first fill
//! consumed), so skipping the pre-hook is correct.

use {
    alloy::{
        network::EthereumWallet,
        primitives::{Address, B256, Bytes, U256, address, keccak256},
        providers::{Provider, ProviderBuilder},
        rpc::types::TransactionRequest,
        signers::{Signer, local::PrivateKeySigner},
        sol,
        sol_types::{SolCall, SolConstructor},
    },
    byos_common::contracts::{
        GPv2InteractionData,
        GPv2Settlement,
        GPv2TradeData,
        HooksTrampoline as HooksTrampolineBindings,
    },
    e2e::chain::{Chain, GPV2_SETTLEMENT},
};

/// USDC at its mainnet address (TestUSDC with EIP-2612 permit in the
/// offline-mode state). 6 decimals.
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

/// WETH at its mainnet address (present in the offline-mode state).
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

/// The GPv2VaultRelayer — the spender the user must approve.
const VAULT_RELAYER: Address = address!("C92E8bdf79f0507f65a392b0ab4667716BFE0110");

sol! {
    #[sol(rpc)]
    interface ISettlement {
        function domainSeparator() external view returns (bytes32);
        function setPreSignature(bytes calldata orderUid, bool signed) external;
    }

    #[sol(rpc)]
    #[allow(clippy::too_many_arguments)]
    interface IERC20Permit {
        function permit(address owner, address spender, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s) external;
        function nonces(address owner) external view returns (uint256);
        function DOMAIN_SEPARATOR() external view returns (bytes32);
        function balanceOf(address owner) external view returns (uint256);
    }
}

sol!(
    HooksTrampolineArtifact,
    "../byos-common/abis/HooksTrampoline.json"
);

// ---------------------------------------------------------------------------
// GPv2 order hashing
// ---------------------------------------------------------------------------

fn order_type_hash() -> B256 {
    keccak256(
        "Order(address sellToken,address buyToken,address receiver,uint256 sellAmount,uint256 \
         buyAmount,uint32 validTo,bytes32 appData,uint256 feeAmount,string kind,bool \
         partiallyFillable,string sellTokenBalance,string buyTokenBalance)",
    )
}

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

fn eip712_digest(domain_separator: B256, struct_hash: B256) -> B256 {
    let mut buf = [0u8; 66];
    buf[0] = 0x19;
    buf[1] = 0x01;
    buf[2..34].copy_from_slice(domain_separator.as_slice());
    buf[34..66].copy_from_slice(struct_hash.as_slice());
    keccak256(buf)
}

fn order_uid(digest: B256, owner: Address, valid_to: u32) -> [u8; 56] {
    let mut uid = [0u8; 56];
    uid[..32].copy_from_slice(digest.as_slice());
    uid[32..52].copy_from_slice(owner.as_slice());
    uid[52..56].copy_from_slice(&valid_to.to_be_bytes());
    uid
}

/// CREATE2 singleton factory, present in the offline-mode state.
const CREATE2_FACTORY: Address = address!("4e59b44847b379578588920cA78FbF26c0B4956C");

// ---------------------------------------------------------------------------
// EIP-2612 permit signing
// ---------------------------------------------------------------------------

fn permit_typehash() -> B256 {
    keccak256("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)")
}

async fn sign_permit_calldata(
    signer: &PrivateKeySigner,
    token_domain_separator: B256,
    owner: Address,
    spender: Address,
    value: U256,
    nonce: U256,
    deadline: U256,
) -> Bytes {
    let struct_hash = keccak256(
        [
            permit_typehash().as_slice(),
            B256::left_padding_from(owner.as_slice()).as_slice(),
            B256::left_padding_from(spender.as_slice()).as_slice(),
            &value.to_be_bytes::<32>(),
            &nonce.to_be_bytes::<32>(),
            &deadline.to_be_bytes::<32>(),
        ]
        .concat(),
    );
    let digest = eip712_digest(token_domain_separator, struct_hash);
    let sig = signer.sign_hash(&digest).await.expect("permit signing");

    IERC20Permit::permitCall {
        owner,
        spender,
        value,
        deadline,
        v: sig.v() as u8,
        r: sig.r().into(),
        s: sig.s().into(),
    }
    .abi_encode()
    .into()
}

// ---------------------------------------------------------------------------
// HooksTrampoline deployment
// ---------------------------------------------------------------------------

async fn deploy_hooks_trampoline(
    provider: &alloy::providers::DynProvider,
) -> anyhow::Result<Address> {
    let constructor = HooksTrampolineArtifact::constructorCall {
        settlement_: GPV2_SETTLEMENT,
    };
    let init_code = [
        HooksTrampolineArtifact::BYTECODE.to_vec(),
        constructor.abi_encode(),
    ]
    .concat();
    let salt = B256::left_padding_from(b"partial-hooks-e2e");
    let addr = CREATE2_FACTORY.create2_from_code(salt, &init_code);

    let calldata = [salt.as_slice(), &init_code].concat();
    let receipt = provider
        .send_transaction(
            TransactionRequest::default()
                .to(CREATE2_FACTORY)
                .input(Bytes::from(calldata).into()),
        )
        .await?
        .get_receipt()
        .await?;

    anyhow::ensure!(receipt.status(), "HooksTrampoline deployment reverted");
    Ok(addr)
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Settles a partially fillable USDC→USDC order in two rounds:
///
/// 1. First fill (50%): pre-hook (permit) + post-hook — proves hooks execute on
///    the first partial fill.
/// 2. Second fill (30%): post-hook only — proves the CoW social consensus that
///    pre-hooks run only on the first fill, and post-hooks run on every fill.
///    The remaining allowance from the permit covers this fill.
#[tokio::test]
#[ignore = "tier-1 e2e: needs anvil and the offline-mode submodule"]
async fn partial_fill_with_hooks_settlement() {
    let chain = Chain::spawn().await.expect("chain fixture should boot");

    // -- deploy HooksTrampoline ----------------------------------------------

    let hooks_trampoline = deploy_hooks_trampoline(chain.provider())
        .await
        .expect("HooksTrampoline deploys");

    // -- setup user ----------------------------------------------------------

    let user = chain.anvil().addresses()[3];
    let user_signer: PrivateKeySigner = chain.anvil().keys()[3].clone().into();
    let user_provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(user_signer.clone()))
        .connect_http(chain.anvil().endpoint_url())
        .erased();

    // USDC has 6 decimals. Account #3 has 75K USDC in the offline-mode state.
    let sell_amount = U256::from(1_000_000_000u64); // 1000 USDC
    let buy_amount = U256::from(900_000_000u64); // 900 USDC limit

    let usdc = IERC20Permit::new(USDC, &user_provider);

    // Sanity: the user has USDC but NO vault-relayer allowance.
    let balance: U256 = usdc.balanceOf(user).call().await.expect("balanceOf");
    assert!(
        balance >= sell_amount,
        "user must have enough USDC (has {balance}, need {sell_amount})"
    );
    // No approve() call — the permit pre-hook is the only source of allowance.

    // -- presign the partially fillable order --------------------------------

    let valid_to = u32::MAX;

    let settlement = ISettlement::new(GPV2_SETTLEMENT, &user_provider);
    let domain_separator: B256 = settlement
        .domainSeparator()
        .call()
        .await
        .expect("domainSeparator read");

    let struct_hash = gpv2_struct_hash(
        USDC,
        USDC,
        Address::ZERO,
        sell_amount,
        buy_amount,
        valid_to,
        true, // partially fillable
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

    // -- encode hooks --------------------------------------------------------

    // Pre-hook: ERC-2612 permit granting VAULT_RELAYER the FULL sell amount.
    // This covers both fills. If it doesn't run on the first fill, the trade
    // reverts for lack of approval.
    let usdc_domain_separator: B256 = usdc
        .DOMAIN_SEPARATOR()
        .call()
        .await
        .expect("USDC DOMAIN_SEPARATOR");
    let nonce: U256 = usdc.nonces(user).call().await.expect("USDC nonces");
    let permit_deadline = U256::from(u64::MAX);

    let permit_calldata = sign_permit_calldata(
        &user_signer,
        usdc_domain_separator,
        user,
        VAULT_RELAYER,
        sell_amount, // permit the full amount — covers both partial fills
        nonce,
        permit_deadline,
    )
    .await;

    let pre_hook = HooksTrampolineBindings::Hook {
        target: USDC,
        callData: permit_calldata,
        gasLimit: U256::from(100_000u64),
    };

    let pre_interaction = GPv2InteractionData {
        target: hooks_trampoline,
        value: U256::ZERO,
        callData: (HooksTrampolineBindings::executeCall {
            hooks: vec![pre_hook],
        })
        .abi_encode()
        .into(),
    };

    // Post-hook: WETH.symbol() — benign view call exercising the post slot.
    let post_hook = HooksTrampolineBindings::Hook {
        target: WETH,
        callData: Bytes::from(vec![0x95, 0xd8, 0x9b, 0x41]), // symbol()
        gasLimit: U256::from(50_000u64),
    };

    let post_interaction = GPv2InteractionData {
        target: hooks_trampoline,
        value: U256::ZERO,
        callData: (HooksTrampolineBindings::executeCall {
            hooks: vec![post_hook],
        })
        .abi_encode()
        .into(),
    };

    // -- first partial fill: 50% with pre + post hooks -----------------------

    // Flags: sell (bit 0 = 0), partially fillable (bit 1),
    // erc20 balances (bits 2-4 = 0), presign (bits 5-6 = 0b11).
    let flags = U256::from(2u64 | (3u64 << 5));
    let fill_1 = U256::from(500_000_000u64); // 500 USDC = 50%

    let calldata_1: Bytes = GPv2Settlement::settleCall {
        tokens: vec![USDC, USDC],
        clearingPrices: vec![fill_1, fill_1], // 1:1 same token
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
            executedAmount: fill_1,
            signature: Bytes::copy_from_slice(user.as_slice()),
        }],
        interactions: [
            vec![pre_interaction],          // pre-hook: permit
            vec![],                         // intra: no trampoline for same-token
            vec![post_interaction.clone()], // post-hook: WETH.symbol()
        ],
    }
    .abi_encode()
    .into();

    let receipt_1 = chain
        .provider()
        .send_transaction(
            TransactionRequest::default()
                .to(GPV2_SETTLEMENT)
                .input(calldata_1.into()),
        )
        .await
        .expect("send first partial fill with hooks")
        .get_receipt()
        .await
        .expect("first partial fill receipt");

    assert!(
        receipt_1.status(),
        "first partial fill (50%) with pre-hook permit must succeed — the permit is the only \
         source of vault-relayer allowance"
    );

    // -- second partial fill: 30% with only post-hook ------------------------
    // Pre-hook is NOT included: the CoW social consensus is that pre-hooks
    // run only on the first fill. The remaining allowance from the permit
    // (500 USDC unused) covers this fill.

    let fill_2 = U256::from(300_000_000u64); // 300 USDC = 30%

    let calldata_2: Bytes = GPv2Settlement::settleCall {
        tokens: vec![USDC, USDC],
        clearingPrices: vec![fill_2, fill_2],
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
            executedAmount: fill_2,
            signature: Bytes::copy_from_slice(user.as_slice()),
        }],
        interactions: [
            vec![],                 // NO pre-hook on second fill
            vec![],                 // intra: no trampoline
            vec![post_interaction], // post-hook still runs
        ],
    }
    .abi_encode()
    .into();

    let receipt_2 = chain
        .provider()
        .send_transaction(
            TransactionRequest::default()
                .to(GPV2_SETTLEMENT)
                .input(calldata_2.into()),
        )
        .await
        .expect("send second partial fill (post-hook only)")
        .get_receipt()
        .await
        .expect("second partial fill receipt");

    assert!(
        receipt_2.status(),
        "second partial fill (30%, cumulative 80%) with only post-hook must succeed — the permit \
         from the first fill already granted enough allowance"
    );
}
