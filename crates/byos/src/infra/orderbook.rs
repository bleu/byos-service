//! Client for `GET /api/v1/orders/{uid}` on the CoW orderbook — the source
//! of truth for the order a proposal settles (ADR-0012).
//!
//! Orders are immutable once placed, so successful fetches are cached for
//! the process lifetime. Off-chain soft-cancellation is invisible to this
//! client by design: the proposal's own expiry bounds the staleness window
//! and the driver re-validates orders at settlement time.

use {
    crate::domain::{order::OrderRecord, proposal::OrderUid},
    alloy::primitives::{Address, B256, Bytes, U256, hex},
    byos_common::settlement::{CowOrder, OrderKind, SigningScheme},
    parking_lot::Mutex,
    reqwest::{StatusCode, Url},
    serde::Deserialize,
    serde_with::{DisplayFromStr, serde_as},
    std::collections::HashMap,
};

/// Why an order fetch failed, split by what the validator should do next.
#[derive(Debug, thiserror::Error)]
pub enum OrderbookError {
    /// The orderbook does not know this uid — reject the proposal.
    #[error("order not found")]
    NotFound,
    /// Network trouble or an unexpected response — defer and retry next
    /// tick.
    #[error("transient orderbook error: {0}")]
    Transient(String),
}

/// Source of orderbook orders. The seam the validator mocks in tests;
/// [`OrderbookClient`] is the production implementation.
pub trait FetchOrder: Send + Sync {
    /// Fetches the order for `uid`.
    fn order(
        &self,
        uid: &OrderUid,
    ) -> impl Future<Output = Result<OrderRecord, OrderbookError>> + Send;
}

/// Client for one CoW orderbook instance, with a forever cache keyed by uid.
pub struct OrderbookClient {
    http: reqwest::Client,
    base_url: Url,
    cache: Mutex<HashMap<OrderUid, OrderRecord>>,
}

impl OrderbookClient {
    pub fn new(base_url: Url) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

impl FetchOrder for OrderbookClient {
    /// Fetches the order for `uid`, from cache when already seen.
    async fn order(&self, uid: &OrderUid) -> Result<OrderRecord, OrderbookError> {
        if let Some(record) = self.cache.lock().get(uid).cloned() {
            return Ok(record);
        }

        // Built by string concatenation, not `Url::join`: a join with an
        // absolute path would replace the base URL's own path, silently
        // dropping the network segment of e.g. https://api.cow.fi/mainnet.
        let url = format!(
            "{}/api/v1/orders/0x{}",
            self.base_url.as_str().trim_end_matches('/'),
            hex::encode(uid.0)
        );
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| OrderbookError::Transient(e.to_string()))?;

        if response.status() == StatusCode::NOT_FOUND {
            return Err(OrderbookError::NotFound);
        }
        if !response.status().is_success() {
            return Err(OrderbookError::Transient(format!(
                "unexpected status {}",
                response.status()
            )));
        }

        let dto: OrderDto = response
            .json()
            .await
            .map_err(|e| OrderbookError::Transient(e.to_string()))?;
        let record = dto.into_record();

        self.cache.lock().insert(uid.clone(), record.clone());
        Ok(record)
    }
}

// ── Wire format ──────────────────────────────────────────────────────────────

/// The slice of the orderbook's order schema this client reads. Unknown
/// fields are tolerated: the API is external and grows fields freely.
#[serde_as]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderDto {
    owner: Address,
    receiver: Option<Address>,
    sell_token: Address,
    buy_token: Address,
    #[serde_as(as = "DisplayFromStr")]
    sell_amount: U256,
    #[serde_as(as = "DisplayFromStr")]
    buy_amount: U256,
    #[serde_as(as = "DisplayFromStr")]
    fee_amount: U256,
    valid_to: u32,
    app_data: B256,
    kind: KindDto,
    partially_fillable: bool,
    sell_token_balance: String,
    buy_token_balance: String,
    signing_scheme: SchemeDto,
    signature: Bytes,
    /// JSON document as a string; `metadata.hooks` (or `metadata.bridging`,
    /// which implies hooks) puts the order outside the envelope.
    full_app_data: Option<String>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
