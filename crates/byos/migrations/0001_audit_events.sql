-- Append-only audit trail (ADR-0001: in-memory hot path + async write-behind).
-- Evidence for slash disputes (ADR-0003): Track B claims arrive up to 3 months
-- post-trade, so records must outlive the hot store. No deletion path — any
-- future retention policy must keep at least that window plus dispute time.
CREATE TABLE audit_events (
    id                 BIGSERIAL PRIMARY KEY,
    proposal_id        BIGINT      NOT NULL,
    event_type         TEXT        NOT NULL,
    -- 0x-prefixed lowercase hex; TEXT so dispute queries are psql-friendly.
    sub_solver         TEXT        NOT NULL,
    order_uid          TEXT        NOT NULL,
    -- Reserved for driver-reported settlement outcomes (ADR-0010): Track B
    -- attribution starts from a cited settlement tx.
    settlement_tx_hash TEXT,
    payload            JSONB       NOT NULL,
    -- occurred_at is stamped at emission (the evidentiary time); recorded_at
    -- when the writer landed it. Their gap is the write-behind lag.
    occurred_at        TIMESTAMPTZ NOT NULL,
    recorded_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX audit_events_proposal_id_idx ON audit_events (proposal_id);
CREATE INDEX audit_events_order_uid_idx ON audit_events (order_uid);
CREATE INDEX audit_events_sub_solver_idx ON audit_events (sub_solver);
CREATE INDEX audit_events_settlement_tx_hash_idx
    ON audit_events (settlement_tx_hash)
    WHERE settlement_tx_hash IS NOT NULL;
