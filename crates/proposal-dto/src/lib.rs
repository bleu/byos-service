//! Serde wire types for the proposal API (`POST /proposals`,
//! `GET /proposals/{order_uid}`, `DELETE /proposals/{id}` — ADR-0001), shared
//! by the `byos` server and sub-solver clients so both ends deserialize one
//! model. Mirrors the `solvers-dto` pattern in cowprotocol/services
//! (ADR-0005). Conventions: camelCase JSON, 256-bit amounts as decimal
//! strings, addresses and order UIDs as hex strings.

use {
    alloy::primitives::{Address, Bytes, U256},
    serde::{Deserialize, Serialize},
    serde_with::{DisplayFromStr, serde_as},
};

/// Body of `POST /proposals`: one signed, immutable proposal (ADR-0001).
#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Proposal {
    /// UID of the CoW order this proposal settles (56 bytes, hex).
    pub order_uid: Bytes,
    #[serde_as(as = "DisplayFromStr")]
    pub sell_amount: U256,
    #[serde_as(as = "DisplayFromStr")]
    pub buy_amount: U256,
    /// The route, executed as-is inside the sub-solver's Trampoline.
    pub interactions: Vec<Interaction>,
    /// Unix timestamp after which the proposal is no longer executable.
    pub valid_until: u64,
    /// Sub-solver-chosen salt distinguishing otherwise identical proposals.
    #[serde_as(as = "DisplayFromStr")]
    pub nonce: U256,
    /// EIP-712 signature over `ProposalData` (owned by byos-contracts).
    pub signature: Bytes,
}

/// One call of the route, mirroring `ITrampoline.Interaction`.
#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Interaction {
    pub target: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub value: U256,
    pub call_data: Bytes,
}

/// Body of the `GET /proposals/{order_uid}` response: per-proposal metadata,
/// never amounts or interactions (ADR-0001's leakage rule).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metadata {
    pub proposals: Vec<ProposalMetadata>,
}

/// One proposal's metadata as exposed by `GET /proposals/{order_uid}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposalMetadata {
    /// Server-assigned id, the handle for `DELETE /proposals/{id}`.
    pub id: u64,
    /// The sub-solver address recovered from the proposal signature.
    pub solver: Address,
    /// Unix timestamp after which the proposal is dropped.
    pub valid_until: u64,
    pub status: Status,
}

/// Proposal lifecycle status. The store keeps proposals active-or-gone
/// (ADR-0001), so `active` is the only status v1 ever serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Active,
}

/// Body of a 2xx `POST /proposals` response: the server-assigned proposal id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Submitted {
    pub id: u64,
}

/// Body of `DELETE /proposals/{id}`: an EIP-712 signature over
/// `CancelProposal { proposalId }` in the proposal domain. API-auth only,
/// never verified on-chain, so this type is owned here (ADR-0001).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Cancellation {
    pub signature: Bytes,
}

/// Body of a 4xx/5xx response (ADR-0007): machine-readable kind plus a
/// human-oriented description.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Error {
    pub kind: Kind,
    pub description: String,
}

/// Typed rejection reasons (ADR-0001/0007). `Unknown` absorbs kinds newer
/// than the client, so server additions never break deserialization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    BadSignature,
    UnderCollateralized,
    SimulationRevert,
    RateLimited,
    FeeNotCovered,
    InvalidProposal,
    UnknownOrder,
    #[serde(other, skip_serializing)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use {super::*, alloy::primitives::U256, serde_json::json};

    #[test]
    fn proposal_round_trips_the_adr_0001_wire_shape() {
        let wire = json!({
            "orderUid": format!("0x{}", "11".repeat(56)),
            "sellAmount": "1000000000000000000",
            "buyAmount": "5000000",
            "interactions": [{
                "target": "0x7a250d5630b4cf539739df2c5dacb4c659f2488d",
                "value": "0",
                "callData": "0xabcdef",
            }],
            "validUntil": 1_750_000_000,
            "nonce": "7",
            "signature": format!("0x{}", "22".repeat(65)),
        });

        let proposal: Proposal = serde_json::from_value(wire.clone()).unwrap();

        assert_eq!(proposal.sell_amount, U256::from(10).pow(U256::from(18)));
        assert_eq!(proposal.order_uid.len(), 56);
        assert_eq!(proposal.interactions[0].value, U256::ZERO);
        assert_eq!(serde_json::to_value(&proposal).unwrap(), wire);
    }

    #[test]
    fn metadata_response_matches_the_adr_0001_example() {
        let wire = json!({
            "proposals": [
                { "id": 42, "solver": "0x00000000000000000000000000000000000abc00", "validUntil": 1_750_000_000, "status": "active" },
                { "id": 43, "solver": "0x00000000000000000000000000000000000def00", "validUntil": 1_750_000_060, "status": "active" },
            ]
        });

        let metadata: Metadata = serde_json::from_value(wire.clone()).unwrap();

        assert_eq!(metadata.proposals[0].id, 42);
        assert_eq!(metadata.proposals[1].status, Status::Active);
        assert_eq!(serde_json::to_value(&metadata).unwrap(), wire);
    }

    #[test]
    fn rejection_kinds_are_pascal_case_and_tolerate_future_kinds() {
        let wire =
            json!({ "kind": "UnderCollateralized", "description": "escrow below gas + c_l" });
        let error: Error = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(error.kind, Kind::UnderCollateralized);
        assert_eq!(serde_json::to_value(&error).unwrap(), wire);

        // A kind added server-side later must not break older clients.
        let future = json!({ "kind": "SomethingNewer", "description": "?" });
        let error: Error = serde_json::from_value(future).unwrap();
        assert_eq!(error.kind, Kind::Unknown);
    }

    #[test]
    fn submission_verdict_and_cancellation_round_trip() {
        let submitted: Submitted = serde_json::from_value(json!({ "id": 42 })).unwrap();
        assert_eq!(submitted.id, 42);

        let wire = json!({ "signature": format!("0x{}", "33".repeat(65)) });
        let cancellation: Cancellation = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&cancellation).unwrap(), wire);
    }
}
