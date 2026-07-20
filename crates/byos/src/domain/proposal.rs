//! Proposal domain types and in-memory store.

use {
    super::audit,
    alloy::primitives::{Address, B256, Bytes, U256},
    serde::Serialize,
    std::{
        collections::HashMap,
        sync::{
            RwLock,
            atomic::{AtomicU64, Ordering},
        },
        time::{Instant, SystemTime},
    },
};

/// Server-assigned proposal identifier.
pub type ProposalId = u64;

/// CoW Protocol order UID (56 bytes: 32-byte hash + 20-byte owner + 4-byte
/// validTo).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OrderUid(pub [u8; 56]);

/// `0x`-prefixed hex — the wire and evidence representation.
impl std::fmt::Display for OrderUid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&alloy::hex::encode_prefixed(self.0))
    }
}

/// Lifecycle state of a proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, strum::Display)]
#[serde(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum ProposalStatus {
    Active,
    Expired,
    Settled,
    SimFailed,
    Cancelled,
}

/// A stored proposal, post-validation. Domain type — never serialized directly
/// to the wire (DTOs handle that).
#[derive(Clone, Debug)]
pub struct Proposal {
    pub id: ProposalId,
    pub sub_solver: Address,
    pub order_uid: OrderUid,
    pub order_uid_hash: B256,
    pub sell_amount: U256,
    pub buy_amount: U256,
    pub interactions: Vec<byos_common::contracts::Interaction>,
    pub interactions_hash: B256,
    pub valid_until: U256,
    pub nonce: U256,
    pub signature: Bytes,
    pub status: ProposalStatus,
    pub created_at: Instant,
}

/// Store-level error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    #[error("proposal {0} not found")]
    NotFound(ProposalId),
    #[error("proposal {0} not owned by {1}")]
    NotOwner(ProposalId, Address),
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

struct Inner {
    proposals: HashMap<ProposalId, Proposal>,
    by_order_uid: HashMap<OrderUid, Vec<ProposalId>>,
    by_sub_solver: HashMap<Address, Vec<ProposalId>>,
}

/// In-memory proposal store backed by `RwLock<Inner>`. A single lock wraps all
/// maps to avoid ordering issues between primary and secondary indexes.
///
/// Every mutation emits an [`audit::AuditEvent`] — auditing happens by
/// construction, so future mutation sites (driver-reported outcomes, async
/// ingestion) cannot forget to leave evidence.
pub struct InMemoryProposalStore {
    next_id: AtomicU64,
    audit: audit::Sender,
    inner: RwLock<Inner>,
}

impl InMemoryProposalStore {
    pub fn new(audit: audit::Sender) -> Self {
        Self {
            next_id: AtomicU64::new(1),
            audit,
            inner: RwLock::new(Inner {
                proposals: HashMap::new(),
                by_order_uid: HashMap::new(),
                by_sub_solver: HashMap::new(),
            }),
        }
    }

    /// Resume ID assignment after `last` — used at boot to continue from the
    /// audit trail's high-water mark so restarts never reissue an ID.
    pub fn seed_next_id(&self, last: ProposalId) {
        self.next_id.store(last + 1, Ordering::Relaxed);
    }

    /// The audit channel is unbounded, so a send only fails if the writer
    /// task is gone — a bug, not a runtime condition; log loudly.
    fn emit(&self, event: audit::AuditEvent) {
        if let Err(err) = self.audit.send(event) {
            tracing::error!(
                proposal_id = err.0.proposal_id(),
                "audit writer gone; evidence event dropped"
            );
        }
    }

    /// Insert a validated proposal. The `id` field on the input is ignored —
    /// the store assigns a fresh one and returns it.
    pub fn insert(&self, mut proposal: Proposal) -> ProposalId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        proposal.id = id;

        {
            let mut inner = self.inner.write().unwrap();
            inner
                .by_order_uid
                .entry(proposal.order_uid.clone())
                .or_default()
                .push(id);
            inner
                .by_sub_solver
                .entry(proposal.sub_solver)
                .or_default()
                .push(id);
            inner.proposals.insert(id, proposal.clone());
        }

