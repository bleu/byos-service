-- Track A revert debit evidence (ADR-0003, ADR-0013, COW-1205): the escrow
-- debit transaction that closed a settleFailed proposal's story. Set on the
-- settleFailed → penalized transition; the row itself answers "was this
-- sub-solver charged, and where's the proof". 0x-prefixed lowercase hex.
ALTER TABLE proposals ADD COLUMN penalty_tx_hash TEXT;

-- Non-settlement debit queue (ADR-0003 "won auction, never settled"): the
-- proposal returns to active and keeps competing, so the pending 0.1 × c_l
-- charge cannot live in proposal state — it lives here, written by /notify
-- when the driver confirms an abandoned submission (cancelled/expired/fail
-- after settlementStarted). penalty_tx_hash NULL = pending; the penalty loop
-- debits and fills it in. Money-tier evidence: rows are never swept, and
-- deliberately no FK — the charge outlives a later-dropped proposal row.
CREATE TABLE penalties (
    id          BIGSERIAL   PRIMARY KEY,
    proposal_id BIGINT      NOT NULL,
    sub_solver  TEXT        NOT NULL,
    order_uid   TEXT        NOT NULL,
    penalty_tx_hash TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The penalty loop's per-tick scan: only pending rows.
CREATE INDEX penalties_pending_idx ON penalties (id) WHERE penalty_tx_hash IS NULL;
