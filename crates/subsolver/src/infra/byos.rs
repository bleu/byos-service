//! HTTP client for the BYOS proposal API (ADR-0001): submit, metadata
//! lookup, and EIP-712-signed cancellation. Rejections surface as the typed
//! `{kind, description}` shape so callers can react to the machine-readable
//! reason.

use alloy::{primitives::Bytes, signers::local::PrivateKeySigner, sol_types::Eip712Domain};
use reqwest::{StatusCode, Url};

use crate::domain::signing::sign_cancellation;

/// Client for one BYOS instance's public proposal endpoints.
pub struct ByosClient {
    http: reqwest::Client,
    base_url: Url,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The service answered with a machine-readable rejection (4xx/5xx with
    /// the ADR-0007 body).
    #[error("proposal rejected: {0:?}")]
    Rejected(proposal_dto::Error),
    /// The service answered outside the API contract (no typed error body).
    #[error("unexpected status {0}")]
    UnexpectedStatus(StatusCode),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

impl ByosClient {
    pub fn new(base_url: Url) -> Self {
        Self { http: reqwest::Client::new(), base_url }
    }

    /// `POST /proposals`: submits a signed proposal, returning the
    /// server-assigned id, or the typed rejection reason.
    pub async fn submit(&self, proposal: &proposal_dto::Proposal) -> Result<u64, Error> {
        let response = self
            .http
            .post(self.url("/proposals"))
            .json(proposal)
            .send()
            .await?;
        let submitted: proposal_dto::Submitted = Self::parse(response).await?;
        Ok(submitted.id)
    }

    /// `GET /proposals/{order_uid}`: this order's proposal metadata; the
    /// discovery channel for cancellation ids.
    pub async fn proposals(&self, order_uid: &Bytes) -> Result<proposal_dto::Metadata, Error> {
        let response = self
            .http
            .get(self.url(&format!("/proposals/{order_uid}")))
            .send()
            .await?;
        Self::parse(response).await
    }

    /// `DELETE /proposals/{id}`: cancels one of the signer's own proposals
    /// via an EIP-712 `CancelProposal` signature.
    pub async fn cancel(
        &self,
        proposal_id: u64,
        domain: &Eip712Domain,
        signer: &PrivateKeySigner,
    ) -> Result<(), Error> {
        let cancellation =
            proposal_dto::Cancellation { signature: sign_cancellation(proposal_id, domain, signer) };
        let response = self
            .http
            .delete(self.url(&format!("/proposals/{proposal_id}")))
            .json(&cancellation)
            .send()
            .await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(Self::error(response).await)
    }

    fn url(&self, path: &str) -> Url {
        self.base_url.join(path).expect("base url joined with a valid path")
    }

    async fn parse<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T, Error> {
        if response.status().is_success() {
            return Ok(response.json().await?);
        }
        Err(Self::error(response).await)
    }

    /// Maps a non-2xx response to the typed rejection when the body follows
    /// ADR-0007, or `UnexpectedStatus` when it doesn't.
    async fn error(response: reqwest::Response) -> Error {
        let status = response.status();
        match response.json::<proposal_dto::Error>().await {
            Ok(rejection) => Error::Rejected(rejection),
            Err(_) => Error::UnexpectedStatus(status),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::{Address, Bytes, Signature, U256},
        signers::local::PrivateKeySigner,
    };
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, method, path},
    };

    use super::*;
    use crate::domain::signing::{cancellation_digest, proposal_domain};

    fn proposal() -> proposal_dto::Proposal {
        proposal_dto::Proposal {
            order_uid: vec![0x11; 56].into(),
            sell_amount: U256::from(1000),
            buy_amount: U256::from(906),
            interactions: vec![],
            valid_until: 1_750_000_000,
            nonce: U256::from(7),
            signature: vec![0x22; 65].into(),
        }
    }

    #[tokio::test]
    async fn submit_posts_the_proposal_and_returns_the_assigned_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/proposals"))
            .and(body_json(serde_json::to_value(proposal()).unwrap()))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 42 })))
            .expect(1)
            .mount(&server)
            .await;

        let client = ByosClient::new(server.uri().parse().unwrap());
        assert_eq!(client.submit(&proposal()).await.unwrap(), 42);
    }

    #[tokio::test]
    async fn submit_surfaces_typed_rejections() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/proposals"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "kind": "UnderCollateralized",
                "description": "escrow below gas + c_l",
            })))
            .mount(&server)
            .await;

        let client = ByosClient::new(server.uri().parse().unwrap());
        match client.submit(&proposal()).await.unwrap_err() {
            Error::Rejected(rejection) => {
                assert_eq!(rejection.kind, proposal_dto::Kind::UnderCollateralized);
            }
            other => panic!("expected typed rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn proposals_fetches_metadata_by_order_uid() {
        let server = MockServer::start().await;
        let order_uid = Bytes::from(vec![0x11; 56]);
        Mock::given(method("GET"))
            .and(path(format!("/proposals/{order_uid}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "proposals": [
                    { "id": 42, "solver": Address::ZERO, "validUntil": 1_750_000_000u64, "status": "active" },
                ]
            })))
            .mount(&server)
            .await;

        let client = ByosClient::new(server.uri().parse().unwrap());
        let metadata = client.proposals(&order_uid).await.unwrap();
        assert_eq!(metadata.proposals[0].id, 42);
    }

    #[tokio::test]
    async fn cancel_sends_a_signature_recovering_to_the_sub_solver() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/proposals/42"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let signer = PrivateKeySigner::from_bytes(&U256::from(0xA11CE).into()).unwrap();
        let domain = proposal_domain(31337, Address::ZERO);
        let client = ByosClient::new(server.uri().parse().unwrap());
        client.cancel(42, &domain, &signer).await.unwrap();

        // The DELETE body must carry an EIP-712 CancelProposal signature the
        // server can recover the sub-solver from.
        let request = &server.received_requests().await.unwrap()[0];
        let cancellation: proposal_dto::Cancellation = serde_json::from_slice(&request.body).unwrap();
        let signature = Signature::from_raw(&cancellation.signature).unwrap();
        let recovered = signature.recover_address_from_prehash(&cancellation_digest(42, &domain)).unwrap();
        assert_eq!(recovered, signer.address());
    }
}