enum KindDto {
    Sell,
    Buy,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SchemeDto {
    Eip712,
    EthSign,
    Eip1271,
    PreSign,
}

impl OrderDto {
    fn into_record(self) -> OrderRecord {
        let has_hooks = self
            .full_app_data
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .map(|doc| {
                let metadata = &doc["metadata"];
                !metadata["hooks"].is_null() || !metadata["bridging"].is_null()
            })
            .unwrap_or(false);
        let erc20_balances =
            self.sell_token_balance == "erc20" && self.buy_token_balance == "erc20";

        OrderRecord {
            order: CowOrder {
                sell_token: self.sell_token,
                buy_token: self.buy_token,
                // Zero already means "same as owner" on-chain; normalize to
                // the explicit owner for local readability.
                receiver: self
                    .receiver
                    .filter(|r| *r != Address::ZERO)
                    .unwrap_or(self.owner),
                sell_amount: self.sell_amount,
                buy_amount: self.buy_amount,
                valid_to: self.valid_to,
                app_data: self.app_data,
                fee_amount: self.fee_amount,
                kind: match self.kind {
                    KindDto::Sell => OrderKind::Sell,
                    KindDto::Buy => OrderKind::Buy,
                },
                partially_fillable: self.partially_fillable,
                signing_scheme: match self.signing_scheme {
                    SchemeDto::Eip712 => SigningScheme::Eip712,
                    SchemeDto::EthSign => SigningScheme::EthSign,
                    SchemeDto::Eip1271 => SigningScheme::Eip1271,
                    SchemeDto::PreSign => SigningScheme::PreSign,
                },
                signature: self.signature,
            },
            has_hooks,
            erc20_balances,
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        alloy::primitives::{U256, address, b256},
        byos_common::settlement::{OrderKind, SigningScheme},
        serde_json::json,
        wiremock::{
            Mock,
            MockServer,
            ResponseTemplate,
            matchers::{method, path},
        },
    };

    /// The real mainnet order 0xb9403b4c... as returned by the orderbook
    /// (fetched 2026-07-27), trimmed to the fields the client reads.
    fn real_order_json() -> serde_json::Value {
        json!({
            "uid": "0xb9403b4c8342c3567e5b1928398030f010730c0b1d83657248e4e4e47984d90bd2e80d60aff5377587e49ff32c9bad639d6f68bc6a678be0",
            "owner": "0xd2e80d60aff5377587e49ff32c9bad639d6f68bc",
            "receiver": "0xd2e80d60aff5377587e49ff32c9bad639d6f68bc",
            "sellToken": "0xb1f1ee126e9c96231cc3d3fad7c08b4cf873b1f1",
            "buyToken": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            "sellAmount": "20000002675677095795",
            "buyAmount": "773213156",
            "feeAmount": "0",
            "validTo": 1785170912_u32,
            "appData": "0x06ebf0fd49ea441fbd174e445f37f792eb8ee8848c66c470f59d06a1c3e318a4",
            "kind": "sell",
            "partiallyFillable": false,
            "sellTokenBalance": "erc20",
            "buyTokenBalance": "erc20",
            "signingScheme": "eip712",
            "signature": "0x45bcd35b2abeeafca8cd2ea00bd662ab327e0ffd7cd38319eeff8432fd49409f6e56384a88dcdc050d92b389285c3cfd78c903f3a20f64641b9f907dbf9de8b71c",
            "fullAppData": "{\"appCode\":\"1inch CoW Swap\",\"metadata\":{\"orderClass\":{\"orderClass\":\"market\"},\"quote\":{\"slippageBips\":56}},\"version\":\"1.4.0\"}",
            "status": "open",
        })
    }

    fn fixture_uid() -> OrderUid {
        let bytes = alloy::primitives::hex!(
            "b9403b4c8342c3567e5b1928398030f010730c0b1d83657248e4e4e47984d90bd2e80d60aff5377587e49ff32c9bad639d6f68bc6a678be0"
        );
        OrderUid(bytes)
    }

    async fn client_with(server: &MockServer) -> OrderbookClient {
        OrderbookClient::new(server.uri().parse().unwrap())
    }

    #[tokio::test]
    async fn fetches_and_parses_a_real_order() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/v1/orders/0x{}",
                alloy::primitives::hex::encode(fixture_uid().0)
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(real_order_json()))
            .mount(&server)
            .await;

