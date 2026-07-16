//! EIP-712 signing of proposals. The `ProposalData` schema, typehash, and
//! domain are owned by `bleu/byos-contracts` (its ADR-0005); this module must
//! produce hashes that verify inside `Trampoline.execute`, so it is tested
//! against the contract-generated vectors in `testdata/proposal-eip712.json`
//! and never re-derives the schema from local constants.

use alloy::{
    primitives::{Address, B256, Bytes, U256, keccak256},
    signers::{SignerSync, local::PrivateKeySigner},
    sol,
    sol_types::{Eip712Domain, SolStruct, SolValue, eip712_domain},
};

sol! {
    /// The signed proposal struct, verbatim from `ITrampoline.sol`. The
    /// `interactionsHash` field commits to the route; the Trampoline
    /// recomputes it on-chain from the interactions actually executed.
    struct ProposalData {
        bytes32 orderUidHash;
        uint256 sellAmount;
        uint256 buyAmount;
        bytes32 interactionsHash;
        uint256 validUntil;
        uint256 nonce;
    }

    /// Mirror of `ITrampoline.Interaction`, needed only to reproduce the
    /// contract's `keccak256(abi.encode(_interactions))` commitment.
    struct SolInteraction {
        address target;
        uint256 value;
        bytes callData;
    }

    /// API-authentication type for `DELETE /proposals/{id}` (ADR-0001).
    /// Signed in the same domain as proposals but never verified on-chain,
    /// so this repo owns it.
    struct CancelProposal {
        uint256 proposalId;
    }
}

/// The digest signed to cancel proposal `proposal_id`.
pub fn cancellation_digest(proposal_id: u64, domain: &Eip712Domain) -> B256 {
    CancelProposal {
        proposalId: U256::from(proposal_id),
    }
    .eip712_signing_hash(domain)
}

/// Signs a `CancelProposal` message for `DELETE /proposals/{id}`.
pub fn sign_cancellation(
    proposal_id: u64,
    domain: &Eip712Domain,
    signer: &PrivateKeySigner,
) -> Bytes {
    let signature = signer
        .sign_hash_sync(&cancellation_digest(proposal_id, domain))
        .expect("in-memory ECDSA signing is infallible");
    signature.as_bytes().into()
}

/// The EIP-712 domain proposal signatures are verified against: name "BYOS",
/// version "0.1", anchored to the chain's TrampolineFactory.
pub fn proposal_domain(chain_id: u64, trampoline_factory: Address) -> Eip712Domain {
    eip712_domain! {
        name: "BYOS",
        version: "0.1",
        chain_id: chain_id,
        verifying_contract: trampoline_factory,
    }
}

/// The `ProposalData` typehash as derived from the struct definition above.
pub fn proposal_typehash() -> B256 {
    ProposalData::eip712_type_hash(&ProposalData {
        orderUidHash: B256::ZERO,
        sellAmount: U256::ZERO,
        buyAmount: U256::ZERO,
        interactionsHash: B256::ZERO,
        validUntil: U256::ZERO,
        nonce: U256::ZERO,
    })
}

/// The route commitment: `keccak256(abi.encode(interactions))`, exactly as
/// `Trampoline.execute` recomputes it.
pub fn interactions_hash(interactions: &[proposal_dto::Interaction]) -> B256 {
    let interactions: Vec<SolInteraction> = interactions
        .iter()
        .map(|interaction| SolInteraction {
            target: interaction.target,
            value: interaction.value,
            callData: interaction.call_data.clone(),
        })
        .collect();
    keccak256(interactions.abi_encode())
}

/// A proposal's signed fields, before signing. Amounts and interactions are
/// the wire types from `proposal-dto`: what goes on the wire is exactly what
/// gets signed, so there is no separate domain model to drift from it.
pub struct UnsignedProposal<'a> {
    pub order_uid: &'a Bytes,
    pub sell_amount: U256,
    pub buy_amount: U256,
    pub interactions: &'a [proposal_dto::Interaction],
    pub valid_until: u64,
    pub nonce: U256,
}

