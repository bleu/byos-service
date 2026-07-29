-- Solutions: maps driver notifications (auction_id, solution_id) back to
-- the proposal a bid was built on (ADR-0013). Written synchronously inside
-- /solve before the solution is returned — if we can't record it, we don't
-- bid it. Doubles as the per-auction participation record: a row with no
-- subsequent settlementStarted notification is a lost auction.
CREATE TABLE solutions (
    auction_id  BIGINT      NOT NULL,
    solution_id BIGINT      NOT NULL,
    -- Dropped proposals take their participation rows with them (the
    -- retention sweep deletes the proposal row); settled ones are never
    -- deleted, so their rows survive indefinitely.
    proposal_id BIGINT      NOT NULL REFERENCES proposals (id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (auction_id, solution_id)
);

CREATE INDEX solutions_proposal_id_idx ON solutions (proposal_id);

-- Settlement outcome evidence (ADR-0013): the landed (settled) or reverted
-- (settleFailed) transaction hash, recorded from driver notifications.
-- 0x-prefixed lowercase hex, like every other hash column.
ALTER TABLE proposals ADD COLUMN settlement_tx_hash TEXT;