        self.emit(audit::AuditEvent {
            occurred_at: SystemTime::now(),
            kind: audit::AuditKind::Received {
                proposal: Box::new(proposal),
            },
        });
        id
    }

    /// Look up a single proposal by ID.
    pub fn get(&self, id: ProposalId) -> Option<Proposal> {
        let inner = self.inner.read().unwrap();
        inner.proposals.get(&id).cloned()
    }

    /// List active proposals for a given order UID.
    pub fn list_by_order_uid(&self, order_uid: &OrderUid) -> Vec<Proposal> {
        let inner = self.inner.read().unwrap();
        inner
            .by_order_uid
            .get(order_uid)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| inner.proposals.get(id))
                    .filter(|p| p.status == ProposalStatus::Active)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// List active proposals for a given sub-solver address.
    pub fn list_by_sub_solver(&self, sub_solver: Address) -> Vec<Proposal> {
        let inner = self.inner.read().unwrap();
        inner
            .by_sub_solver
            .get(&sub_solver)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| inner.proposals.get(id))
                    .filter(|p| p.status == ProposalStatus::Active)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Cancel a proposal. Returns `Err` if not found or not owned by the given
    /// sub-solver.
    pub fn cancel(&self, id: ProposalId, sub_solver: Address) -> Result<(), StoreError> {
        let order_uid = {
            let mut inner = self.inner.write().unwrap();
            let proposal = inner
                .proposals
                .get_mut(&id)
                .ok_or(StoreError::NotFound(id))?;
            if proposal.sub_solver != sub_solver {
                return Err(StoreError::NotOwner(id, sub_solver));
            }
            proposal.status = ProposalStatus::Cancelled;
            proposal.order_uid.clone()
        };

        self.emit(audit::AuditEvent {
            occurred_at: SystemTime::now(),
            kind: audit::AuditKind::Cancelled {
                proposal_id: id,
                sub_solver,
                order_uid,
            },
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::domain::audit::{AuditEvent, AuditKind},
        alloy::primitives::{address, keccak256},
        tokio::sync::mpsc,
    };

    fn test_store() -> (InMemoryProposalStore, mpsc::UnboundedReceiver<AuditEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (InMemoryProposalStore::new(tx), rx)
    }

    fn make_proposal(order_uid: OrderUid, sub_solver: Address) -> Proposal {
        let order_uid_hash = keccak256(order_uid.0);
        Proposal {
            id: 0,
            sub_solver,
            order_uid,
            order_uid_hash,
            sell_amount: U256::from(1_000_000u64),
            buy_amount: U256::from(990_000u64),
            interactions: vec![],
            interactions_hash: B256::ZERO,
            valid_until: U256::from(u64::MAX),
            nonce: U256::from(1u64),
            signature: Bytes::new(),
            status: ProposalStatus::Active,
            created_at: Instant::now(),
        }
    }

    fn test_order_uid() -> OrderUid {
        OrderUid([0xaa; 56])
    }

    #[test]
    fn insert_emits_received_audit_event() {
        let (store, mut audit) = test_store();
        let solver = address!("0000000000000000000000000000000000000001");

        let id = store.insert(make_proposal(test_order_uid(), solver));

        let event = audit.try_recv().expect("insert should emit an audit event");
        assert_eq!(event.proposal_id(), id);
        assert_eq!(event.sub_solver(), solver);
        assert_eq!(*event.order_uid(), test_order_uid());
        match event.kind {
            AuditKind::Received { proposal } => {
                assert_eq!(proposal.id, id);
                assert_eq!(proposal.sub_solver, solver);
            }
            other => panic!("expected Received, got {other:?}"),
        }
    }

    #[test]
    fn insert_and_get() {
        let (store, _audit) = test_store();
        let solver = address!("0000000000000000000000000000000000000001");
        let p = make_proposal(test_order_uid(), solver);

        let id = store.insert(p);
        assert!(id > 0);

        let fetched = store.get(id).expect("should exist");
        assert_eq!(fetched.id, id);
        assert_eq!(fetched.sub_solver, solver);
    }

    #[test]
    fn get_nonexistent_returns_none() {
        let (store, _audit) = test_store();
        assert!(store.get(999).is_none());
    }

    #[test]
    fn list_by_order_uid() {
        let (store, _audit) = test_store();
        let uid = test_order_uid();
        let solver = address!("0000000000000000000000000000000000000001");

        store.insert(make_proposal(uid.clone(), solver));
        store.insert(make_proposal(uid.clone(), solver));

        let results = store.list_by_order_uid(&uid);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn list_by_sub_solver() {
        let (store, _audit) = test_store();
        let solver_a = address!("0000000000000000000000000000000000000001");
        let solver_b = address!("0000000000000000000000000000000000000002");

        store.insert(make_proposal(test_order_uid(), solver_a));
        store.insert(make_proposal(OrderUid([0xbb; 56]), solver_b));

        assert_eq!(store.list_by_sub_solver(solver_a).len(), 1);
        assert_eq!(store.list_by_sub_solver(solver_b).len(), 1);
    }

    #[test]
    fn cancel_sets_status() {
        let (store, _audit) = test_store();
        let solver = address!("0000000000000000000000000000000000000001");

        let id = store.insert(make_proposal(test_order_uid(), solver));
        store.cancel(id, solver).expect("should succeed");

        let fetched = store.get(id).expect("should exist");
        assert_eq!(fetched.status, ProposalStatus::Cancelled);
    }

    #[test]
    fn cancel_emits_cancelled_audit_event() {
        let (store, mut audit) = test_store();
        let solver = address!("0000000000000000000000000000000000000001");

        let id = store.insert(make_proposal(test_order_uid(), solver));
        let _received = audit.try_recv().expect("insert event");

        store.cancel(id, solver).expect("should succeed");

        let event = audit.try_recv().expect("cancel should emit an audit event");
        assert_eq!(event.proposal_id(), id);
        assert_eq!(event.sub_solver(), solver);
        assert_eq!(*event.order_uid(), test_order_uid());
        assert!(matches!(event.kind, AuditKind::Cancelled { .. }));
    }

    #[test]
    fn cancel_wrong_owner_fails() {
        let (store, mut audit) = test_store();
        let solver = address!("0000000000000000000000000000000000000001");
        let other = address!("0000000000000000000000000000000000000002");

        let id = store.insert(make_proposal(test_order_uid(), solver));
        let _received = audit.try_recv().expect("insert event");

        let err = store.cancel(id, other).unwrap_err();
        assert!(matches!(err, StoreError::NotOwner(_, _)));
        assert!(
            audit.try_recv().is_err(),
            "failed cancel must not leave a cancelled event"
        );
    }

    #[test]
    fn cancel_nonexistent_fails() {
        let (store, _audit) = test_store();
        let solver = address!("0000000000000000000000000000000000000001");
        let err = store.cancel(999, solver).unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    #[test]
    fn cancelled_proposals_excluded_from_list() {
        let (store, _audit) = test_store();
        let uid = test_order_uid();
        let solver = address!("0000000000000000000000000000000000000001");

        let id = store.insert(make_proposal(uid.clone(), solver));
        store.cancel(id, solver).unwrap();

        assert!(store.list_by_order_uid(&uid).is_empty());
        assert!(store.list_by_sub_solver(solver).is_empty());
    }
}
