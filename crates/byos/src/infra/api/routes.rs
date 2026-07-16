//! Route handlers for the public proposal API.

use {
    super::{
        AppState,
        dto::{
            CreateProposalRequest,
            CreateProposalResponse,
            GetProposalResponse,
            InteractionDto,
            ListProposalsResponse,
            ProposalMetadata,
            parse_hex,
            parse_u256,
        },
        error::{Error, Kind},
    },
    crate::domain::proposal::{OrderUid, ProposalStatus},
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
    std::time::Instant,
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
    let order_uid_bytes = parse_hex(&body.order_uid)
        .map_err(|_| Error::new(Kind::BadRequest, "invalid orderUid hex"))?;
    if order_uid_bytes.len() != 56 {
        return Err(Error::new(Kind::BadRequest, "orderUid must be 56 bytes"));
    }
    let mut order_uid_arr = [0u8; 56];
    order_uid_arr.copy_from_slice(&order_uid_bytes);
    let order_uid = OrderUid(order_uid_arr);

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
        .map(dto_to_interaction)
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

    // 6. [M1 placeholder] Escrow balance check — accept unconditionally.
    // Actual on-chain check comes in COW-1162 (proposal simulation).

    // 7. Store the proposal.
    let stored = crate::domain::proposal::Proposal {
        id: 0,
        sub_solver,
        order_uid,
        order_uid_hash,
        sell_amount,
        buy_amount,
        interactions,
        interactions_hash,
        valid_until,
        nonce,
        signature: Bytes::from(signature_bytes),
        status: ProposalStatus::Active,
        created_at: Instant::now(),
    };

    let id = state.store().insert(stored);

    tracing::info!(id, %sub_solver, "proposal accepted");

    Ok((StatusCode::CREATED, Json(CreateProposalResponse { id })))
}

fn dto_to_interaction(dto: &InteractionDto) -> Result<Interaction, Error> {
    let value = parse_u256(&dto.value)
        .map_err(|_| Error::new(Kind::BadRequest, "invalid interaction value"))?;
    let call_data = parse_hex(&dto.call_data)
        .map_err(|_| Error::new(Kind::BadRequest, "invalid interaction callData"))?;
    Ok(Interaction {
        target: dto.target,
        value,
        callData: call_data.into(),
    })
}

// ---------------------------------------------------------------------------
// GET /proposal/{id}
// ---------------------------------------------------------------------------

pub async fn get_proposal(
    State(state): State<AppState>,
    Path(id): Path<u64>,
) -> Result<Json<GetProposalResponse>, Error> {
    let proposal = state
        .store()
        .get(id)
        .ok_or(Error::from(Kind::ProposalNotFound))?;

    Ok(Json(GetProposalResponse {
        id: proposal.id,
        sub_solver: proposal.sub_solver,
        order_uid: format!("0x{}", alloy::hex::encode(proposal.order_uid.0)),
        sell_amount: proposal.sell_amount.to_string(),
        buy_amount: proposal.buy_amount.to_string(),
        valid_until: proposal.valid_until.to_string(),
        status: proposal.status.to_string(),
    }))
}

// ---------------------------------------------------------------------------
// GET /proposals/{order_uid}
// ---------------------------------------------------------------------------

pub async fn list_proposals(
    State(state): State<AppState>,
    Path(order_uid_hex): Path<String>,
) -> Result<Json<ListProposalsResponse>, Error> {
    let order_uid_bytes = parse_hex(&order_uid_hex)
        .map_err(|_| Error::new(Kind::BadRequest, "invalid orderUid hex"))?;
    if order_uid_bytes.len() != 56 {
        return Err(Error::new(Kind::BadRequest, "orderUid must be 56 bytes"));
    }
    let mut arr = [0u8; 56];
    arr.copy_from_slice(&order_uid_bytes);

    let proposals = state.store().list_by_order_uid(&OrderUid(arr));

    Ok(Json(ListProposalsResponse {
        proposals: proposals.iter().map(proposal_to_metadata).collect(),
    }))
}

// ---------------------------------------------------------------------------
// GET /proposals/by-solver/{address}
// ---------------------------------------------------------------------------

pub async fn list_proposals_by_solver(
    State(state): State<AppState>,
    Path(address_hex): Path<String>,
) -> Result<Json<ListProposalsResponse>, Error> {
    let address: alloy::primitives::Address = address_hex
        .parse()
        .map_err(|_| Error::new(Kind::BadRequest, "invalid address"))?;

    let proposals = state.store().list_by_sub_solver(address);

    Ok(Json(ListProposalsResponse {
        proposals: proposals.iter().map(proposal_to_metadata).collect(),
    }))
}

// ---------------------------------------------------------------------------
// DELETE /proposal/{id}
// ---------------------------------------------------------------------------

pub async fn cancel_proposal(
    State(state): State<AppState>,
    Path(id): Path<u64>,
    headers: HeaderMap,
) -> Result<StatusCode, Error> {
    // 1. Extract signature from X-Signature header.
    let signature = signature_from_header(&headers)?;

    // 2. Recover signer from CancelProposal EIP-712 message.
    let signer = eip712::recover_canceller(&signature, state.domain(), U256::from(id))
        .map_err(|_| Error::from(Kind::SignatureRecoveryFailed))?;

    // 3. Cancel the proposal (store checks ownership).
    state.store().cancel(id, signer).map_err(|e| match e {
        crate::domain::proposal::StoreError::NotFound(_) => Error::from(Kind::ProposalNotFound),
        crate::domain::proposal::StoreError::NotOwner(_, _) => Error::from(Kind::NotProposalOwner),
    })?;

    tracing::info!(id, %signer, "proposal cancelled");

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn proposal_to_metadata(p: &crate::domain::proposal::Proposal) -> ProposalMetadata {
    ProposalMetadata {
        id: p.id,
        sub_solver: p.sub_solver,
        valid_until: p.valid_until.to_string(),
        status: p.status.to_string(),
    }
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
