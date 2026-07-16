//! Client for the public CoW orderbook: polls `GET /api/v1/auction` — the
//! same discovery channel a production sub-solver uses — and converts the
//! solvable batch to domain orders at the edge (ADR-0005).
//!
//! Eligibility is deliberately narrow for a reference implementation:
//! fill-or-kill orders only, and only ones with the default (all-zero)
//! `appData`, since non-default app data may declare pre/post hooks the
//! sub-solver would be responsible for including in its route (ADR-0001) —
//! hook decoding is out of scope for this testing tool.

use alloy::primitives::{Address, B256, Bytes, U256};
use reqwest::Url;
use serde::Deserialize;
use serde_with::{DisplayFromStr, serde_as};

use crate::domain::proposal::{Order, OrderKind};

/// Client for one CoW orderbook instance.
pub struct OrderbookClient {
    http: reqwest::Client,
    base_url: Url,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

/// The slice of the auction response the sub-solver consumes. Unknown fields
/// are tolerated: the orderbook API is external and grows fields freely.
#[derive(Deserialize)]
struct Auction {
    orders: Vec<AuctionOrder>,
}

#[serde_as]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuctionOrder {
    uid: Bytes,
    sell_token: Address,
    buy_token: Address,
    #[serde_as(as = "DisplayFromStr")]
    sell_amount: U256,
    #[serde_as(as = "DisplayFromStr")]
    buy_amount: U256,
    kind: Kind,
    partially_fillable: bool,
    app_data: B256,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Kind {
    Sell,
    Buy,
}

impl OrderbookClient {
    pub fn new(base_url: Url) -> Self {
        Self { http: reqwest::Client::new(), base_url }
    }

    /// Fetches the current auction and returns the orders this sub-solver is
    /// willing to route (see module docs for the eligibility rules).
    pub async fn solvable_orders(&self) -> Result<Vec<Order>, Error> {
        let url = self.base_url.join("/api/v1/auction").expect("base url joined with a valid path");
        let auction: Auction = self.http.get(url).send().await?.error_for_status()?.json().await?;
        Ok(auction
            .orders
            .into_iter()
            .filter(|order| !order.partially_fillable && order.app_data == B256::ZERO)
            .map(|order| Order {
                uid: order.uid,
                sell_token: order.sell_token,
                buy_token: order.buy_token,
                sell_amount: order.sell_amount,
                buy_amount: order.buy_amount,
                kind: match order.kind {
                    Kind::Sell => OrderKind::Sell,
                    Kind::Buy => OrderKind::Buy,
                },
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;
    use crate::domain::proposal::OrderKind;

    #[tokio::test]
    async fn solvable_orders_keeps_default_app_data_fill_or_kill_orders_only() {
        let server = MockServer::start().await;
        let order = |uid_byte: u8, kind: &str, partially_fillable: bool, app_data: &str| {
            json!({
                "uid": format!("0x{}", format!("{uid_byte:02x}").repeat(56)),
                "sellToken": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                "buyToken": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "sellAmount": "1000",
                "buyAmount": "900",
                "kind": kind,
                "partiallyFillable": partially_fillable,
                "appData": app_data,
                // Fields the reference sub-solver does not consume, present
                // in real auction responses and required to be tolerated.
                "owner": "0x0000000000000000000000000000000000000001",
                "class": "market",
            })
        };
        let zero_app_data = format!("0x{}", "00".repeat(32));
        Mock::given(method("GET"))
            .and(path("/api/v1/auction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 1,
                "block": 100,
                "orders": [
                    order(0x11, "sell", false, &zero_app_data),
                    order(0x22, "buy", false, &zero_app_data),
                    order(0x33, "sell", true, &zero_app_data),
                    order(0x44, "sell", false, &format!("0x{}", "aa".repeat(32))),
                ],
            })))
            .mount(&server)
            .await;

        let client = OrderbookClient::new(server.uri().parse().unwrap());
        let orders = client.solvable_orders().await.unwrap();

        assert_eq!(orders.len(), 2);
        assert_eq!(orders[0].uid, Bytes::from(vec![0x11; 56]));
        assert_eq!(orders[0].kind, OrderKind::Sell);
        assert_eq!(orders[0].sell_amount, U256::from(1000));
        assert_eq!(orders[1].uid, Bytes::from(vec![0x22; 56]));
        assert_eq!(orders[1].kind, OrderKind::Buy);
    }
}
