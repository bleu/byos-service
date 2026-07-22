//! Route handlers for the public proposal API.

use {
    super::{
        AppState,
        dto::{
            CreateProposalRequest,
            CreateProposalResponse,
            GetProposalResponse,
            ListProposalsResponse,
            ProposalMetadata,
            parse_hex,
            parse_u256,
        },
        error::{Error, Kind},
    },
    crate::domain::proposal::{OrderUid, ProposalId, ProposalStatus},
    alloy::primitives::{Bytes, Signature, U256, keccak256},
    axum::{
        Json,
        extract::{Path, State},
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
    },
    byos_common::{
        contracts::{Interaction, Proposal},
        eip712,
    },
    std::time::{Instant, SystemTime, UNIX_EPOCH},
};

// ---------------------------------------------------------------------------
// GET /healthz
// ---------------------------------------------------------------------------

pub async fn healthz() -> StatusCode {
    StatusCode::OK
}

// ---------------------------------------------------------------------------
// POST /proposals
// ---------------------------------------------------------------------------

pub async fn create_proposal(
    State(state): State<AppState>,
    Json(body): Json<CreateProposalRequest>,
) -> Result<impl IntoResponse, Error> {
    // 1. Parse and validate fields.
    let order_uid = OrderUid::from_hex(&body.order_uid)
        .map_err(|e| Error::new(Kind::BadRequest, format!("invalid orderUid: {e}")))?;

    let sell_amount = parse_u256(&body.sell_amount)
        .map_err(|_| Error::new(Kind::BadRequest, "invalid sellAmount"))?;
    let buy_amount = parse_u256(&body.buy_amount)
        .map_err(|_| Error::new(Kind::BadRequest, "invalid buyAmount"))?;
    let valid_until = parse_u256(&body.valid_until)
        .map_err(|_| Error::new(Kind::BadRequest, "invalid validUntil"))?;
    let nonce =
        parse_u256(&body.nonce).map_err(|_| Error::new(Kind::BadRequest, "invalid nonce"))?;

    let signature_bytes = parse_hex(&body.signature)
        .map_err(|_| Error::new(Kind::BadRequest, "invalid signature hex"))?;
    let signature = Signature::try_from(signature_bytes.as_slice())
        .map_err(|_| Error::from(Kind::InvalidSignature))?;

    // 2. Convert interactions.
    let interactions: Vec<Interaction> = body
        .interactions
        .iter()
        .map(Interaction::try_from)
        .collect::<Result<_, _>>()?;

    // 3. Compute hashes.
    let order_uid_hash = keccak256(order_uid.0);
    let interactions_hash = eip712::compute_interactions_hash(&interactions);

    // 4. Build the on-chain Proposal struct for signature recovery.
    let proposal = Proposal {
        orderUidHash: order_uid_hash,
        sellAmount: sell_amount,
        buyAmount: buy_amount,
        validUntil: valid_until,
        nonce,
    };

    // 5. Recover the sub-solver address.
    let sub_solver =
        eip712::recover_proposer(&signature, state.domain(), &proposal, interactions_hash)
            .map_err(|_| Error::from(Kind::SignatureRecoveryFailed))?;

    tracing::info!(%sub_solver, "proposal signature verified");

    // 6. Reject proposals that are already expired — no point accepting,
    // storing, and auditing a DOA proposal (ADR-0001).
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_secs();
    if valid_until < U256::from(now) {
        return Err(Error::from(Kind::ProposalExpired));
    }

    // 7. Store as Submitted. The background validator picks it up and flips
    // it to Active or Rejected; sub-solvers poll GET /proposal/{id} for the
    // verdict.
    let stored = crate::domain::proposal::Proposal {
        id: ProposalId(0),
        sub_solver,
        order_uid,
        order_uid_hash,
        sell_amount,
        buy_amount,
        sell_token: body.sell_token,
        buy_token: body.buy_token,
        interactions,
        interactions_hash,
        valid_until,
        nonce,
        signature: Bytes::from(signature_bytes),
        status: ProposalStatus::Submitted,
        rejection_reason: None,
        gas_used: None,
        trampoline: None,
        created_at: Instant::now(),
    };

    let id = state.store().insert(stored);

    tracing::info!(%id, %sub_solver, "proposal accepted for validation");

    Ok((StatusCode::ACCEPTED, Json(CreateProposalResponse { id })))
}

// ---------------------------------------------------------------------------
// GET /proposal/{id}
// ---------------------------------------------------------------------------

