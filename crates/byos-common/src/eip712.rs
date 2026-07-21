//! EIP-712 schema for proposal signing and verification.
//!
//! Sub-solvers sign a [`ProposalData`] message (6 fields, including
//! `interactionsHash`) using the EIP-712 domain anchored to the
//! [`TrampolineFactory`](crate::contracts::TrampolineFactory) contract. The
//! BYOS service verifies these signatures to authenticate proposals; the
//! signing helper is used by the reference sub-solver and tests.
//!
//! The canonical Solidity implementation is in
//! `test/utils/ProposalSigning.sol` in byos-contracts.

use {
    crate::contracts::{Interaction, Proposal},
    alloy::{
        primitives::{Address, B256, Signature, U256},
        signers::Signer,
        sol,
        sol_types::{Eip712Domain, SolStruct, SolType},
    },
};

/// EIP-712 domain name, matching the on-chain `EIP712("BYOS", "0.1")` in
/// TrampolineFactory.
pub const DOMAIN_NAME: &str = "BYOS";

/// EIP-712 domain version, matching the on-chain constructor.
pub const DOMAIN_VERSION: &str = "0.1";

sol! {
    /// The full EIP-712 type that sub-solvers sign. Includes `interactionsHash`
    /// (computed off-chain from the interaction list), unlike the on-chain
    /// [`Proposal`] struct which omits it (recomputed from actual calldata).
    ///
    /// Type string:
    /// ```text
    /// ProposalData(bytes32 orderUidHash,uint256 sellAmount,uint256 buyAmount,
    ///              bytes32 interactionsHash,uint256 validUntil,uint256 nonce)
    /// ```
    struct ProposalData {
        bytes32 orderUidHash;
        uint256 sellAmount;
        uint256 buyAmount;
        bytes32 interactionsHash;
        uint256 validUntil;
        uint256 nonce;
    }
}

/// The expected type hash, verified at test time against the alloy-derived
/// value. Matches the on-chain `PROPOSAL_TYPEHASH` in ITrampoline.sol.
pub const PROPOSAL_TYPEHASH: B256 =
    alloy::primitives::b256!("2045708f2cdb91d16aa77dec29e1d20d5d7bdc6bbbc2a4158457a9d0be739209");

/// Builds the BYOS EIP-712 domain for a specific chain and TrampolineFactory.
pub fn byos_domain(chain_id: u64, verifying_contract: Address) -> Eip712Domain {
    Eip712Domain {
        name: Some(DOMAIN_NAME.into()),
        version: Some(DOMAIN_VERSION.into()),
        chain_id: Some(U256::from(chain_id)),
        verifying_contract: Some(verifying_contract),
        salt: None,
    }
}

/// Computes `interactionsHash = keccak256(abi.encode(interactions))`, matching
/// the Solidity `keccak256(abi.encode(interactions))` in ProposalSigning.sol.
pub fn compute_interactions_hash(interactions: &[Interaction]) -> B256 {
    let encoded =
        <alloy::sol_types::sol_data::Array<Interaction> as SolType>::abi_encode(interactions);
    alloy::primitives::keccak256(&encoded)
}

/// Constructs a [`ProposalData`] from a [`Proposal`] and a precomputed
/// `interactions_hash`.
pub fn proposal_data(proposal: &Proposal, interactions_hash: B256) -> ProposalData {
    ProposalData {
        orderUidHash: proposal.orderUidHash,
        sellAmount: proposal.sellAmount,
        buyAmount: proposal.buyAmount,
        interactionsHash: interactions_hash,
        validUntil: proposal.validUntil,
        nonce: proposal.nonce,
    }
}

/// Signs a proposal on behalf of a sub-solver. Returns the raw signature bytes.
///
/// Used by the reference sub-solver and tests.
pub async fn sign_proposal<S: Signer>(
    signer: &S,
    domain: &Eip712Domain,
    proposal: &Proposal,
    interactions: &[Interaction],
) -> alloy::signers::Result<Signature> {
    let interactions_hash = compute_interactions_hash(interactions);
    let data = proposal_data(proposal, interactions_hash);
    let hash = data.eip712_signing_hash(domain);
    signer.sign_hash(&hash).await
}

sol! {
    /// EIP-712 type for proposal cancellation. Signed by the sub-solver to
    /// authorize DELETE /proposals/{id}. API-only type, not on-chain.
    struct CancelProposal {
        uint256 proposalId;
    }
}

