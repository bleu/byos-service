//! Checks the EIP-712 implementation against contract-generated vectors
//! (`testdata/proposal-eip712.json`, emitted by `Eip712Vectors` in
//! bleu/byos-contracts). The schema is owned by the contracts; these tests
//! guarantee our hashes and signatures match what `Trampoline.execute`
//! verifies on-chain, without re-deriving the schema locally.

use {
    alloy::{
        primitives::{Address, B256, Bytes, U256},
        signers::local::PrivateKeySigner,
        sol_types::SolStruct,
    },
    byos_common::{
        contracts::{Interaction, Proposal},
        eip712,
    },
    serde::Deserialize,
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Vector {
    order_uid: Bytes,
    sell_amount: String,
    buy_amount: String,
    valid_until: u64,
    nonce: String,
    interactions: Vec<WireInteraction>,
    interactions_hash: B256,
    struct_hash: B256,
    digest: B256,
    signature: Bytes,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireInteraction {
    target: Address,
    value: String,
    call_data: Bytes,
}

fn u256(decimal: &str) -> U256 {
    U256::from_str_radix(decimal, 10).unwrap()
}

fn vectors() -> VectorFile {
    serde_json::from_str(include_str!("../testdata/proposal-eip712.json")).unwrap()
}

fn proposal(vector: &Vector) -> Proposal {
    Proposal {
        orderUidHash: alloy::primitives::keccak256(&vector.order_uid),
        sellAmount: u256(&vector.sell_amount),
        buyAmount: u256(&vector.buy_amount),
        validUntil: U256::from(vector.valid_until),
        nonce: u256(&vector.nonce),
    }
}

fn interactions(vector: &Vector) -> Vec<Interaction> {
    vector
        .interactions
        .iter()
        .map(|interaction| Interaction {
            target: interaction.target,
            value: u256(&interaction.value),
            callData: interaction.call_data.clone(),
        })
        .collect()
}

#[tokio::test]
async fn signing_matches_the_contract_vectors() {
    let file = vectors();
    let domain = eip712::byos_domain(file.domain.chain_id, file.domain.verifying_contract);
    assert_eq!(domain.separator(), file.domain.domain_separator);

    let signer = PrivateKeySigner::from_bytes(&file.sub_solver.private_key).unwrap();
    assert_eq!(signer.address(), file.sub_solver.address);

    for vector in file.vectors {
        let proposal = proposal(&vector);
        let interactions = interactions(&vector);

        let interactions_hash = eip712::compute_interactions_hash(&interactions);
        assert_eq!(interactions_hash, vector.interactions_hash);

        let data = eip712::proposal_data(&proposal, interactions_hash);
        assert_eq!(data.eip712_hash_struct(), vector.struct_hash);
        assert_eq!(data.eip712_signing_hash(&domain), vector.digest);

        let signature = eip712::sign_proposal(&signer, &domain, &proposal, &interactions)
            .await
            .unwrap();
        assert_eq!(Bytes::from(signature.as_bytes()), vector.signature);
    }
}

#[test]
fn typehash_matches_the_contract_constant() {
    assert_eq!(eip712::PROPOSAL_TYPEHASH, vectors().proposal_typehash);
}
