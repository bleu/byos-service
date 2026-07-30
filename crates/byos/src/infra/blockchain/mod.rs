//! Chain access: the escrow collateral check, the full-settle simulation
//! (ADR-0012), the composite proposal validator that sequences them, and
//! the escrow operator that submits Track A debits (ADR-0003).

pub mod escrow;
pub mod operator;
pub mod simulation;
pub mod validator;
