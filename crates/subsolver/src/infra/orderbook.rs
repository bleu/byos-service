//! Client for the public CoW orderbook: polls `GET /api/v1/auction` — the
//! same discovery channel a production sub-solver uses — and converts the
//! solvable batch to domain orders at the edge (ADR-0005).
//!
//! Eligibility mirrors the validation envelope BYOS enforces in
//! `OrderRecord::check_envelope` (ADR-0012): no hooks, plain `erc20` balance
//! locations. Both fill-or-kill and partially fillable orders are accepted.
//! BYOS stays the authority on the envelope. This
//! copy exists so the reference client states its limits where an integrator
//! will read them, and so it does not spend a submission on a proposal
//! certain to be rejected: `POST /proposals` only checks the signature, so
//! an out-of-envelope order costs a stored row, an orderbook fetch, and a
//! verdict poll before the sub-solver learns anything. Hook support is the
//! gap tracked in COW-1197; closing it deletes the two interaction
//! conditions here. Should the two definitions drift apart, the poll loop
//! logs the resulting `UnsupportedOrder` verdict rather than absorbing it
//! (see `run.rs`).
//!
//! Every condition reads a field the auction already carries, so none of it
//! costs an extra request: an auction order has no `fullAppData`, but the
//! orderbook expands app-data hooks into `preInteractions` and
//! `postInteractions` before serving them. Missing fields fail open, since a
//! filter that silently discovers nothing is the bug this one replaced.

use {
    crate::domain::proposal::{Order, OrderKind},
    alloy::primitives::{Address, Bytes, U256},
    reqwest::Url,
    serde::Deserialize,
    serde_with::{DisplayFromStr, serde_as},
};

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
    /// App-data hooks, already expanded into concrete calls by the orderbook
    /// — the auction never carries `fullAppData`. Only emptiness matters, so
    /// the elements are parsed and discarded.
    #[serde(default)]
    pre_interactions: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    post_interactions: Vec<serde::de::IgnoredAny>,
    /// Balance locations. Deprecated in the orderbook's schema — only
    /// `erc20` is accepted for new orders — so in practice these never
    /// exclude anything; they are read so the filter states the whole
    /// envelope rather than most of it.
    #[serde(default = "erc20")]
    sell_token_balance: String,
    #[serde(default = "erc20")]
    buy_token_balance: String,
}

