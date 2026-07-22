//! Proposal domain types and in-memory store.

use {
    super::audit,
    alloy::primitives::{Address, B256, Bytes, U256},
    parking_lot::RwLock,
    serde::Serialize,
    std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::{Instant, SystemTime},
    },
};

/// Server-assigned proposal identifier (newtype for type safety — a
/// `ProposalId` cannot be accidentally confused with any other `u64`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ProposalId(pub u64);

impl std::fmt::Display for ProposalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for ProposalId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<u64>().map(Self)
    }
}

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

/// Parse error for `OrderUid::from_hex`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OrderUidError {
    #[error("invalid hex: {0}")]
    Hex(#[from] alloy::hex::FromHexError),
    #[error("expected 56 bytes, got {0}")]
    Length(usize),
}

impl OrderUid {
    /// Parse a `0x`-prefixed (or bare) hex string into an `OrderUid`.
    pub fn from_hex(s: &str) -> Result<Self, OrderUidError> {
        let bytes = alloy::hex::decode(s.strip_prefix("0x").unwrap_or(s))?;
        if bytes.len() != 56 {
            return Err(OrderUidError::Length(bytes.len()));
        }
        let mut arr = [0u8; 56];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }

    /// Extract the 20-byte owner address from bytes 32..52 of the order UID.
    pub fn owner(&self) -> Address {
        Address::from_slice(&self.0[32..52])
    }
}

impl std::str::FromStr for OrderUid {
    type Err = OrderUidError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

/// Lifecycle state of a proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, strum::Display)]
#[serde(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum ProposalStatus {
    /// Signature verified, awaiting background validation.
    Submitted,
    Active,
    /// Failed background gatekeeping (e.g. insufficient escrow).
    Rejected,
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
    pub sell_token: Address,
    pub buy_token: Address,
    pub interactions: Vec<byos_common::contracts::Interaction>,
    pub interactions_hash: B256,
    pub valid_until: U256,
    pub nonce: U256,
    pub signature: Bytes,
    pub status: ProposalStatus,
    /// Why the background validator rejected this proposal. Only ever set by
    /// the `Submitted → Rejected` transition.
    pub rejection_reason: Option<crate::domain::validator::RejectionReason>,
    /// Gas consumed by the simulation `eth_estimateGas` call. Set by the
    /// validator on successful simulation; `None` until first validation pass.
    pub gas_used: Option<u64>,
    /// Trampoline address resolved via `TrampolineFactory.addressOf(sub_solver)`.
    /// Set by the validator on first validation; `None` until resolved.
    pub trampoline: Option<Address>,
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
    #[error("proposal {id} is {actual}, expected {expected}")]
    StaleTransition {
        id: ProposalId,
        expected: ProposalStatus,
        actual: ProposalStatus,
    },
}