/// Signs a proposal cancellation on behalf of a sub-solver. Returns the raw
/// signature bytes.
///
/// Used by the reference sub-solver and tests.
pub async fn sign_cancellation<S: Signer>(
    signer: &S,
    domain: &Eip712Domain,
    proposal_id: U256,
) -> alloy::signers::Result<Signature> {
    let cancel = CancelProposal {
        proposalId: proposal_id,
    };
    let hash = cancel.eip712_signing_hash(domain);
    signer.sign_hash(&hash).await
}

/// Recovers the sub-solver address from a cancellation signature.
pub fn recover_canceller(
    signature: &Signature,
    domain: &Eip712Domain,
    proposal_id: U256,
) -> Result<Address, alloy::primitives::SignatureError> {
    let cancel = CancelProposal {
        proposalId: proposal_id,
    };
    let signing_hash = cancel.eip712_signing_hash(domain);
    signature.recover_address_from_prehash(&signing_hash)
}

sol! {
    /// EIP-712 type for read authentication. Signed once by the sub-solver and
    /// sent as a bearer token in the `X-Signature` header on GET endpoints.
    /// API-only type, not on-chain. No timestamp or nonce: a leaked signature
    /// only grants read access to the signer's own proposals (ADR-0011).
    struct ReadAuth {
        uint256 version;
    }
}

/// Current `ReadAuth.version`. Bumping it invalidates all outstanding read
/// tokens without changing the type name.
pub const READ_AUTH_VERSION: u64 = 1;

/// Signs the read-auth bearer message. Returns the raw signature bytes.
///
/// Used by the reference sub-solver and tests.
pub async fn sign_read_auth<S: Signer>(
    signer: &S,
    domain: &Eip712Domain,
) -> alloy::signers::Result<Signature> {
    let auth = ReadAuth {
        version: U256::from(READ_AUTH_VERSION),
    };
    let hash = auth.eip712_signing_hash(domain);
    signer.sign_hash(&hash).await
}

/// Recovers the sub-solver address from a read-auth signature.
///
/// A signature over any other version recovers a different (unknown) address
/// and simply fails the caller's ownership checks — no explicit version check
/// is needed.
pub fn recover_reader(
    signature: &Signature,
    domain: &Eip712Domain,
) -> Result<Address, alloy::primitives::SignatureError> {
    let auth = ReadAuth {
        version: U256::from(READ_AUTH_VERSION),
    };
    let signing_hash = auth.eip712_signing_hash(domain);
    signature.recover_address_from_prehash(&signing_hash)
}

