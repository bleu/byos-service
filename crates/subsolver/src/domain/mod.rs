//! Pure business logic (ADR-0005): routing math and proposal building. No
//! IO. EIP-712 signing lives in `byos_common::eip712`, shared with the
//! service.

pub mod proposal;
pub mod routing;