/// The orderbook schema's own default for the balance-location fields, and
/// the value that keeps an order inside the envelope when a deprecated field
/// finally disappears from the response.
fn erc20() -> String {
    "erc20".to_owned()
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Kind {
    Sell,
    Buy,
}

impl OrderbookClient {
    pub fn new(base_url: Url) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
        }
    }

    /// Fetches the current auction and returns the orders this sub-solver is
    /// willing to route (see module docs for the eligibility rules).
    pub async fn solvable_orders(&self) -> Result<Vec<Order>, Error> {
        let url = self
            .base_url
            .join("/api/v1/auction")
            .expect("base url joined with a valid path");
        let auction: Auction = self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(auction
            .orders
            .into_iter()
            .filter(|order| {
                order.pre_interactions.is_empty()
                    && order.post_interactions.is_empty()
                    && order.sell_token_balance == "erc20"
                    && order.buy_token_balance == "erc20"
            })
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
    use {
        super::*,
        crate::domain::proposal::OrderKind,
        serde_json::json,
        wiremock::{
            Mock,
            MockServer,
            ResponseTemplate,
            matchers::{method, path},
        },
    };

    /// One auction order carrying every field the orderbook's `AuctionOrder`
    /// schema marks required, with in-envelope values throughout. Cases
    /// override only the field they exercise, so a schema drift in anything
    /// else shows up here rather than against a live orderbook.
    fn auction_order(uid_byte: u8) -> serde_json::Value {
        json!({
            "uid": format!("0x{}", format!("{uid_byte:02x}").repeat(56)),
            "sellToken": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
            "buyToken": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            "sellAmount": "1000",
            "buyAmount": "900",
            "created": "1750000000",
            "validTo": 1_785_170_912_u32,
            "kind": "sell",
            "receiver": "0x0000000000000000000000000000000000000001",
            "owner": "0x0000000000000000000000000000000000000001",
            "partiallyFillable": false,
            "executed": "0",
            "preInteractions": [],
            "postInteractions": [],
            "sellTokenBalance": "erc20",
            "buyTokenBalance": "erc20",
            "class": "market",
            // A real quote-derived app-data hash (the same mainnet order the
            // service-side fixture in crates/byos/src/infra/orderbook.rs
            // uses). Every order placed through a CoW orderbook carries one.
            "appData": "0x06ebf0fd49ea441fbd174e445f37f792eb8ee8848c66c470f59d06a1c3e318a4",
            "signature": format!("0x{}", "11".repeat(65)),
            "protocolFees": [],
        })
    }

    /// Serves `orders` as the current auction and returns what the
    /// sub-solver is willing to route.
    async fn solvable(orders: Vec<serde_json::Value>) -> Vec<Order> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/auction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 1,
                "block": 100,
                "orders": orders,
            })))
            .mount(&server)
            .await;

        OrderbookClient::new(server.uri().parse().unwrap())
            .solvable_orders()
            .await
            .expect("auction should fetch")
    }

    #[tokio::test]
    async fn solvable_orders_keeps_in_envelope_fill_or_kill_orders() {
        // Buy orders are inside the envelope as much as sell orders are;
        // kept here so the kind mapping stays covered.
        let mut buy = auction_order(0x22);
        buy["kind"] = json!("buy");

        let mut partially_fillable = auction_order(0x66);
        partially_fillable["partiallyFillable"] = json!(true);
        // A pre-hook, as the orderbook expands it for the auction.
        let mut pre_hooked = auction_order(0x33);
        pre_hooked["preInteractions"] = json!([{
            "target": "0x0000000000000000000000000000000000000002",
            "value": "0",
            "callData": "0xdeadbeef",
        }]);

        let mut post_hooked = auction_order(0x44);
        post_hooked["postInteractions"] = json!([{
            "target": "0x0000000000000000000000000000000000000003",
            "value": "0",
            "callData": "0xfeedface",
        }]);

        let mut vault_balance = auction_order(0x55);
        vault_balance["sellTokenBalance"] = json!("external");

        let orders = solvable(vec![
            auction_order(0x11),
            buy,
            partially_fillable,
            pre_hooked,
            post_hooked,
            vault_balance,
        ])
        .await;

        assert_eq!(
            orders.len(),
            3,
            "fill-or-kill and partially fillable orders are both in-envelope"
        );
        assert_eq!(orders[0].uid, Bytes::from(vec![0x11; 56]));
        assert_eq!(orders[0].kind, OrderKind::Sell);
        assert_eq!(orders[0].sell_amount, U256::from(1000));
        assert_eq!(orders[0].buy_amount, U256::from(900));
        assert_eq!(orders[1].uid, Bytes::from(vec![0x22; 56]));
        assert_eq!(orders[2].uid, Bytes::from(vec![0x66; 56]));
        assert_eq!(orders[1].kind, OrderKind::Buy);
    }

    #[tokio::test]
    async fn an_order_missing_the_envelope_fields_is_still_solvable() {
        // The balance fields are already deprecated upstream and the
        // interaction arrays could be reshaped. When a field the filter
        // depends on goes missing the sub-solver must submit and let BYOS
        // judge — the opposite default would silently empty the auction,
        // which is the failure this filter replaced.
        let mut order = auction_order(0x11);
        for field in [
            "preInteractions",
            "postInteractions",
            "sellTokenBalance",
            "buyTokenBalance",
        ] {
            order.as_object_mut().unwrap().remove(field);
        }

        let orders = solvable(vec![order]).await;

        assert_eq!(
            orders.len(),
            1,
            "a missing envelope field must not exclude the order"
        );
    }
}