/// Recovers the sub-solver address that signed a proposal.
///
/// Returns `Err` if the signature is invalid (does not recover to a valid
/// address). The caller should compare the recovered address against the
/// expected sub-solver.
pub fn recover_proposer(
    signature: &Signature,
    domain: &Eip712Domain,
    proposal: &Proposal,
    interactions_hash: B256,
) -> Result<Address, alloy::primitives::SignatureError> {
    let data = proposal_data(proposal, interactions_hash);
    let signing_hash = data.eip712_signing_hash(domain);
    signature.recover_address_from_prehash(&signing_hash)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        alloy::{
            primitives::{Address, U256, address, b256, keccak256},
            signers::local::PrivateKeySigner,
        },
    };

    #[test]
    fn type_hash_matches_on_chain_constant() {
        // eip712_type_hash is an instance method but depends only on the type,
        // so we use a zero-valued instance.
        let dummy = ProposalData {
            orderUidHash: B256::ZERO,
            sellAmount: U256::ZERO,
            buyAmount: U256::ZERO,
            interactionsHash: B256::ZERO,
            validUntil: U256::ZERO,
            nonce: U256::ZERO,
        };

        let derived = dummy.eip712_type_hash();
        assert_eq!(
            derived, PROPOSAL_TYPEHASH,
            "alloy-derived type hash does not match on-chain PROPOSAL_TYPEHASH"
        );

        // Double-check against manual keccak256 of the type string.
        let manual = keccak256(
            "ProposalData(bytes32 orderUidHash,uint256 sellAmount,uint256 buyAmount,bytes32 \
             interactionsHash,uint256 validUntil,uint256 nonce)",
        );
        assert_eq!(derived, manual, "type hash does not match manual keccak256");
    }

    #[test]
    fn interactions_hash_empty() {
        let hash = compute_interactions_hash(&[]);
        // keccak256(abi.encode([])) = keccak256 of the ABI encoding of an empty
        // dynamic array: offset (32 bytes) + length 0 (32 bytes).
        // Alloy's SolType::abi_encode for an empty array produces the encoded
        // form; verify it's deterministic and non-zero.
        assert_ne!(hash, B256::ZERO);
    }

    #[test]
    fn interactions_hash_deterministic() {
        let interactions = vec![Interaction {
            target: address!("0000000000000000000000000000000000000001"),
            value: U256::ZERO,
            callData: vec![0xab, 0xcd].into(),
        }];
        let h1 = compute_interactions_hash(&interactions);
        let h2 = compute_interactions_hash(&interactions);
        assert_eq!(h1, h2);
    }

    #[tokio::test]
    async fn sign_and_recover_round_trip() {
        let signer = PrivateKeySigner::random();
        let sub_solver: Address = signer.address();

        let factory = address!("00000000000000000000000000000000DeaDBeef");
        let domain = byos_domain(1, factory);

        let proposal = Proposal {
            orderUidHash: b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            sellAmount: U256::from(1_000_000u64),
            buyAmount: U256::from(990_000u64),
            validUntil: U256::from(1_700_000_000u64),
            nonce: U256::from(42u64),
        };

        let interactions = vec![Interaction {
            target: address!("0000000000000000000000000000000000000001"),
            value: U256::ZERO,
            callData: vec![0x01, 0x02, 0x03].into(),
        }];

        let sig = sign_proposal(&signer, &domain, &proposal, &interactions)
            .await
            .expect("signing should succeed");

        let interactions_hash = compute_interactions_hash(&interactions);
        let recovered = recover_proposer(&sig, &domain, &proposal, interactions_hash)
            .expect("recovery should succeed");

        assert_eq!(recovered, sub_solver, "recovered address must match signer");
    }

    #[tokio::test]
    async fn cancel_proposal_sign_and_recover() {
        let signer = PrivateKeySigner::random();
        let sub_solver: Address = signer.address();

        let factory = address!("00000000000000000000000000000000DeaDBeef");
        let domain = byos_domain(1, factory);

        let proposal_id = U256::from(42u64);
        let sig = sign_cancellation(&signer, &domain, proposal_id)
            .await
            .expect("signing should succeed");

        let recovered =
            recover_canceller(&sig, &domain, proposal_id).expect("recovery should succeed");
        assert_eq!(recovered, sub_solver);
    }

    #[tokio::test]
    async fn read_auth_sign_and_recover() {
        let signer = PrivateKeySigner::random();
        let sub_solver: Address = signer.address();

        let factory = address!("00000000000000000000000000000000DeaDBeef");
        let domain = byos_domain(1, factory);

        let sig = sign_read_auth(&signer, &domain)
            .await
            .expect("signing should succeed");

        let recovered = recover_reader(&sig, &domain).expect("recovery should succeed");
        assert_eq!(recovered, sub_solver);
    }

    #[tokio::test]
    async fn wrong_interactions_hash_recovers_wrong_address() {
        let signer = PrivateKeySigner::random();
        let sub_solver: Address = signer.address();

        let factory = address!("00000000000000000000000000000000DeaDBeef");
        let domain = byos_domain(1, factory);

        let proposal = Proposal {
            orderUidHash: b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            sellAmount: U256::from(500_000u64),
            buyAmount: U256::from(495_000u64),
            validUntil: U256::from(1_700_000_000u64),
            nonce: U256::from(1u64),
        };

        let interactions = vec![Interaction {
            target: address!("0000000000000000000000000000000000000002"),
            value: U256::ZERO,
            callData: vec![].into(),
        }];

        let sig = sign_proposal(&signer, &domain, &proposal, &interactions)
            .await
            .expect("signing should succeed");

        // Use a different interactions hash for recovery.
        let wrong_hash = B256::ZERO;
        let recovered = recover_proposer(&sig, &domain, &proposal, wrong_hash)
            .expect("recovery itself succeeds");

        assert_ne!(
            recovered, sub_solver,
            "wrong interactions hash should recover a different address"
        );
    }
}
