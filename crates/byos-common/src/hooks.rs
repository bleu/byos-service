//! Order hooks: user-specified Ethereum calls declared in
//! `fullAppData.metadata.hooks`.
//!
//! The orderbook already encodes these as trampoline-wrapped interactions on
//! the order's `interactions` field, so the service uses those pre-encoded
//! interactions directly rather than re-encoding from the raw hook structs.
//! These types are kept for reference and potential future use.

use {
    alloy::primitives::{Address, Bytes, U256},
    serde::{Deserialize, Serialize},
};

/// Pre- and post-hooks attached to an order via `fullAppData.metadata.hooks`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hooks {
    #[serde(default)]
    pub pre: Vec<Hook>,
    #[serde(default)]
    pub post: Vec<Hook>,
}

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty()
    }
}

/// A single hook: an external call the settlement executes on behalf of
/// the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hook {
    pub target: Address,
    pub call_data: Bytes,
    pub gas_limit: U256,
}

#[cfg(test)]
mod tests {
    use {super::*, alloy::primitives::address};

    #[test]
    fn hooks_default_is_empty() {
        let hooks = Hooks::default();
        assert!(hooks.is_empty());
        assert!(hooks.pre.is_empty());
        assert!(hooks.post.is_empty());
    }

    #[test]
    fn hooks_serde_round_trip() {
        let hooks = Hooks {
            pre: vec![Hook {
                target: address!("0000000000000000000000000000000000005678"),
                call_data: Bytes::from(vec![0xab, 0xcd]),
                gas_limit: U256::from(100_000_u64),
            }],
            post: vec![],
        };

        let json = serde_json::to_string(&hooks).expect("serialize");
        let deserialized: Hooks = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(hooks, deserialized);
    }
}
