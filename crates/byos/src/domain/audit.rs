//! Audit events — the durable evidence trail (ADR-0001: in-memory hot path +
//! async write-behind). The store emits one event per mutation; an infra
//! writer task persists them to Postgres. Track B slash claims (ADR-0003) can
//! arrive months after a trade, so these records outlive the hot store.

use {
    super::proposal::{OrderUid, Proposal, ProposalId},
    alloy::primitives::Address,
    std::time::SystemTime,
};

/// Emitting half of the write-behind channel. Unbounded: emission must never
/// block the hot path, and dropping evidence by design is worse than memory
/// growth during a DB outage (which is an ops page anyway).
pub type Sender = tokio::sync::mpsc::UnboundedSender<AuditEvent>;

/// A proposal lifecycle event worth keeping as dispute evidence.
#[derive(Clone, Debug)]
pub struct AuditEvent {
    /// Wall-clock time at emission — the evidentiary timestamp. The hot
    /// store's `created_at` stays monotonic (`Instant`); evidence needs an
    /// absolute clock.
    pub occurred_at: SystemTime,
    pub kind: AuditKind,
}

#[derive(Clone, Debug)]
pub enum AuditKind {
    /// Proposal accepted into the store; carries the full body as evidence
    /// (the dispute-query keys come out of it). Boxed: the body dwarfs the
    /// other variants.
    Received { proposal: Box<Proposal> },
    /// Cancelled by its sub-solver via a signed `CancelProposal`. Carries the
    /// dispute-query keys explicitly — the body already sits in the
    /// `received` row.
    Cancelled {
        proposal_id: ProposalId,
        sub_solver: Address,
        order_uid: OrderUid,
    },
}

impl AuditEvent {
    /// Dispute-query keys for the indexed columns, extracted per variant so
    /// body-carrying events don't have to duplicate them.
    pub fn proposal_id(&self) -> ProposalId {
        match &self.kind {
            AuditKind::Received { proposal } => proposal.id,
            AuditKind::Cancelled { proposal_id, .. } => *proposal_id,
        }
    }

    pub fn sub_solver(&self) -> Address {
        match &self.kind {
            AuditKind::Received { proposal } => proposal.sub_solver,
            AuditKind::Cancelled { sub_solver, .. } => *sub_solver,
        }
    }

    pub fn order_uid(&self) -> &OrderUid {
        match &self.kind {
            AuditKind::Received { proposal } => &proposal.order_uid,
            AuditKind::Cancelled { order_uid, .. } => order_uid,
        }
    }

    /// Wire name for the `event_type` column. New lifecycle events (driver
    /// outcomes, ingestion states) add variants here — the column is TEXT, so
    /// additions are migration-free.
    pub fn event_type(&self) -> &'static str {
        match self.kind {
            AuditKind::Received { .. } => "received",
            AuditKind::Cancelled { .. } => "cancelled",
        }
    }

    /// JSON evidence payload. Follows the wire conventions (camelCase, hex
    /// strings for bytes, decimal strings for 256-bit amounts) but is its own
    /// representation — API DTO changes must not silently rewrite what stored
    /// evidence looks like. Full proposal body for `Received`; transitions
    /// stay minimal because the `received` row already holds the body.
    pub fn payload(&self) -> serde_json::Value {
        match &self.kind {
            AuditKind::Received { proposal } => received_payload(proposal),
            AuditKind::Cancelled { .. } => serde_json::json!({}),
        }
    }
}

fn received_payload(p: &Proposal) -> serde_json::Value {
    serde_json::json!({
        "id": p.id,
        "subSolver": p.sub_solver,
        "orderUid": p.order_uid.to_string(),
        "orderUidHash": p.order_uid_hash,
        "sellAmount": p.sell_amount.to_string(),
        "buyAmount": p.buy_amount.to_string(),
        "interactions": p.interactions.iter().map(|i| serde_json::json!({
            "target": i.target,
            "value": i.value.to_string(),
            "callData": i.callData,
        })).collect::<Vec<_>>(),
        "interactionsHash": p.interactions_hash,
        "validUntil": p.valid_until.to_string(),
        "nonce": p.nonce.to_string(),
        "signature": p.signature,
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::domain::proposal::ProposalStatus,
        alloy::primitives::{Bytes, U256, address, b256, bytes},
        byos_common::contracts::Interaction,
    };

    fn event_for(kind_of: &str) -> AuditEvent {
        let proposal = Proposal {
            id: 7,
            sub_solver: address!("00000000000000000000000000000000000000aa"),
            order_uid: OrderUid([0xab; 56]),
            order_uid_hash: b256!(
                "1111111111111111111111111111111111111111111111111111111111111111"
            ),
            sell_amount: U256::from(1_000_000u64),
            buy_amount: U256::from(990_000u64),
            interactions: vec![Interaction {
                target: address!("00000000000000000000000000000000000000bb"),
                value: U256::from(5u64),
                callData: bytes!("deadbeef"),
            }],
            interactions_hash: b256!(
                "2222222222222222222222222222222222222222222222222222222222222222"
            ),
            valid_until: U256::from(1_700_000_000u64),
            nonce: U256::from(3u64),
            signature: Bytes::from(vec![0x11; 65]),
            status: ProposalStatus::Active,
            created_at: std::time::Instant::now(),
        };
        AuditEvent {
            occurred_at: SystemTime::now(),
            kind: match kind_of {
                "received" => AuditKind::Received {
                    proposal: Box::new(proposal),
                },
                _ => AuditKind::Cancelled {
                    proposal_id: proposal.id,
                    sub_solver: proposal.sub_solver,
                    order_uid: proposal.order_uid.clone(),
                },
            },
        }
    }

    /// Both variants must yield the same dispute-query keys — `received`
    /// extracts them from the body, `cancelled` carries them explicitly.
    #[test]
    fn dispute_keys_agree_across_variants() {
        for kind_of in ["received", "cancelled"] {
            let event = event_for(kind_of);
            assert_eq!(event.proposal_id(), 7);
            assert_eq!(
                event.sub_solver(),
                address!("00000000000000000000000000000000000000aa")
            );
            assert_eq!(*event.order_uid(), OrderUid([0xab; 56]));
        }
    }

    #[test]
    fn received_payload_is_full_evidence() {
        let event = event_for("received");
        assert_eq!(event.event_type(), "received");

        let payload = event.payload();
        assert_eq!(payload["id"], 7);
        assert_eq!(
            payload["subSolver"],
            "0x00000000000000000000000000000000000000aa"
        );
        assert_eq!(payload["orderUid"], format!("0x{}", "ab".repeat(56)));
        assert_eq!(payload["orderUidHash"], format!("0x{}", "11".repeat(32)));
        assert_eq!(payload["sellAmount"], "1000000");
        assert_eq!(payload["buyAmount"], "990000");
        assert_eq!(payload["validUntil"], "1700000000");
        assert_eq!(payload["nonce"], "3");
        assert_eq!(payload["signature"], format!("0x{}", "11".repeat(65)));
        assert_eq!(
            payload["interactions"][0]["target"],
            "0x00000000000000000000000000000000000000bb"
        );
        assert_eq!(payload["interactions"][0]["value"], "5");
        assert_eq!(payload["interactions"][0]["callData"], "0xdeadbeef");
        assert_eq!(
            payload["interactionsHash"],
            format!("0x{}", "22".repeat(32))
        );
    }

    #[test]
    fn cancelled_payload_is_minimal() {
        let event = event_for("cancelled");
        assert_eq!(event.event_type(), "cancelled");
        assert_eq!(event.payload(), serde_json::json!({}));
    }
}