pub async fn get_proposal(
    State(state): State<AppState>,
    Path(id): Path<ProposalId>,
    headers: HeaderMap,
) -> Result<Json<GetProposalResponse>, Error> {
    let reader = authenticate_reader(&headers, state.domain())?;

    let proposal = state
        .store()
        .get(id)
        // Non-owners get the same 404 as a genuine miss so proposal IDs
        // cannot be probed for existence (ADR-0011).
        .filter(|p| p.sub_solver == reader)
        .ok_or(Error::from(Kind::ProposalNotFound))?;

    Ok(Json(GetProposalResponse {
        id: proposal.id,
        sub_solver: proposal.sub_solver,
        order_uid: proposal.order_uid.to_string(),
        sell_amount: proposal.sell_amount.to_string(),
        buy_amount: proposal.buy_amount.to_string(),
        valid_until: proposal.valid_until.to_string(),
        status: proposal.status.to_string(),
        rejection_reason: proposal.rejection_reason,
    }))
}

// ---------------------------------------------------------------------------
// GET /proposals/{order_uid}
// ---------------------------------------------------------------------------

pub async fn list_proposals(
    State(state): State<AppState>,
    Path(order_uid_hex): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ListProposalsResponse>, Error> {
    let reader = authenticate_reader(&headers, state.domain())?;

    let order_uid = OrderUid::from_hex(&order_uid_hex)
        .map_err(|e| Error::new(Kind::BadRequest, format!("invalid orderUid: {e}")))?;

    let proposals = state.store().list_by_order_uid(&order_uid);

    Ok(Json(ListProposalsResponse {
        proposals: proposals
            .iter()
            // Owner-scoped reads (ADR-0011): competitors' proposals on the
            // same order are invisible to the caller.
            .filter(|p| p.sub_solver == reader)
            .map(|p| ProposalMetadata::from(p.as_ref()))
            .collect(),
    }))
}

// ---------------------------------------------------------------------------
// GET /proposals/by-solver
// ---------------------------------------------------------------------------

/// Lists the caller's own active proposals. The caller's identity comes
/// entirely from the `X-Signature` header — there is no address parameter
/// (ADR-0011).
pub async fn list_proposals_by_solver(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ListProposalsResponse>, Error> {
    let reader = authenticate_reader(&headers, state.domain())?;

    let proposals = state.store().list_by_sub_solver(reader);

    Ok(Json(ListProposalsResponse {
        proposals: proposals
            .iter()
            .map(|p| ProposalMetadata::from(p.as_ref()))
            .collect(),
    }))
}

// ---------------------------------------------------------------------------
// DELETE /proposal/{id}
// ---------------------------------------------------------------------------

pub async fn cancel_proposal(
    State(state): State<AppState>,
    Path(id): Path<ProposalId>,
    headers: HeaderMap,
) -> Result<StatusCode, Error> {
    // 1. Extract signature from X-Signature header.
    let signature = signature_from_header(&headers)?;

    // 2. Recover signer from CancelProposal EIP-712 message.
    let signer = eip712::recover_canceller(&signature, state.domain(), U256::from(id.0))
        .map_err(|_| Error::from(Kind::SignatureRecoveryFailed))?;

    // 3. Cancel the proposal (store checks ownership).
    state.store().cancel(id, signer).map_err(|e| match e {
        crate::domain::proposal::StoreError::NotFound(_) => Error::from(Kind::ProposalNotFound),
        crate::domain::proposal::StoreError::NotOwner(_, _) => Error::from(Kind::NotProposalOwner),
        crate::domain::proposal::StoreError::StaleTransition { .. } => {
            Error::from(Kind::ProposalNotCancellable)
        }
    })?;

    tracing::info!(%id, %signer, "proposal cancelled");

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Authenticates a GET request: extracts the `X-Signature` bearer token and
/// recovers the reader's address from the `ReadAuth` EIP-712 message
/// (ADR-0011). The returned address scopes what the caller may read.
fn authenticate_reader(
    headers: &HeaderMap,
    domain: &alloy::sol_types::Eip712Domain,
) -> Result<alloy::primitives::Address, Error> {
    let signature = signature_from_header(headers)?;
    eip712::recover_reader(&signature, domain)
        .map_err(|_| Error::from(Kind::SignatureRecoveryFailed))
}

fn signature_from_header(headers: &HeaderMap) -> Result<Signature, Error> {
    let value = headers.get("X-Signature").ok_or(Error::new(
        Kind::InvalidSignature,
        "missing X-Signature header",
    ))?;
    let hex_str = value
        .to_str()
        .map_err(|_| Error::new(Kind::InvalidSignature, "X-Signature is not valid UTF-8"))?;
    let bytes = parse_hex(hex_str)
        .map_err(|_| Error::new(Kind::InvalidSignature, "X-Signature is not valid hex"))?;
    Signature::try_from(bytes.as_slice())
        .map_err(|_| Error::new(Kind::InvalidSignature, "invalid signature bytes"))
}
