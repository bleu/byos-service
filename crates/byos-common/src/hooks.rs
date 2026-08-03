//! Order hooks: user-specified Ethereum calls executed as settlement
//! pre- or post-interactions via the `HooksTrampoline` contract.

use {
    crate::contracts::{GPv2InteractionData, HooksTrampoline},
    alloy::{
        primitives::{Address, Bytes, U256},
        sol_types::SolCall,
    },
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

    /// Encodes pre- and post-hooks as `HooksTrampoline.execute()` interactions.
    /// Returns empty vecs when `hooks_trampoline` is `None` or when hooks are
    /// empty.
    pub fn encode_interactions(
        &self,
        hooks_trampoline: Option<Address>,
    ) -> (Vec<GPv2InteractionData>, Vec<GPv2InteractionData>) {
        match hooks_trampoline {
            Some(ht) => (
                encode_hooks_interaction(&self.pre, ht),
                encode_hooks_interaction(&self.post, ht),
            ),
            None => (vec![], vec![]),
        }
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

/// Encodes a list of hooks as a single `HooksTrampoline.execute(hooks)`
/// interaction, or returns an empty vec when there are no hooks.
pub fn encode_hooks_interaction(
    hooks: &[Hook],
    hooks_trampoline: Address,
) -> Vec<GPv2InteractionData> {
    if hooks.is_empty() {
        return vec![];
    }

    let abi_hooks: Vec<HooksTrampoline::Hook> = hooks
        .iter()
        .map(|h| HooksTrampoline::Hook {
            target: h.target,
            callData: h.call_data.clone(),
            gasLimit: h.gas_limit,
        })
        .collect();

    let calldata = HooksTrampoline::executeCall { hooks: abi_hooks };

    vec![GPv2InteractionData {
        target: hooks_trampoline,
        value: U256::ZERO,
        callData: calldata.abi_encode().into(),
    }]
}

#[cfg(test)]
mod tests {
    use {super::*, alloy::primitives::address};

    #[test]
    fn empty_hooks_produce_no_interactions() {
        let result =
            encode_hooks_interaction(&[], address!("0000000000000000000000000000000000001234"));
        assert!(result.is_empty());
    }

    #[test]
    fn single_hook_produces_one_interaction() {
        let hooks_trampoline = address!("0000000000000000000000000000000000001234");
        let hooks = vec![Hook {
            target: address!("0000000000000000000000000000000000005678"),
            call_data: Bytes::from(vec![0xab, 0xcd]),
            gas_limit: U256::from(100_000_u64),
        }];

        let result = encode_hooks_interaction(&hooks, hooks_trampoline);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].target, hooks_trampoline);
        assert_eq!(result[0].value, U256::ZERO);

        // Verify the calldata decodes back to the same hooks.
        let decoded = HooksTrampoline::executeCall::abi_decode(&result[0].callData)
            .expect("should decode as execute()");
        assert_eq!(decoded.hooks.len(), 1);
        assert_eq!(decoded.hooks[0].target, hooks[0].target);
        assert_eq!(decoded.hooks[0].callData, hooks[0].call_data);
        assert_eq!(decoded.hooks[0].gasLimit, hooks[0].gas_limit);
    }

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