/// Test fixture: a minimal proposal in the given status.
#[cfg(test)]
pub(crate) fn test_proposal(
    order_uid: OrderUid,
    sub_solver: Address,
    status: ProposalStatus,
) -> Proposal {
    let order_uid_hash = alloy::primitives::keccak256(order_uid.0);
    Proposal {
        id: ProposalId(0),
        sub_solver,
        order_uid,
        order_uid_hash,
        sell_amount: U256::from(1_000_000_u64),
        buy_amount: U256::from(990_000_u64),
        sell_token: Address::ZERO,
        buy_token: Address::ZERO,
        interactions: vec![],
        interactions_hash: B256::ZERO,
        valid_until: U256::from(u64::MAX),
        nonce: U256::from(1_u64),
        signature: Bytes::new(),
        status,
        rejection_reason: None,
        gas_used: None,
        trampoline: None,
        created_at: Instant::now(),
    }
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

struct Inner {
    proposals: HashMap<ProposalId, Arc<Proposal>>,
    by_order_uid: HashMap<OrderUid, Vec<ProposalId>>,
    by_sub_solver: HashMap<Address, Vec<ProposalId>>,
}

/// In-memory proposal store backed by `parking_lot::RwLock<Inner>`. Uses
/// `parking_lot` instead of `std::sync::RwLock` for two reasons: (1) no lock
/// poisoning, so a panic on one request cannot cascade to every subsequent
/// request; (2) faster for the microsecond-scale critical sections here.
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
        self.next_id.store(last.0 + 1, Ordering::Relaxed);
    }

    /// The audit channel is unbounded, so a send only fails if the writer
    /// task is gone — a bug, not a runtime condition; log loudly.
    fn emit(&self, event: audit::AuditEvent) {
        if let Err(err) = self.audit.send(event) {
            tracing::error!(
                proposal_id = %err.0.proposal_id(),
                "audit writer gone; evidence event dropped"
            );
        }
    }

    /// Insert a validated proposal. The `id` field on the input is ignored —
    /// the store assigns a fresh one and returns it.
    pub fn insert(&self, mut proposal: Proposal) -> ProposalId {
        let id = ProposalId(self.next_id.fetch_add(1, Ordering::Relaxed));
        proposal.id = id;

        let arc = Arc::new(proposal);
        {
            let mut inner = self.inner.write();
            inner
                .by_order_uid
                .entry(arc.order_uid.clone())
                .or_default()
                .push(id);
            inner
                .by_sub_solver
                .entry(arc.sub_solver)
                .or_default()
                .push(id);
            inner.proposals.insert(id, Arc::clone(&arc));
        }

        self.emit(audit::AuditEvent {
            occurred_at: SystemTime::now(),
            kind: audit::AuditKind::Received { proposal: arc },
        });
        id
    }

    /// Look up a single proposal by ID.
    pub fn get(&self, id: ProposalId) -> Option<Arc<Proposal>> {
        let inner = self.inner.read();
        inner.proposals.get(&id).cloned()
    }

    /// List active proposals for a given order UID.
    pub fn list_by_order_uid(&self, order_uid: &OrderUid) -> Vec<Arc<Proposal>> {
        let inner = self.inner.read();
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

    /// List live (`Submitted` or `Active`) proposals for a given sub-solver
    /// address. This is the owner's management view, so pending submissions
    /// are included.
    pub fn list_by_sub_solver(&self, sub_solver: Address) -> Vec<Arc<Proposal>> {
        let inner = self.inner.read();
        inner
            .by_sub_solver
            .get(&sub_solver)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| inner.proposals.get(id))
                    .filter(|p| {
                        matches!(p.status, ProposalStatus::Submitted | ProposalStatus::Active)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Clone out every proposal currently in one of the given statuses. Used
    /// by the background validator to work on a snapshot without holding the
    /// lock — one lock acquisition and one scan per tick. Returns `Arc`
    /// pointers (cheap clone) so each read is a pointer bump, not a deep copy.
    pub fn snapshot_by_statuses(&self, statuses: &[ProposalStatus]) -> Vec<Arc<Proposal>> {
        let inner = self.inner.read();
        inner
            .proposals
            .values()
            .filter(|p| statuses.contains(&p.status))
            .cloned()
            .collect()
    }

    /// Transition a proposal from `from` to `to`, only if it is still in
    /// `from`. A mismatch means someone else (e.g. a cancellation) won the
    /// race — the caller's verdict is stale and must be dropped. A successful
    /// transition emits a status-changed audit event and prunes the proposal
    /// from secondary indexes if it reached a terminal state.
    pub fn transition(
        &self,
        id: ProposalId,
        from: ProposalStatus,
        to: ProposalStatus,
    ) -> Result<(), StoreError> {
        let (sub_solver, order_uid) = {
            let mut inner = self.inner.write();
            let (sub_solver, order_uid) = {
                let proposal = inner
                    .proposals
                    .get_mut(&id)
                    .ok_or(StoreError::NotFound(id))?;
                if proposal.status != from {
                    return Err(StoreError::StaleTransition {
                        id,
                        expected: from,
                        actual: proposal.status,
                    });
                }
                let p = Arc::make_mut(proposal);
                p.status = to;
                (p.sub_solver, p.order_uid.clone())
            };
            if is_terminal(to) {
                prune_indexes(&mut inner, id, &order_uid, sub_solver);
            }
            (sub_solver, order_uid)
        };

        self.emit(audit::AuditEvent {
            occurred_at: SystemTime::now(),
            kind: audit::AuditKind::StatusChanged {
                proposal_id: id,
                sub_solver,
                order_uid,
                from,
                to,
                rejection_reason: None,
            },
        });
        Ok(())
    }

    /// Apply a validator verdict to a `Submitted` or `Active` proposal.
    ///
    /// - `Accept`: transitions `Submitted` → `Active`, or keeps `Active` →
    ///   `Active` (re-validation). Writes `gas_used` and `trampoline` when
    ///   present in the verdict.
    /// - `Reject`: transitions to `Rejected` with a rejection reason.
    /// - `SimFailed`: transitions to `SimFailed`.
    ///
    /// Fails with `StaleTransition` if the proposal is not in `Submitted` or
    /// `Active` (e.g. a cancellation raced the validator).
    pub fn resolve_verdict(
        &self,
        id: ProposalId,
        verdict: crate::domain::validator::Verdict,
    ) -> Result<ProposalStatus, StoreError> {
        use crate::domain::validator::Verdict;

        let (from_status, status, sub_solver, order_uid, rejection_reason) = {
            let mut inner = self.inner.write();
            let (from_status, status, sub_solver, order_uid, rejection_reason) = {
                let proposal = inner
                    .proposals
                    .get_mut(&id)
                    .ok_or(StoreError::NotFound(id))?;
                if !matches!(
                    proposal.status,
                    ProposalStatus::Submitted | ProposalStatus::Active
                ) {
                    return Err(StoreError::StaleTransition {
                        id,
                        expected: ProposalStatus::Active,
                        actual: proposal.status,
                    });
                }
                let from_status = proposal.status;
                let rejection_reason = match &verdict {
                    Verdict::Reject(reason) => Some(*reason),
                    Verdict::Accept { .. } | Verdict::SimFailed => None,
                };
                let p = Arc::make_mut(proposal);
                p.status = match &verdict {
                    Verdict::Accept { gas_used, trampoline } => {
                        if let Some(g) = gas_used {
                            p.gas_used = Some(*g);
                        }
                        if let Some(t) = trampoline {
                            p.trampoline = Some(*t);
                        }
                        ProposalStatus::Active
                    }
                    Verdict::Reject(reason) => {
                        p.rejection_reason = Some(*reason);
                        ProposalStatus::Rejected
                    }
                    Verdict::SimFailed => ProposalStatus::SimFailed,
                };
                (
                    from_status,
                    p.status,
                    p.sub_solver,
                    p.order_uid.clone(),
                    rejection_reason,
                )
            };
            if is_terminal(status) {
                prune_indexes(&mut inner, id, &order_uid, sub_solver);
            }
            (from_status, status, sub_solver, order_uid, rejection_reason)
        };

        self.emit(audit::AuditEvent {
            occurred_at: SystemTime::now(),
            kind: audit::AuditKind::StatusChanged {
                proposal_id: id,
                sub_solver,
                order_uid,
                from: from_status,
                to: status,
                rejection_reason,
            },
        });
        Ok(status)
    }

    /// Cancel a proposal. Only live proposals (`Submitted`/`Active`) can be
    /// cancelled; returns `Err` if not found, not owned by the given
    /// sub-solver, or already in a terminal state.
    pub fn cancel(&self, id: ProposalId, sub_solver: Address) -> Result<(), StoreError> {
        let order_uid = {
            let mut inner = self.inner.write();
            let order_uid = {
                let proposal = inner
                    .proposals
                    .get_mut(&id)
                    .ok_or(StoreError::NotFound(id))?;
                if proposal.sub_solver != sub_solver {
                    return Err(StoreError::NotOwner(id, sub_solver));
                }
                if !matches!(
                    proposal.status,
                    ProposalStatus::Submitted | ProposalStatus::Active
                ) {
                    return Err(StoreError::StaleTransition {
                        id,
                        expected: ProposalStatus::Active,
                        actual: proposal.status,
                    });
                }
                let p = Arc::make_mut(proposal);
                p.status = ProposalStatus::Cancelled;
                p.order_uid.clone()
            };
            prune_indexes(&mut inner, id, &order_uid, sub_solver);
            order_uid
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

fn is_terminal(status: ProposalStatus) -> bool {
    matches!(
        status,
        ProposalStatus::Cancelled
            | ProposalStatus::Expired
            | ProposalStatus::Settled
            | ProposalStatus::SimFailed
            | ProposalStatus::Rejected
    )
}

/// Remove a proposal ID from the secondary indexes. Called inside a
/// write-locked block after the proposal reaches a terminal state.
fn prune_indexes(inner: &mut Inner, id: ProposalId, order_uid: &OrderUid, sub_solver: Address) {
    if let Some(ids) = inner.by_order_uid.get_mut(order_uid) {
        ids.retain(|&pid| pid != id);
        if ids.is_empty() {
            inner.by_order_uid.remove(order_uid);
        }
    }
    if let Some(ids) = inner.by_sub_solver.get_mut(&sub_solver) {
        ids.retain(|&pid| pid != id);
        if ids.is_empty() {
            inner.by_sub_solver.remove(&sub_solver);
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::domain::audit::{AuditEvent, AuditKind},
        alloy::primitives::address,
        tokio::sync::mpsc,
    };

    const SOLVER_A: Address = address!("0000000000000000000000000000000000000001");
    const SOLVER_B: Address = address!("0000000000000000000000000000000000000002");

    #[test]
    fn order_uid_owner_extracts_bytes_32_to_52() {
        let mut uid = [0u8; 56];
        // bytes 0..32: order hash
        uid[..32].fill(0xaa);
        // bytes 32..52: owner address
        uid[32..52].copy_from_slice(SOLVER_A.as_slice());
        // bytes 52..56: validTo
        uid[52..56].fill(0xff);

        assert_eq!(OrderUid(uid).owner(), SOLVER_A);
    }

    fn test_store() -> (InMemoryProposalStore, mpsc::UnboundedReceiver<AuditEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (InMemoryProposalStore::new(tx), rx)
    }

    fn make_proposal(order_uid: OrderUid, sub_solver: Address) -> Proposal {
        test_proposal(order_uid, sub_solver, ProposalStatus::Active)
    }

    fn test_order_uid() -> OrderUid {
        OrderUid([0xaa; 56])
    }

    #[test]
    fn insert_emits_received_audit_event() {
        let (store, mut audit) = test_store();
        let solver = SOLVER_A;

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
        let solver = SOLVER_A;
        let p = make_proposal(test_order_uid(), solver);

        let id = store.insert(p);
        assert!(id.0 > 0);

        let fetched = store.get(id).expect("should exist");
        assert_eq!(fetched.id, id);
        assert_eq!(fetched.sub_solver, solver);
    }

    #[test]
    fn get_nonexistent_returns_none() {
        let (store, _audit) = test_store();
        assert!(store.get(ProposalId(999)).is_none());
    }

    #[test]
    fn list_by_order_uid() {
        let (store, _audit) = test_store();
        let uid = test_order_uid();
        let solver = SOLVER_A;

        store.insert(make_proposal(uid.clone(), solver));
        store.insert(make_proposal(uid.clone(), solver));

        let results = store.list_by_order_uid(&uid);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn list_by_sub_solver() {
        let (store, _audit) = test_store();
        let solver_a = SOLVER_A;
        let solver_b = SOLVER_B;

        store.insert(make_proposal(test_order_uid(), solver_a));
        store.insert(make_proposal(OrderUid([0xbb; 56]), solver_b));

        assert_eq!(store.list_by_sub_solver(solver_a).len(), 1);
        assert_eq!(store.list_by_sub_solver(solver_b).len(), 1);
    }

    #[test]
    fn submitted_visible_to_owner_but_not_in_order_view() {
        let (store, _audit) = test_store();
        let uid = test_order_uid();
        let solver = SOLVER_A;
        let mut proposal = make_proposal(uid.clone(), solver);
        proposal.status = ProposalStatus::Submitted;
        store.insert(proposal);

        // The owner manages pending submissions through the by-solver view…
        assert_eq!(store.list_by_sub_solver(solver).len(), 1);
        // …but the public per-order view only shows gatekept proposals.
        assert!(store.list_by_order_uid(&uid).is_empty());
    }

    #[test]
    fn cancel_sets_status() {
        let (store, _audit) = test_store();
        let solver = SOLVER_A;

        let id = store.insert(make_proposal(test_order_uid(), solver));
        store.cancel(id, solver).expect("should succeed");

        let fetched = store.get(id).expect("should exist");
        assert_eq!(fetched.status, ProposalStatus::Cancelled);
    }

    #[test]
    fn cancel_emits_cancelled_audit_event() {
        let (store, mut audit) = test_store();
        let solver = SOLVER_A;

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
        let solver = SOLVER_A;
        let other = SOLVER_B;

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
    fn cancel_terminal_state_fails() {
        let (store, _audit) = test_store();
        let solver = SOLVER_A;
        let mut proposal = make_proposal(test_order_uid(), solver);
        proposal.status = ProposalStatus::Settled;

        let id = store.insert(proposal);
        let err = store.cancel(id, solver).unwrap_err();
        assert!(matches!(err, StoreError::StaleTransition { .. }));
        assert_eq!(
            store.get(id).unwrap().status,
            ProposalStatus::Settled,
            "a settled proposal must stay settled"
        );
    }

    #[test]
    fn cancel_submitted_proposal_succeeds() {
        let (store, _audit) = test_store();
        let solver = SOLVER_A;
        let mut proposal = make_proposal(test_order_uid(), solver);
        proposal.status = ProposalStatus::Submitted;

        let id = store.insert(proposal);
        store.cancel(id, solver).expect("cancel before verdict");
        assert_eq!(store.get(id).unwrap().status, ProposalStatus::Cancelled);
    }

    #[test]
    fn cancel_nonexistent_fails() {
        let (store, _audit) = test_store();
        let solver = SOLVER_A;
        let err = store.cancel(ProposalId(999), solver).unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    #[test]
    fn resolve_verdict_emits_status_changed_event() {
        let (store, mut audit) = test_store();
        let solver = SOLVER_A;
        let mut proposal = make_proposal(test_order_uid(), solver);
        proposal.status = ProposalStatus::Submitted;

        let id = store.insert(proposal);
        let _received = audit.try_recv().expect("insert event");

        let reason = crate::domain::validator::RejectionReason::InsufficientEscrow;
        store
            .resolve_verdict(id, crate::domain::validator::Verdict::Reject(reason))
            .expect("verdict lands");

        let event = audit.try_recv().expect("verdict should emit an event");
        assert_eq!(event.proposal_id(), id);
        assert_eq!(event.event_type(), "rejected");
        match event.kind {
            AuditKind::StatusChanged {
                from,
                to,
                rejection_reason,
                ..
            } => {
                assert_eq!(from, ProposalStatus::Submitted);
                assert_eq!(to, ProposalStatus::Rejected);
                assert_eq!(rejection_reason, Some(reason));
            }
            other => panic!("expected StatusChanged, got {other:?}"),
        }
    }

    #[test]
    fn transition_emits_status_changed_event() {
        let (store, mut audit) = test_store();
        let solver = SOLVER_A;

        let id = store.insert(make_proposal(test_order_uid(), solver));
        let _received = audit.try_recv().expect("insert event");

        store
            .transition(id, ProposalStatus::Active, ProposalStatus::Expired)
            .expect("transition lands");

        let event = audit.try_recv().expect("transition should emit an event");
        assert_eq!(event.proposal_id(), id);
        assert_eq!(event.event_type(), "expired");
    }

    #[test]
    fn stale_transition_emits_nothing() {
        let (store, mut audit) = test_store();
        let solver = SOLVER_A;

        let id = store.insert(make_proposal(test_order_uid(), solver));
        let _received = audit.try_recv().expect("insert event");

        let err = store
            .transition(id, ProposalStatus::Submitted, ProposalStatus::Expired)
            .unwrap_err();
        assert!(matches!(err, StoreError::StaleTransition { .. }));
        assert!(
            audit.try_recv().is_err(),
            "a dropped transition must not leave evidence"
        );
    }

    #[test]
    fn cancelled_proposals_excluded_from_list() {
        let (store, _audit) = test_store();
        let uid = test_order_uid();
        let solver = SOLVER_A;

        let id = store.insert(make_proposal(uid.clone(), solver));
        store.cancel(id, solver).unwrap();

        assert!(store.list_by_order_uid(&uid).is_empty());
        assert!(store.list_by_sub_solver(solver).is_empty());
    }
}
