# Signature-gated, owner-scoped proposal reads

Status: accepted

Supersedes the "GET metadata: per-proposal, no amounts" section of [ADR-0001](0001-proposal-api.md) and its "pre-settlement information leakage via GET" consequence.

## Context

The proposal GET endpoints shipped in PR #4 (COW-1159) were public, per ADR-0001: anyone could list per-order proposal metadata (id, sub-solver address, validUntil, status), and `GET /proposal/{id}` returned full amounts to any caller who knew an ID. ADR-0001 accepted the metadata visibility on the grounds that sub-solver addresses are recoverable from on-chain settlement calldata anyway.

The PR #4 review (COW-1169) reversed that stance: no proposal information should leak to other sub-solvers at all. Pre-settlement, even "which addresses are competing on which order" is competitively useful, and full amounts behind an unauthenticated ID lookup is a leak, not a trade-off.

Callers are sub-solver servers, not browsers — machine-to-machine over TLS, no sessions.

## Decision

### Every read is authenticated and owner-scoped

All proposal GET endpoints require an EIP-712 signature in the `X-Signature` header (the same header DELETE uses). The recovered signer is the caller's identity and scopes the response:

- `GET /proposal/{id}` — returns the proposal only if the recovered signer is its sub-solver.
- `GET /proposals/{order_uid}` — returns only the caller's own proposals on that order; competitors' proposals are invisible.
- `GET /proposals/by-solver` — returns the caller's proposals. The address path parameter was dropped: identity comes entirely from the signature, so an address in the URL would be either redundant or a mismatch error for a value the server already knows.

### The read token is a bearer signature

The signed message is a dedicated EIP-712 type, owned by this repo (like `CancelProposal`, never verified on-chain):

```solidity
struct ReadAuth {
    uint256 version; // pinned to 1
}
```

Signed once, sent on every request. No timestamp, no nonce, no path binding — it is a bearer token. Reasoning:

- **Blast radius of a leaked token is small.** It grants read access to the signer's *own* proposals only — no writes, no cancellation, no funds. Unused proposals are short-lived (a discard API is planned), and settled proposals can become public later anyway.
- **No off-chain nonce bookkeeping**, consistent with ADR-0001's stateless stance — a nonce set for reads would be the first per-signer auth state in the service.
- **No clock coupling.** A timestamp window would make external teams' clock drift a support burden for marginal benefit.
- The distinct `ReadAuth` typehash is what prevents cross-type replay: a read signature can never be replayed as a proposal submission or cancellation.
- `version` exists because EIP-712 structs need at least one field; bumping it invalidates all outstanding read tokens without renaming the type.

### Non-owners get 404, not 403

`GET /proposal/{id}` returns the same 404 for "does not exist" and "exists but not yours". A 403 would be an existence oracle — anyone could probe IDs 1..N and learn how many proposals are live. Since the point of this decision is that nothing leaks to non-owners, the two cases are indistinguishable on the wire.

## Alternatives considered

- **Keep reads public (ADR-0001 status quo).** Rejected — full amounts behind an unauthenticated ID lookup, and pre-settlement competitor mapping via order-UID listing.
- **Timestamp with validity window in the signed message.** Bounds how long a captured signature stays usable. Rejected — the leak it defends against yields read-only access to the attacker-chosen victim's own data, while the clock-sync failure mode hits every honest integrator.
- **Nonce with server-side replay tracking.** Strongest replay protection. Rejected — introduces per-signer state the service deliberately avoids, for the same marginal benefit as the timestamp.
- **Path binding (sign the request path).** Rejected — every read is already scoped to the signer regardless of path, so binding the path adds signing complexity without changing what a replayed signature can reach.
- **Keep the address path parameter on the by-solver route and verify it matches the signer.** Rejected — the parameter is decorative once identity comes from the signature; a mismatch is just a confusing error.
- **403 for non-owner reads.** Honest, but an existence oracle. Rejected.

## Consequences

- Sub-solvers must sign a `ReadAuth` message and send it on every GET. The signature is a long-lived credential; compromise of the header grants read access to that sub-solver's proposals until the key rotates (which means a new escrow identity — accepted, per the key-rotation stance in ADR-0001).
- The order-UID listing no longer serves as a public "who is competing" view. Any future public statistics (e.g. proposals that settled and earned) are a deliberate, separate API addition.
- The reference `subsolver` client and external integration docs must document the `ReadAuth` signing step.
- Replay of a captured read token is possible indefinitely within a key's lifetime. Accepted: read-only, own-data-only.