        let record = client_with(&server)
            .await
            .order(&fixture_uid())
            .await
            .expect("order should fetch");

        assert_eq!(
            record.order.sell_token,
            address!("b1f1ee126e9c96231cc3d3fad7c08b4cf873b1f1")
        );
        assert_eq!(
            record.order.buy_token,
            address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")
        );
        assert_eq!(
            record.order.receiver,
            address!("d2e80d60aff5377587e49ff32c9bad639d6f68bc")
        );
        assert_eq!(
            record.order.sell_amount,
            U256::from(20_000_002_675_677_095_795_u128)
        );
        assert_eq!(record.order.buy_amount, U256::from(773_213_156_u64));
        assert_eq!(record.order.valid_to, 1_785_170_912);
        assert_eq!(
            record.order.app_data,
            b256!("06ebf0fd49ea441fbd174e445f37f792eb8ee8848c66c470f59d06a1c3e318a4")
        );
        assert_eq!(record.order.kind, OrderKind::Sell);
        assert!(!record.order.partially_fillable);
        assert_eq!(record.order.signing_scheme, SigningScheme::Eip712);
        assert_eq!(record.order.signature.len(), 65);
        assert!(!record.has_hooks, "plain metadata is not hooks");
        assert!(record.erc20_balances);
    }

    #[tokio::test]
    async fn base_url_path_prefix_is_preserved() {
        // Production base URLs carry a network segment
        // (https://api.cow.fi/mainnet); it must survive URL construction.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/mainnet/api/v1/orders/0x{}",
                alloy::primitives::hex::encode(fixture_uid().0)
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(real_order_json()))
            .mount(&server)
            .await;

        let client = OrderbookClient::new(format!("{}/mainnet", server.uri()).parse().unwrap());

        client
            .order(&fixture_uid())
            .await
            .expect("order should fetch through the prefixed path");
    }

    #[tokio::test]
    async fn caches_orders_forever() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(real_order_json()))
            .expect(1) // a second HTTP hit fails verification on drop
            .mount(&server)
            .await;

        let client = client_with(&server).await;
        client.order(&fixture_uid()).await.expect("first fetch");
        let cached = client.order(&fixture_uid()).await.expect("cached fetch");

        assert_eq!(
            cached.order.sell_amount,
            U256::from(20_000_002_675_677_095_795_u128)
        );
    }

    #[tokio::test]
    async fn unknown_order_is_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = client_with(&server)
            .await
            .order(&fixture_uid())
            .await
            .expect_err("404 should error");
        assert!(matches!(err, OrderbookError::NotFound));
    }

    #[tokio::test]
    async fn unreachable_orderbook_is_transient() {
        // Nothing listens on this port.
        let client = OrderbookClient::new("http://127.0.0.1:9".parse().unwrap());

        let err = client
            .order(&fixture_uid())
            .await
            .expect_err("connection failure should error");
        assert!(matches!(err, OrderbookError::Transient(_)));
    }

    #[tokio::test]
    async fn bridging_metadata_counts_as_hooks() {
        let server = MockServer::start().await;
        let mut body = real_order_json();
        body["fullAppData"] = json!(
            "{\"appCode\":\"CoW \
             Swap\",\"metadata\":{\"bridging\":{\"destinationChainId\":\"56\"}}}"
        );
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let record = client_with(&server)
            .await
            .order(&fixture_uid())
            .await
            .expect("order should fetch");

        assert!(record.has_hooks);
    }
}
