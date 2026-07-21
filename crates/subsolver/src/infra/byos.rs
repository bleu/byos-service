//! HTTP client for the BYOS proposal API: submit, verdict polling, metadata
//! lookup, and EIP-712-signed cancellation. Wire shapes mirror the server's
//! `byos::infra::api::dto` (camelCase JSON, 256-bit values as decimal
//! strings, bytes as hex — ADR-0005); reads authenticate with the `ReadAuth`
//! bearer signature in the `X-Signature` header (ADR-0011). Rejections
//! surface as the typed `{kind, description}` shape (ADR-0007) so callers
//! can react to the machine-readable reason.

use {
    crate::domain::proposal::SignedProposal,
    alloy::{
        hex,
        primitives::{Address, Bytes, U256},
        signers::local::PrivateKeySigner,
        sol_types::Eip712Domain,
    },
    byos_common::{contracts::Interaction, eip712},
    reqwest::{StatusCode, Url},
    serde::{Deserialize, Serialize},
};

/// Client for one BYOS instance's public proposal endpoints, acting as one
/// sub-solver identity (reads are owner-scoped, ADR-0011).
pub struct ByosClient {
    http: reqwest::Client,
    base_url: Url,
    domain: Eip712Domain,
    signer: PrivateKeySigner,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The service answered with a machine-readable rejection (4xx/5xx with
    /// the ADR-0007 body).
    #[error("proposal rejected: {0:?}")]
    Rejected(Rejection),
    /// The service answered outside the API contract (no typed error body).
    #[error("unexpected status {0}")]
    UnexpectedStatus(StatusCode),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

// ---------------------------------------------------------------------------
// Wire types (client side of `byos::infra::api::dto`)
// ---------------------------------------------------------------------------

/// Body of `POST /proposals`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateProposalRequest {
    order_uid: String,
    sell_amount: String,
    buy_amount: String,
    interactions: Vec<InteractionDto>,
    valid_until: String,
    nonce: String,
    signature: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InteractionDto {
    target: Address,
    value: String,
    call_data: String,
}

impl From<&SignedProposal> for CreateProposalRequest {
    fn from(proposal: &SignedProposal) -> Self {
        Self {
            order_uid: proposal.order_uid.to_string(),
            sell_amount: proposal.sell_amount.to_string(),
            buy_amount: proposal.buy_amount.to_string(),
            interactions: proposal.interactions.iter().map(Into::into).collect(),
            valid_until: proposal.valid_until.to_string(),
            nonce: proposal.nonce.to_string(),
            signature: proposal.signature.to_string(),
        }
    }
}

impl From<&Interaction> for InteractionDto {
    fn from(interaction: &Interaction) -> Self {
        Self {
            target: interaction.target,
            value: interaction.value.to_string(),
            call_data: hex::encode_prefixed(&interaction.callData),
        }
    }
}

/// Body of a 202 `POST /proposals` response: the server-assigned proposal id.
#[derive(Deserialize)]
struct Submitted {
    id: u64,
}

/// Proposal lifecycle status as served by the API. `Unknown` absorbs states
/// newer than this client, so server additions never break deserialization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    /// Signature verified, awaiting background validation (COW-1162).
    Submitted,
    Active,
    Rejected,
    Expired,
    Settled,
    SimFailed,
    Cancelled,
    #[serde(other)]
    Unknown,
}

impl Status {
    /// Whether the proposal can never become executable again — the signal
    /// to resubmit with a fresh nonce.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Rejected | Self::Expired | Self::Settled | Self::SimFailed | Self::Cancelled
        )
    }
}

/// Why the background validator rejected a proposal (PascalCase, ADR-0007).
/// `Unknown` tolerates reasons newer than this client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum RejectionReason {
    InsufficientEscrow,
    #[serde(other)]
    Unknown,
}

/// Body of `GET /proposal/{id}`: the caller's own proposal, including the
/// async validation verdict.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalView {
    pub id: u64,
    pub status: Status,
    /// Only present when `status` is `rejected`.
    #[serde(default)]
    pub rejection_reason: Option<RejectionReason>,
}

/// Body of `GET /proposals/{order_uid}`: per-proposal metadata for the
/// caller's own proposals on that order.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    pub proposals: Vec<ProposalMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalMetadata {
    pub id: u64,
    pub sub_solver: Address,
    pub status: Status,
}

/// Body of a 4xx/5xx response (ADR-0007): machine-readable kind plus a
/// human-oriented description. `Unknown` absorbs kinds newer than the
/// client, so server additions never break deserialization.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Rejection {
    pub kind: Kind,
    pub description: String,
}

