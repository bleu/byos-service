-- Proposals: the single source of truth for current proposal state
-- (ADR-0013). This table holds what *is*; audit_events holds what
-- *happened*. The BIGSERIAL sequence is the proposal-ID authority — IDs
-- stay unique across restarts with no reseed dance.
CREATE TABLE proposals (
    id                 BIGSERIAL   PRIMARY KEY,
    -- 0x-prefixed lowercase hex, matching the audit_events conventions.
    sub_solver         TEXT        NOT NULL,
    order_uid          TEXT        NOT NULL,
    order_uid_hash     TEXT        NOT NULL,
    -- 256-bit amounts as decimal strings (the wire and evidence
    -- representation); comparisons happen in Rust, not SQL.
    sell_amount        TEXT        NOT NULL,
    buy_amount         TEXT        NOT NULL,
    sell_token         TEXT        NOT NULL,
    buy_token          TEXT        NOT NULL,
    -- [{target, value, callData}], same shape as the audit payload.
    interactions       JSONB       NOT NULL,
    interactions_hash  TEXT        NOT NULL,
    valid_until        TEXT        NOT NULL,
    nonce              TEXT        NOT NULL,
    signature          TEXT        NOT NULL,
    status             TEXT        NOT NULL,
    rejection_reason   TEXT,
    gas_used           BIGINT,
    trampoline         TEXT,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- When the current status was entered. The retention sweep deletes
    -- dropped-tier rows (rejected/simFailed/expired/cancelled) once this is
    -- older than --dropped-retention.
    status_changed_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX proposals_order_uid_idx ON proposals (order_uid, status);
CREATE INDEX proposals_sub_solver_idx ON proposals (sub_solver, status);
-- Serves both the validator's live-proposal snapshot and the retention sweep.
CREATE INDEX proposals_status_idx ON proposals (status, status_changed_at);