impl UnsignedProposal<'_> {
    fn proposal_data(&self) -> ProposalData {
        ProposalData {
            orderUidHash: keccak256(self.order_uid),
            sellAmount: self.sell_amount,
            buyAmount: self.buy_amount,
            interactionsHash: interactions_hash(self.interactions),
            validUntil: U256::from(self.valid_until),
            nonce: self.nonce,
        }
    }

    /// The EIP-712 struct hash of the proposal.
    pub fn struct_hash(&self) -> B256 {
        self.proposal_data().eip712_hash_struct()
    }

    /// The digest the sub-solver signs: `toTypedDataHash(domainSeparator,
    /// structHash)`.
    pub fn signing_digest(&self, domain: &Eip712Domain) -> B256 {
        self.proposal_data().eip712_signing_hash(domain)
    }

    /// Signs the proposal, returning the 65-byte `r || s || v` signature the
    /// Trampoline's `ECDSA.recover` expects.
    pub fn sign(&self, domain: &Eip712Domain, signer: &PrivateKeySigner) -> Bytes {
        let signature = signer
            .sign_hash_sync(&self.signing_digest(domain))
            .expect("in-memory ECDSA signing is infallible");
        signature.as_bytes().into()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        alloy::{
            primitives::{Address, B256, Bytes, U256},
            signers::local::PrivateKeySigner,
        },
        serde::Deserialize,
        serde_with::{DisplayFromStr, serde_as},
    };

    /// Mirror of the vector file emitted by byos-contracts' `Eip712Vectors`.
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct VectorFile {
        domain: Domain,
        proposal_typehash: B256,
        sub_solver: SubSolver,
        vectors: Vec<Vector>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Domain {
        chain_id: u64,
        verifying_contract: Address,
        domain_separator: B256,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct SubSolver {
        address: Address,
        private_key: B256,
    }

    #[serde_as]
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Vector {
        order_uid: Bytes,
        #[serde_as(as = "DisplayFromStr")]
        sell_amount: U256,
        #[serde_as(as = "DisplayFromStr")]
        buy_amount: U256,
        valid_until: u64,
        #[serde_as(as = "DisplayFromStr")]
        nonce: U256,
        interactions: Vec<proposal_dto::Interaction>,
        interactions_hash: B256,
        struct_hash: B256,
        digest: B256,
        signature: Bytes,
    }

    fn vectors() -> VectorFile {
        serde_json::from_str(include_str!("../../testdata/proposal-eip712.json")).unwrap()
    }

    #[test]
    fn signing_matches_the_contract_vectors() {
        let file = vectors();
        let domain = proposal_domain(file.domain.chain_id, file.domain.verifying_contract);
        assert_eq!(domain.separator(), file.domain.domain_separator);

        let signer = PrivateKeySigner::from_bytes(&file.sub_solver.private_key).unwrap();
        assert_eq!(signer.address(), file.sub_solver.address);

        for vector in file.vectors {
            let unsigned = UnsignedProposal {
                order_uid: &vector.order_uid,
                sell_amount: vector.sell_amount,
                buy_amount: vector.buy_amount,
                interactions: &vector.interactions,
                valid_until: vector.valid_until,
                nonce: vector.nonce,
            };

            assert_eq!(
                interactions_hash(&vector.interactions),
                vector.interactions_hash
            );
            assert_eq!(unsigned.struct_hash(), vector.struct_hash);
            assert_eq!(unsigned.signing_digest(&domain), vector.digest);
            assert_eq!(unsigned.sign(&domain, &signer), vector.signature);
        }
    }

    #[test]
    fn typehash_matches_the_contract_constant() {
        assert_eq!(proposal_typehash(), vectors().proposal_typehash);
    }
}