/// Error kinds served by `byos::infra::api::error` (PascalCase).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum Kind {
    InvalidSignature,
    SignatureRecoveryFailed,
    InsufficientEscrow,
    ProposalExpired,
    ProposalNotFound,
    NotProposalOwner,
    ProposalNotCancellable,
    BadRequest,
    Internal,
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

impl ByosClient {
    pub fn new(base_url: Url, domain: Eip712Domain, signer: PrivateKeySigner) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
            domain,
            signer,
        }
    }

    /// `POST /proposals`: submits a signed proposal, returning the
    /// server-assigned id, or the typed rejection reason. Acceptance (202)
    /// only means the signature verified — the validation verdict is
    /// asynchronous; poll [`ByosClient::proposal`] for it.
    pub async fn submit(&self, proposal: &SignedProposal) -> Result<u64, Error> {
        let response = self
            .http
            .post(self.url("/proposals"))
            .json(&CreateProposalRequest::from(proposal))
            .send()
            .await?;
        let submitted: Submitted = Self::parse(response).await?;
        Ok(submitted.id)
    }

    /// `GET /proposal/{id}`: one of the signer's own proposals, including
    /// the async validation verdict.
    pub async fn proposal(&self, id: u64) -> Result<ProposalView, Error> {
        let response = self
            .http
            .get(self.url(&format!("/proposal/{id}")))
            .header("X-Signature", self.read_auth().await)
            .send()
            .await?;
        Self::parse(response).await
    }

    /// `GET /proposals/{order_uid}`: the signer's own proposal metadata on
    /// this order; the discovery channel for cancellation ids.
    pub async fn proposals(&self, order_uid: &Bytes) -> Result<Metadata, Error> {
        let response = self
            .http
            .get(self.url(&format!("/proposals/{order_uid}")))
            .header("X-Signature", self.read_auth().await)
            .send()
            .await?;
        Self::parse(response).await
    }

    /// `DELETE /proposal/{id}`: cancels one of the signer's own proposals
    /// via an EIP-712 `CancelProposal` signature in the `X-Signature`
    /// header.
    pub async fn cancel(&self, proposal_id: u64) -> Result<(), Error> {
        let signature =
            eip712::sign_cancellation(&self.signer, &self.domain, U256::from(proposal_id))
                .await
                .expect("in-memory ECDSA signing is infallible");
        let response = self
            .http
            .delete(self.url(&format!("/proposal/{proposal_id}")))
            .header("X-Signature", hex::encode_prefixed(signature.as_bytes()))
            .send()
            .await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(Self::error(response).await)
    }

    /// The `ReadAuth` bearer signature authenticating GET requests
    /// (ADR-0011).
    async fn read_auth(&self) -> String {
        let signature = eip712::sign_read_auth(&self.signer, &self.domain)
            .await
            .expect("in-memory ECDSA signing is infallible");
        hex::encode_prefixed(signature.as_bytes())
    }

    fn url(&self, path: &str) -> Url {
        self.base_url
            .join(path)
            .expect("base url joined with a valid path")
    }

    async fn parse<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
    ) -> Result<T, Error> {
        if response.status().is_success() {
            return Ok(response.json().await?);
        }
        Err(Self::error(response).await)
    }

    /// Maps a non-2xx response to the typed rejection when the body follows
    /// ADR-0007, or `UnexpectedStatus` when it doesn't.
    async fn error(response: reqwest::Response) -> Error {
        let status = response.status();
        match response.json::<Rejection>().await {
            Ok(rejection) => Error::Rejected(rejection),
            Err(_) => Error::UnexpectedStatus(status),
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        alloy::{
            primitives::{Address, Bytes, Signature, U256},
            signers::local::PrivateKeySigner,
        },
        serde_json::json,
        wiremock::{
            Mock,
            MockServer,
            ResponseTemplate,
            matchers::{body_json, method, path},
        },
    };

    fn signer() -> PrivateKeySigner {
        PrivateKeySigner::from_bytes(&U256::from(0xA11CE).into()).unwrap()
    }

    fn domain() -> Eip712Domain {
        eip712::byos_domain(31337, Address::ZERO)
    }

    fn client(server: &MockServer) -> ByosClient {
        ByosClient::new(server.uri().parse().unwrap(), domain(), signer())
    }

    fn proposal() -> SignedProposal {
        SignedProposal {
            order_uid: vec![0x11; 56].into(),
            sell_amount: U256::from(1000),
            buy_amount: U256::from(906),
            interactions: vec![Interaction {
                target: Address::repeat_byte(0xab),
                value: U256::ZERO,
                callData: vec![0xde, 0xad].into(),
            }],
            valid_until: 1_750_000_000,
            nonce: U256::from(7),
            signature: vec![0x22; 65].into(),
        }
    }

    /// The exact `POST /proposals` body the server's DTO expects: camelCase,
    /// decimal strings for 256-bit values, hex strings for bytes.
    fn wire_proposal() -> serde_json::Value {
        json!({
            "orderUid": format!("0x{}", "11".repeat(56)),
            "sellAmount": "1000",
            "buyAmount": "906",
            "interactions": [{
                "target": Address::repeat_byte(0xab),
                "value": "0",
                "callData": "0xdead",
            }],
            "validUntil": "1750000000",
            "nonce": "7",
            "signature": format!("0x{}", "22".repeat(65)),
        })
    }

    #[tokio::test]
    async fn submit_posts_the_wire_shape_and_returns_the_assigned_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/proposals"))
            .and(body_json(wire_proposal()))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "id": 42 })))
            .expect(1)
            .mount(&server)
            .await;

        assert_eq!(client(&server).submit(&proposal()).await.unwrap(), 42);
    }

    #[tokio::test]
    async fn submit_surfaces_typed_rejections() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/proposals"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "kind": "InvalidSignature",
                "description": "Invalid EIP-712 signature",
            })))
            .mount(&server)
            .await;

        match client(&server).submit(&proposal()).await.unwrap_err() {
            Error::Rejected(rejection) => assert_eq!(rejection.kind, Kind::InvalidSignature),
            other => panic!("expected typed rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_rejection_kinds_do_not_break_the_client() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/proposals"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "kind": "SomethingNewer",
                "description": "?",
            })))
            .mount(&server)
            .await;

        match client(&server).submit(&proposal()).await.unwrap_err() {
            Error::Rejected(rejection) => assert_eq!(rejection.kind, Kind::Unknown),
            other => panic!("expected typed rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn proposal_polls_the_verdict_with_read_auth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/proposal/42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 42,
                "subSolver": signer().address(),
                "orderUid": format!("0x{}", "11".repeat(56)),
                "sellAmount": "1000",
                "buyAmount": "906",
                "validUntil": "1750000000",
                "status": "rejected",
                "rejectionReason": "InsufficientEscrow",
            })))
            .mount(&server)
            .await;

        let view = client(&server).proposal(42).await.unwrap();
        assert_eq!(view.status, Status::Rejected);
        assert!(view.status.is_terminal());
        assert_eq!(
            view.rejection_reason,
            Some(RejectionReason::InsufficientEscrow)
        );

        // The read-auth bearer signature recovers to the sub-solver.
        let request = &server.received_requests().await.unwrap()[0];
        let header = request
            .headers
            .get("X-Signature")
            .unwrap()
            .to_str()
            .unwrap();
        let signature = Signature::from_raw(&hex::decode(header).unwrap()).unwrap();
        let recovered = eip712::recover_reader(&signature, &domain()).unwrap();
        assert_eq!(recovered, signer().address());
    }

    #[tokio::test]
    async fn proposals_fetches_metadata_by_order_uid() {
        let server = MockServer::start().await;
        let order_uid = Bytes::from(vec![0x11; 56]);
        Mock::given(method("GET"))
            .and(path(format!("/proposals/{order_uid}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "proposals": [
                    { "id": 42, "subSolver": Address::ZERO, "validUntil": "1750000000", "status": "active" },
                ]
            })))
            .mount(&server)
            .await;

        let metadata = client(&server).proposals(&order_uid).await.unwrap();
        assert_eq!(metadata.proposals[0].id, 42);
        assert_eq!(metadata.proposals[0].status, Status::Active);
    }

    #[tokio::test]
    async fn cancel_sends_a_signature_recovering_to_the_sub_solver() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/proposal/42"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        client(&server).cancel(42).await.unwrap();

        // The X-Signature header must carry an EIP-712 CancelProposal
        // signature the server can recover the sub-solver from.
        let request = &server.received_requests().await.unwrap()[0];
        let header = request
            .headers
            .get("X-Signature")
            .unwrap()
            .to_str()
            .unwrap();
        let signature = Signature::from_raw(&hex::decode(header).unwrap()).unwrap();
        let recovered = eip712::recover_canceller(&signature, &domain(), U256::from(42)).unwrap();
        assert_eq!(recovered, signer().address());
    }
}
